import asyncio
import csv
import gzip
import time
from collections import Counter

import traffic


async def exchange(reader, writer, encoding, expected, method='GET', etag=None):
    accept = 'identity' if encoding == 'identity' else encoding + ', identity;q=0'
    conditional = f'If-None-Match: {etag}\r\n' if etag else ''
    writer.write(f'{method} /small.bin HTTP/1.1\r\nHost: bench.test\r\nAccept-Encoding: {accept}\r\n{conditional}Connection: keep-alive\r\n\r\n'.encode())
    await asyncio.wait_for(writer.drain(), 5)
    raw = await asyncio.wait_for(reader.readuntil(b'\r\n\r\n'), 5)
    if len(raw) > 65536:
        raise ValueError('headers')
    lines = raw[:-4].split(b'\r\n')
    status = int(lines[0].split(b' ')[1])
    headers = {}
    for line in lines[1:]:
        key, value = line.split(b':', 1)
        headers.setdefault(key.lower(), []).append(value.strip())
    if status != 200 or headers.get(b'content-length') != [str(len(expected)).encode()] or b'transfer-encoding' in headers:
        raise ValueError('framing')
    ce = headers.get(b'content-encoding', [])
    if ce != ([] if encoding == 'identity' else [encoding.encode()]):
        raise ValueError('encoding')
    vary = b','.join(headers.get(b'vary', [])).lower().split(b',')
    if b'accept-encoding' not in [v.strip() for v in vary]:
        raise ValueError('vary')
    tags = headers.get(b'etag', [])
    if len(tags) != 1:
        raise ValueError('etag')
    if method == 'GET' and await asyncio.wait_for(reader.readexactly(len(expected)), 5) != expected:
        raise ValueError('body')
    return tags[0].decode('ascii')


async def check(fixtures):
    tags = {}
    for encoding, expected in fixtures.items():
        reader, writer, _, _ = await traffic.connect(False, None)
        try:
            tags[encoding] = await exchange(reader, writer, encoding, expected)
            if await exchange(reader, writer, encoding, expected, 'HEAD') != tags[encoding]:
                raise ValueError('head etag')
        finally:
            await traffic.close(writer)
    if len(set(tags.values())) != len(tags):
        raise ValueError('variant etags')
    for encoding, expected in fixtures.items():
        other = next(v for k, v in tags.items() if k != encoding)
        reader, writer, _, _ = await traffic.connect(False, None)
        try:
            await exchange(reader, writer, encoding, expected, etag=other)
        finally:
            await traffic.close(writer)


async def run(encoding, expected, seconds, warmup, concurrency, raw_path, window_path):
    start = time.monotonic()
    measure, deadline = start + warmup, start + warmup + seconds
    traffic.record_window(window_path, start, measure, deadline)
    latencies, errors = [], Counter()
    stats = dict(samples=0, bytes=0, within=0, within_bytes=0, late=0, reused=0, boundary=0, connections=0, last=deadline, lag=0.0)
    with gzip.open(raw_path, 'xt', newline='', compresslevel=1) as out:
        csvout = csv.writer(out)
        csvout.writerow(['start_monotonic', 'end_monotonic', 'latency_us', 'bytes', 'status', 'error', 'connection_id', 'tls_version', 'cipher', 'session_reused', 'connection_request_index', 'was_reused', 'attempts', 'connect_events', 'idle_close_reconnects', 'header_reset_retries'])
        async def worker():
            reader = writer = None
            connection_id = None
            index = 0
            try:
                while True:
                    begin = time.monotonic()
                    if begin >= deadline:
                        break
                    measured = begin >= measure
                    error, status, size, connected = '', '', 0, 0
                    conn = connection_id if connection_id is not None else ''
                    try:
                        if writer is None:
                            index = 0
                            reader, writer, connection_id, _ = await traffic.connect(False, None)
                            conn = connection_id
                            connected = 1
                        index += 1
                        await exchange(reader, writer, encoding, expected)
                        status, size = 200, len(expected)
                    except (OSError, ValueError, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError) as exc:
                        error = type(exc).__name__
                        await traffic.close(writer)
                        writer = None
                        connection_id = None
                    end = time.monotonic()
                    if begin < measure <= end:
                        stats['boundary'] += 1
                    if not measured:
                        if error:
                            raise ValueError('warmup failure')
                        continue
                    stats['samples'] += 1
                    if stats['samples'] > traffic.LIMIT:
                        raise ValueError('sample limit')
                    reused = index > 1 and not connected
                    stats['reused'] += reused
                    stats['connections'] += connected
                    us = round((end - begin) * 1000000)
                    csvout.writerow([begin, end, us, size, status, error, conn, '', '', '', index, reused, 1, connected, 0, 0])
                    stats['last'] = max(stats['last'], end)
                    stats['late'] += end > deadline
                    if error:
                        errors[error] += 1
                    else:
                        latencies.append(us)
                        stats['bytes'] += size
                        if end <= deadline:
                            stats['within'] += 1
                            stats['within_bytes'] += size
            finally:
                await traffic.close(writer)
        async def lag():
            while time.monotonic() < deadline:
                target = time.monotonic() + .1
                await asyncio.sleep(.1)
                stats['lag'] = max(stats['lag'], time.monotonic() - target)
        tasks = [asyncio.create_task(worker()) for _ in range(concurrency)] + [asyncio.create_task(lag())]
        try:
            await asyncio.gather(*tasks)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
    n, drain = stats['samples'], max(0, stats['last'] - deadline)
    ratio = stats['reused'] / n if n else 0
    return {'samples': n, 'issue_window_samples': n, 'successes': len(latencies), 'measurement_seconds': seconds,
            'completed_within_window': stats['within'], 'bytes_within_window': stats['within_bytes'],
            'completed_rps_nominal': stats['within'] / seconds, 'completed_bytes_per_second_nominal': stats['within_bytes'] / seconds,
            'cohort_rps_including_drain': len(latencies) / (seconds + drain), 'drain_seconds': drain,
            'late_completions': stats['late'], 'warmup_boundary_excluded_completions': stats['boundary'],
            'bytes': stats['bytes'], 'error_rate': sum(errors.values()) / n if n else None,
            'p50_ms': traffic.percentile(latencies, .5), 'p95_ms': traffic.percentile(latencies, .95), 'p99_ms': traffic.percentile(latencies, .99),
            'errors': dict(errors), 'measured_reused_requests': stats['reused'], 'measured_reuse_ratio': ratio,
            'connect_events': stats['connections'], 'event_loop_lag_max_seconds': stats['lag'],
            'valid': bool(latencies) and not errors and ratio >= .95, 'sample_limit': traffic.LIMIT,
            'percentile_method': 'nearest rank on successful issue cohort'}
