import asyncio
import base64
import csv
import gzip
import hashlib
import os
import struct
import time
from collections import Counter

import traffic


LIMIT = 65536
MAX_SAMPLES = 2000000
GUID = b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


async def headers(reader):
    raw = await asyncio.wait_for(reader.readuntil(b'\r\n\r\n'), 5)
    if len(raw) > LIMIT:
        raise ValueError('headers')
    lines = raw[:-4].split(b'\r\n')
    status = lines.pop(0).split(b' ', 2)
    if len(status) < 2 or status[0] != b'HTTP/1.1':
        raise ValueError('status')
    result = {}
    for line in lines:
        name, value = line.split(b':', 1)
        result.setdefault(name.lower(), []).append(value.strip())
    return status[1], result


def tokens(headers, name):
    return {v.strip().lower() for line in headers.get(name, []) for v in line.split(b',')}


async def send(writer, data):
    writer.write(data)
    await asyncio.wait_for(writer.drain(), 5)


async def ws_open(reader, writer):
    key = base64.b64encode(os.urandom(16))
    await send(writer, b'GET /ws HTTP/1.1\r\nHost: bench.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ' + key + b'\r\n\r\n')
    status, fields = await headers(reader)
    accept = base64.b64encode(hashlib.sha1(key + GUID).digest())
    if (status != b'101' or b'upgrade' not in tokens(fields, b'connection')
            or tokens(fields, b'upgrade') != {b'websocket'}
            or fields.get(b'sec-websocket-accept') != [accept]
            or b'sec-websocket-extensions' in fields or b'sec-websocket-protocol' in fields):
        raise ValueError('ws_handshake')


async def ws_send(writer, opcode, payload):
    n = len(payload)
    if n > LIMIT or opcode >= 8 and n > 125:
        raise ValueError('ws_length')
    length = bytes([n]) if n < 126 else (b'\x7e' + struct.pack('!H', n) if n < 65536 else b'\x7f' + struct.pack('!Q', n))
    mask = os.urandom(4)
    await send(writer, bytes([0x80 | opcode, length[0] | 0x80]) + length[1:] + mask + bytes(v ^ mask[i % 4] for i, v in enumerate(payload)))


async def ws_read(reader):
    first, second = await reader.readexactly(2)
    opcode = first & 15
    if first & 0x70 or not first & 0x80 or second & 0x80 or opcode not in (1, 2, 8, 9, 10):
        raise ValueError('ws_frame')
    n = second & 127
    if n == 126:
        n = struct.unpack('!H', await reader.readexactly(2))[0]
        if n < 126:
            raise ValueError('ws_length')
    elif n == 127:
        n = struct.unpack('!Q', await reader.readexactly(8))[0]
        if n < 65536 or n >> 63:
            raise ValueError('ws_length')
    if n > LIMIT or opcode >= 8 and n > 125:
        raise ValueError('ws_length')
    payload = await reader.readexactly(n)
    if opcode == 1:
        payload.decode('utf-8')
    if opcode == 8:
        if len(payload) == 1:
            raise ValueError('ws_close')
        if payload:
            code = struct.unpack('!H', payload[:2])[0]
            if not (code in (1000, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011) or 3000 <= code <= 4999):
                raise ValueError('ws_close_code')
            payload[2:].decode('utf-8')
    return opcode, payload


async def sse_open(reader, writer):
    await send(writer, b'GET /events HTTP/1.1\r\nHost: bench.test\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n')
    status, fields = await headers(reader)
    ctype = fields.get(b'content-type', [])
    if (status != b'200' or len(ctype) != 1 or ctype[0].split(b';', 1)[0].strip().lower() != b'text/event-stream'
            or tokens(fields, b'transfer-encoding') != {b'chunked'} or b'content-length' in fields):
        raise ValueError('sse_handshake')


async def chunk(reader):
    line = await reader.readuntil(b'\r\n')
    if len(line) > LIMIT:
        raise ValueError('chunk_header')
    size_text = line[:-2].split(b';', 1)[0]
    if not size_text or any(c not in b'0123456789abcdefABCDEF' for c in size_text):
        raise ValueError('chunk_size')
    n = int(size_text, 16)
    if n == 0 or n > 1048576:
        raise ValueError('chunk_end_or_limit')
    data = await reader.readexactly(n)
    if await reader.readexactly(2) != b'\r\n':
        raise ValueError('chunk_framing')
    return data


async def until(awaitable, deadline, stop):
    task = asyncio.create_task(awaitable)
    stopper = asyncio.create_task(stop.wait()) if stop is not None else None
    try:
        tasks = {task} | ({stopper} if stopper else set())
        done, _ = await asyncio.wait(tasks, timeout=max(0, deadline - time.monotonic()), return_when=asyncio.FIRST_COMPLETED)
        if task in done:
            return True, task.result()
        return False, None
    finally:
        for t in (task, stopper):
            if t is not None and not t.done():
                t.cancel()
        await asyncio.gather(*(t for t in (task, stopper) if t is not None), return_exceptions=True)


def stats(values):
    return {'samples': len(values), 'p50_ms': traffic.percentile(values, .5),
            'p95_ms': traffic.percentile(values, .95), 'p99_ms': traffic.percentile(values, .99)}


async def run(kind, ca, seconds, count, raw_path, stop_event=None, tls=False, ready_future=None, start_gate=None):
    timings, firsts, intervals, handshakes, closes = [], [], [], [], []
    errors = Counter()
    per_connection = {}
    progress_by_connection = {}
    connections = []
    disconnects = 0
    expected_closes = 0
    lag_max = 0.0
    with gzip.open(raw_path, 'xt', newline='', compresslevel=1) as output:
        rows = csv.writer(output)
        rows.writerow(['kind', 'connection_id', 'start_monotonic', 'end_monotonic', 'duration_us', 'bytes', 'sequence', 'error', 'close_type'])

        async def establish(index):
            begin = time.monotonic()
            writer = None
            success = False
            identifier = f'failed-{index}'
            try:
                reader, writer, identifier, _ = await traffic.connect(tls, ca)
                await (ws_open(reader, writer) if kind == 'ws' else sse_open(reader, writer))
                end = time.monotonic()
                handshakes.append(round((end - begin) * 1000000))
                rows.writerow(['handshake', identifier, begin, end, handshakes[-1], 0, '', '', ''])
                success = True
                return reader, writer, identifier, end
            except Exception as exc:
                errors['handshake_' + type(exc).__name__] += 1
                rows.writerow(['handshake', identifier, begin, time.monotonic(), '', 0, '', type(exc).__name__, 'unexpected'])
                return None
            finally:
                if not success:
                    await traffic.close(writer)

        connections = await asyncio.gather(*(establish(i) for i in range(count)))
        live = [item for item in connections if item is not None]
        if ready_future is not None and not ready_future.done():
            ready_future.set_result(len(live))
        if start_gate is not None:
            try:
                await start_gate.wait()
            except BaseException:
                await asyncio.gather(*(traffic.close(item[1]) for item in live), return_exceptions=True)
                raise
        start = time.monotonic()
        deadline = start + seconds

        async def worker(item):
            nonlocal disconnects, expected_closes
            reader, writer, identifier, established = item
            seq = 0
            previous = None
            buffer = b''
            normal = False
            last_progress = start
            max_gap = 0.0
            try:
                while time.monotonic() < deadline and not (stop_event and stop_event.is_set()):
                    if kind == 'ws':
                        payload = struct.pack('!QQ', identifier, seq + 1)
                        begin = time.monotonic()
                        await asyncio.wait_for(ws_send(writer, 2, payload), 3)
                        while True:
                            completed, frame = await until(ws_read(reader), min(deadline, begin + 3), stop_event)
                            if not completed:
                                if time.monotonic() >= deadline or stop_event and stop_event.is_set():
                                    normal = True
                                    return
                                raise TimeoutError('ws_progress')
                            opcode, data = frame
                            if opcode == 9:
                                await asyncio.wait_for(ws_send(writer, 10, data), max(.001, begin + 3 - time.monotonic()))
                            elif opcode == 10:
                                continue
                            elif opcode == 2 and data == payload:
                                break
                            else:
                                raise ValueError('ws_echo_or_disconnect')
                        end = time.monotonic()
                        if end - begin > 3 and begin + 3 < deadline:
                            raise TimeoutError('ws_progress')
                        seq += 1
                        max_gap = max(max_gap, end - last_progress)
                        last_progress = end
                        timings.append(round((end - begin) * 1000000))
                        rows.writerow(['message', identifier, begin, end, timings[-1], len(data), seq, '', ''])
                        await until(asyncio.sleep(max(0, begin + .1 - time.monotonic())), deadline, stop_event)
                    else:
                        completed, data = await until(chunk(reader), min(deadline, last_progress + 3), stop_event)
                        if not completed:
                            if time.monotonic() >= deadline or stop_event and stop_event.is_set():
                                normal = True
                                return
                            raise TimeoutError('sse_progress')
                        buffer += data
                        while b'\n\n' in buffer:
                            event, buffer = buffer.split(b'\n\n', 1)
                            if len(event) > LIMIT or any(len(line) > LIMIT for line in event.split(b'\n')):
                                raise ValueError('sse_limit')
                            lines = event.split(b'\n')
                            if len(lines) != 1 or not lines[0].startswith(b'data: '):
                                raise ValueError('sse_event')
                            number = lines[0][6:]
                            if not number.isdigit() or int(number) != seq + 1:
                                raise ValueError('sse_sequence')
                            seq += 1
                            now = time.monotonic()
                            if now - last_progress > 3 and last_progress + 3 < deadline:
                                raise TimeoutError('sse_progress')
                            max_gap = max(max_gap, now - last_progress)
                            last_progress = now
                            value = round((now - (previous if previous is not None else established)) * 1000000)
                            (intervals if previous is not None else firsts).append(value)
                            rows.writerow(['interval' if previous is not None else 'first_event', identifier, previous if previous is not None else established, now, value, len(event), seq, '', ''])
                            previous = now
                        if len(buffer) > LIMIT:
                            raise ValueError('sse_buffer')
                    if len(timings) + len(firsts) + len(intervals) > MAX_SAMPLES:
                        raise ValueError('sample_limit')
                normal = True
            except Exception as exc:
                errors[type(exc).__name__] += 1
                disconnects += 1
                rows.writerow(['error', identifier, time.monotonic(), '', '', 0, seq, type(exc).__name__, 'unexpected'])
            finally:
                per_connection[str(identifier)] = seq
                progress_end = min(time.monotonic(), deadline)
                tail = max(0, progress_end - last_progress)
                max_gap = max(max_gap, tail)
                progress_by_connection[str(identifier)] = {'last_progress_age_at_end': tail, 'max_inactive_gap_seconds': max_gap}
                if normal and tail >= 3:
                    normal = False
                    disconnects += 1
                    errors['tail_progress'] += 1
                if normal:
                    expected_closes += 1
                    begin = time.monotonic()
                    close_error = ''
                    if kind == 'ws':
                        try:
                            await ws_send(writer, 8, struct.pack('!H', 1000))
                            async def close_reply():
                                while True:
                                    opcode, payload = await ws_read(reader)
                                    if opcode == 8:
                                        return
                                    if opcode == 9:
                                        await ws_send(writer, 10, payload)
                            await asyncio.wait_for(close_reply(), 2)
                        except Exception as exc:
                            close_error = type(exc).__name__
                            errors['close_' + close_error] += 1
                    end = time.monotonic()
                    closes.append(round((end - begin) * 1000000))
                    rows.writerow(['close', identifier, begin, end, closes[-1], 0, seq, close_error, 'expected'])
                rows.writerow(['duration', identifier, start, time.monotonic(), round((time.monotonic() - start) * 1000000), 0, seq, '', 'expected' if normal else 'unexpected'])
                await traffic.close(writer)

        async def lag():
            nonlocal lag_max
            while time.monotonic() < deadline and not (stop_event and stop_event.is_set()):
                target = time.monotonic() + .1
                await asyncio.sleep(.1)
                lag_max = max(lag_max, time.monotonic() - target)

        lag_task = asyncio.create_task(lag())
        try:
            await asyncio.gather(*(worker(item) for item in live))
        finally:
            lag_task.cancel()
            await asyncio.gather(lag_task, return_exceptions=True)
            await asyncio.gather(*(traffic.close(item[1]) for item in live), return_exceptions=True)
    valid = len(live) == count and not errors and all(per_connection.get(str(item[2]), 0) > 0 for item in live)
    return {'kind': kind, 'connections_requested': count, 'connections_established': len(live),
            'measurement_start_monotonic': start, 'measurement_deadline_monotonic': deadline,
            'samples': len(timings) if kind == 'ws' else len(firsts) + len(intervals),
            'message_rtt': stats(timings), 'first_event': stats(firsts), 'event_interval': stats(intervals),
            'handshake': stats(handshakes), 'close_handshake': stats(closes),
            'expected_closes': expected_closes, 'unexpected_disconnects': disconnects,
            'events_per_connection': per_connection, 'progress_by_connection': progress_by_connection, 'errors': dict(errors), 'valid': valid,
            'event_loop_lag_max_seconds': lag_max,
            'percentile_method': 'nearest rank; milliseconds', 'sample_limit': MAX_SAMPLES}


async def run_ws(ca, seconds, count, raw_path, stop_event=None, tls=False, ready_future=None, start_gate=None):
    return await run('ws', ca, seconds, count, raw_path, stop_event, tls, ready_future, start_gate)


async def run_sse(ca, seconds, count, raw_path, stop_event=None, tls=False, ready_future=None, start_gate=None):
    return await run('sse', ca, seconds, count, raw_path, stop_event, tls, ready_future, start_gate)
