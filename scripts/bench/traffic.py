import asyncio
import csv
import gzip
import itertools
import math
import ssl
import time
from collections import Counter


IDS = itertools.count(1)
LIMIT = 2000000
TIMEOUT = 120


def context(ca):
    ctx = ssl.create_default_context(cafile=str(ca))
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.set_alpn_protocols(['http/1.1'])
    return ctx


async def close(writer):
    if writer is not None:
        writer.close()
        try:
            await asyncio.wait_for(writer.wait_closed(), 1)
        except (OSError, asyncio.TimeoutError):
            pass


async def connect(tls, ca, ctx=None):
    ctx = (ctx or context(ca)) if tls else None
    reader, writer = await asyncio.wait_for(asyncio.open_connection(
        '127.0.0.1', 18443 if tls else 18080, ssl=ctx,
        server_hostname='bench.test' if tls else None, limit=65536), 5)
    obj = writer.get_extra_info('ssl_object')
    details = (obj.version(), obj.cipher()[0], obj.session_reused) if obj else ('', '', '')
    if obj and obj.session_reused:
        await close(writer)
        raise ValueError('unexpected TLS session reuse')
    return reader, writer, next(IDS), details


class EmptyResponse(Exception):
    pass


class HeaderReset(Exception):
    pass


async def exchange(reader, writer, path, warm):
    expected = {'/proxy': (1024, b'p'), '/small.bin': (1024, b's'), '/large.bin': (16777216, b'l')}[path]
    try:
        writer.write(f'GET {path} HTTP/1.1\r\nHost: bench.test\r\nConnection: {"keep-alive" if warm else "close"}\r\n\r\n'.encode())
        await asyncio.wait_for(writer.drain(), 5)
        raw = await asyncio.wait_for(reader.readuntil(b'\r\n\r\n'), 5)
    except ConnectionResetError as exc:
        raise HeaderReset from exc
    except asyncio.IncompleteReadError as exc:
        if not exc.partial:
            raise EmptyResponse from exc
        raise
    if len(raw) > 65536:
        raise ValueError('header limit')
    lines = raw[:-4].split(b'\r\n')
    parts = lines[0].split(b' ', 2)
    if len(parts) < 2 or parts[0] != b'HTTP/1.1':
        raise ValueError('invalid status line')
    status = int(parts[1])
    headers = {}
    for line in lines[1:]:
        name, value = line.split(b':', 1)
        headers.setdefault(name.strip().lower(), []).append(value.strip())
    if status != 200 or headers.get(b'content-length') != [str(expected[0]).encode()] or b'transfer-encoding' in headers:
        raise ValueError(f'unexpected status or framing: {status}')
    remaining = expected[0]
    while remaining:
        data = await asyncio.wait_for(reader.readexactly(min(remaining, 65536)), TIMEOUT)
        if data != expected[1] * len(data):
            raise ValueError('incorrect payload')
        remaining -= len(data)
    server_close = b'close' in b','.join(headers.get(b'connection', [])).lower().split(b',')
    return status, expected[0], server_close


def percentile(values, fraction):
    if not values:
        return None
    return sorted(values)[math.ceil(len(values) * fraction) - 1] / 1000


async def round_http(path, tls, warm, ca, seconds, warmup, concurrency, raw_path):
    errors = Counter()
    latencies = []
    successful_bytes = attempted = reused_count = connect_events = reconnects = idle_reconnects = 0
    header_reset_retries = physical_attempts = 0
    within = within_bytes = late = boundary = 0
    lag_max = 0.0
    start = time.monotonic()
    measure = start + warmup
    deadline = measure + seconds
    last_completion = deadline
    with gzip.open(raw_path, 'xt', newline='', compresslevel=1) as out:
        csvout = csv.writer(out)
        csvout.writerow(['start_monotonic', 'end_monotonic', 'latency_us', 'bytes', 'status', 'error', 'connection_id', 'tls_version', 'cipher', 'session_reused', 'connection_request_index', 'was_reused', 'attempts', 'connect_events', 'idle_close_reconnects', 'header_reset_retries'])

        async def worker():
            nonlocal successful_bytes, attempted, reused_count, connect_events, reconnects, idle_reconnects
            nonlocal within, within_bytes, late, boundary, last_completion, header_reset_retries, physical_attempts
            reader = writer = None
            connection_id = ''
            details = ('', '', '')
            index = 0
            connections = 0
            ctx = context(ca) if tls else None
            try:
                while time.monotonic() < deadline:
                    begin = time.monotonic()
                    measured = begin >= measure
                    status, size, error = '', 0, ''
                    attempt_count = connects = idle_closes = reset_retries = 0
                    reused = False
                    request_index = 0
                    try:
                        if warm and writer is not None and reader.at_eof():
                            await close(writer)
                            reader = writer = None
                            idle_closes += 1
                        while True:
                            if writer is None:
                                reader, writer, connection_id, details = await connect(tls, ca, ctx)
                                connections += 1
                                connects += 1
                                index = 0
                            index += 1
                            request_index = index
                            attempt_count += 1
                            reused = index > 1
                            try:
                                status, size, server_close = await exchange(reader, writer, path, warm)
                                break
                            except (EmptyResponse, HeaderReset) as exc:
                                if not warm or not reused or attempt_count != 1:
                                    raise
                                reset_retries += isinstance(exc, HeaderReset)
                                idle_closes += 1
                                await close(writer)
                                reader = writer = None
                        if not warm or server_close:
                            await close(writer)
                            reader = writer = None
                    except (OSError, ValueError, EmptyResponse, HeaderReset, asyncio.TimeoutError, asyncio.IncompleteReadError, asyncio.LimitOverrunError, ssl.SSLError) as exc:
                        error = type(exc).__name__
                        await close(writer)
                        reader = writer = None
                    end = time.monotonic()
                    if begin < measure <= end:
                        boundary += 1
                    if measured:
                        attempted += 1
                        if attempted > LIMIT:
                            raise RuntimeError('sample limit exceeded')
                        connect_events += connects
                        reconnects += connects if connections > connects else max(0, connects - 1)
                        idle_reconnects += idle_closes
                        header_reset_retries += reset_retries
                        physical_attempts += attempt_count
                        reused_count += reused
                        us = round((end - begin) * 1000000)
                        csvout.writerow([begin, end, us, size, status, error, connection_id, *details, request_index, reused, attempt_count, connects, idle_closes, reset_retries])
                        last_completion = max(last_completion, end)
                        if end > deadline:
                            late += 1
                        if error:
                            errors[error] += 1
                        else:
                            latencies.append(us)
                            successful_bytes += size
                            if end <= deadline:
                                within += 1
                                within_bytes += size
            finally:
                await close(writer)

        async def lag():
            nonlocal lag_max
            while time.monotonic() < deadline:
                target = time.monotonic() + .1
                await asyncio.sleep(.1)
                lag_max = max(lag_max, time.monotonic() - target)

        tasks = [asyncio.create_task(worker()) for _ in range(concurrency)]
        lag_task = asyncio.create_task(lag())
        try:
            await asyncio.gather(*tasks, lag_task)
        finally:
            for task in tasks + [lag_task]:
                task.cancel()
            await asyncio.gather(*tasks, lag_task, return_exceptions=True)
    drain = max(0, last_completion - deadline)
    successes = len(latencies)
    reuse_ratio = reused_count / attempted if attempted else 0
    return {'samples': attempted, 'issue_window_samples': attempted, 'successes': successes,
            'measurement_seconds': seconds, 'completed_within_window': within, 'bytes_within_window': within_bytes,
            'completed_rps_nominal': within / seconds, 'completed_bytes_per_second_nominal': within_bytes / seconds,
            'cohort_rps_including_drain': successes / (seconds + drain), 'drain_seconds': drain,
            'late_completions': late, 'warmup_boundary_excluded_completions': boundary,
            'bytes': successful_bytes, 'error_rate': sum(errors.values()) / attempted if attempted else None,
            'p50_ms': percentile(latencies, .50), 'p95_ms': percentile(latencies, .95),
            'p99_ms': percentile(latencies, .99), 'errors': dict(errors),
            'measured_reused_requests': reused_count, 'measured_reuse_ratio': reuse_ratio,
            'connect_events': connect_events, 'reconnects': reconnects, 'idle_close_reconnects': idle_reconnects,
            'header_reset_retries': header_reset_retries, 'physical_attempts': physical_attempts,
            'retry_policy': 'GET only; at most one fresh connection retry after empty headers or header-phase reset on an existing warm connection; never retry body failures',
            'event_loop_lag_max_seconds': lag_max, 'valid': bool(successes) and not errors and (reuse_ratio >= .95 if warm else reused_count == 0),
            'sample_limit': LIMIT, 'percentile_method': 'nearest rank on successful issue cohort'}


async def correctness(ca):
    for tls in (False, True):
        for path in ('/proxy', '/small.bin', '/large.bin'):
            reader, writer, _, _ = await connect(tls, ca)
            try:
                await exchange(reader, writer, path, True)
                await exchange(reader, writer, path, False)
            finally:
                await close(writer)
