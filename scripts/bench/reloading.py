import asyncio
from array import array
import math
import csv
import gzip
import hashlib
import json
import os
import re
import signal
import tempfile
import time
from collections import Counter

import container
import streams
import traffic


def atomic_config(source, target):
    data = source.read_bytes()
    fd, path = tempfile.mkstemp(prefix='.reload-', dir=target.parent)
    try:
        with os.fdopen(fd, 'wb') as out:
            out.write(data)
            out.flush()
            os.fsync(out.fileno())
        # The container runs as a non-root UID and must be able to read the
        # replacement, so the temporary file cannot keep mkstemp's 0600.
        os.chmod(path, 0o644)
        if os.geteuid() == 0:
            os.chown(path, 10001, 10001)
        os.replace(path, target)
    finally:
        if os.path.exists(path):
            os.unlink(path)
    return hashlib.sha256(data).hexdigest()


async def command(args, timeout):
    proc = await asyncio.create_subprocess_exec(*args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout)
        return proc.returncode, stdout, stderr
    except BaseException:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
        raise


async def get_small(reader, writer, keepalive):
    await streams.send(writer, b'GET /small.bin HTTP/1.1\r\nHost: bench.test\r\nConnection: ' + (b'keep-alive' if keepalive else b'close') + b'\r\n\r\n')
    status, headers = await streams.headers(reader)
    if status != b'200' or headers.get(b'content-length') != [b'1024'] or b'transfer-encoding' in headers:
        raise ValueError('framing')
    if await asyncio.wait_for(reader.readexactly(1024), 5) != b's' * 1024:
        raise ValueError('body')
    ages = headers.get(b'cache-control', [])
    if len(ages) != 1 or ages[0] not in (b'max-age=10', b'max-age=20'):
        raise ValueError('cache_control')
    return int(ages[0][8:])


async def observe(age, ca):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        writer = None
        try:
            reader, writer, _, _ = await asyncio.wait_for(traffic.connect(False, ca), max(.001, deadline - time.monotonic()))
            actual = await asyncio.wait_for(get_small(reader, writer, False), max(.001, deadline - time.monotonic()))
            if actual == age:
                return time.monotonic()
        except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, ValueError):
            pass
        finally:
            await traffic.close(writer)
        if time.monotonic() < deadline:
            await asyncio.sleep(min(.05, deadline - time.monotonic()))
    raise TimeoutError('observation_timeout')


async def apply(args, target, work, server, age, ca):
    start = time.monotonic()
    config = target.active(work)
    template = target.template(work, 'a' if age == 10 else 'b')
    result = {'start': start, 'sent': None, 'observed': None, 'end': None, 'target_age': age,
              'returncode': None, 'generation': None, 'error': None,
              'check_seconds': None, 'signal_seconds': None, 'observation_seconds': None,
              'config_sha256': None, 'mechanism': target.name}
    try:
        if not container.running(server):
            raise RuntimeError('server_exited')
        os.chmod(work, 0o711)
        result['config_sha256'] = atomic_config(template, config)
        begin = time.monotonic()
        rc, stdout, stderr = await target.reload(work, config, server)
        result['returncode'] = rc
        result['signal_seconds'] = time.monotonic() - begin
        if rc != 0:
            result['error_output'] = (stdout + stderr).decode('utf-8', 'replace')[:800]
            raise RuntimeError('reload_failed')
        result['sent'] = begin
        match = re.search(rb'(?:^|\s)generation=([0-9]+)(?:\s|$)', stdout)
        if match:
            result['generation'] = int(match.group(1))
        begin = time.monotonic()
        result['observed'] = await observe(age, ca)
        result['observation_seconds'] = result['observed'] - begin
    except (OSError, ValueError, RuntimeError, asyncio.TimeoutError, TimeoutError, container.ContainerError) as exc:
        result['error'] = type(exc).__name__
    result['end'] = time.monotonic()
    result['latency_seconds'] = result['end'] - start
    return result


def distribution(values):
    micros = [round(v * 1000000) for v in values]
    return {'samples': len(values), 'p50_ms': traffic.percentile(micros, .5),
            'p95_ms': traffic.percentile(micros, .95), 'p99_ms': traffic.percentile(micros, .99),
            'max_ms': max(values) * 1000 if values else None}


def distribution_us(values):
    ordered = sorted(values)
    n = len(ordered)
    def quantile(fraction):
        return ordered[math.ceil(n * fraction) - 1] / 1000 if n else None
    return {'samples': n, 'p50_ms': quantile(.5), 'p95_ms': quantile(.95),
            'p99_ms': quantile(.99), 'max_ms': ordered[-1] / 1000 if n else None}


def windows(path, calls):
    items = [dict(start=c['start'], end=(c['observed'] or c['end']) + 1, durations=array('Q'), errors=0) for c in calls]
    union, all_values = array('Q'), array('Q')
    union_errors = total = total_errors = stored = 0
    aggregate_limit = traffic.LIMIT * (len(calls) + 2)
    with gzip.open(path, 'rt', newline='') as source:
        for row in csv.DictReader(source):
            total += 1
            if total > traffic.LIMIT:
                raise RuntimeError('raw_sample_limit')
            failed = bool(row['error'])
            total_errors += failed
            timestamp = float(row['start_monotonic'])
            matches = [w for w in items if w['start'] <= timestamp <= w['end']]
            for window in matches:
                window['errors'] += failed
            if not failed:
                value = int(row['latency_us'])
                stored += 1 + len(matches) + bool(matches)
                if stored > aggregate_limit:
                    raise RuntimeError('aggregate_value_limit')
                all_values.append(value)
                for window in matches:
                    window['durations'].append(value)
                if matches:
                    union.append(value)
            elif matches:
                union_errors += 1
    return {'total_samples': total, 'total_errors': total_errors,
            'aggregate_value_limit': aggregate_limit,
            'whole_phase': distribution_us(all_values),
            'union': {**distribution_us(union), 'errors': union_errors},
            'per_reload': [{'start': w['start'], 'end': w['end'], 'errors': w['errors'],
                            'invalid_reason': 'empty_window' if not w['durations'] else ('window_errors' if w['errors'] else None),
                            **distribution_us(w['durations'])} for w in items]}


async def probe(ca, stop):
    writer = None
    count = 0
    reconnects = 0
    errors = Counter()
    try:
        while not stop.is_set():
            try:
                if writer is None:
                    reader, writer, _, _ = await traffic.connect(False, ca)
                await get_small(reader, writer, True)
                count += 1
            except asyncio.IncompleteReadError as exc:
                if not exc.partial:
                    reconnects += 1
                else:
                    errors[type(exc).__name__] += 1
                await traffic.close(writer)
                writer = None
            except (OSError, ValueError, asyncio.TimeoutError) as exc:
                errors[type(exc).__name__] += 1
                await traffic.close(writer)
                writer = None
            try:
                await asyncio.wait_for(stop.wait(), .2)
            except asyncio.TimeoutError:
                pass
    finally:
        await traffic.close(writer)
    return {'samples': count, 'closed_reconnects': reconnects, 'errors': dict(errors), 'valid': count > 0 and not errors}


async def run(args, target, work, output, label, server, ca, current_age):
    setup = None
    name = target.name
    if current_age != 10:
        setup = await apply(args, target, work, server, 10, ca)
        (output / f'{label}-setup.json').write_text(json.dumps(setup, indent=2))
        if setup['error']:
            return {'valid': False, 'setup': setup, 'error': 'setup_failed'}, current_age
    else:
        (output / f'{label}-setup.json').write_text(json.dumps({'skipped': True, 'age': 10}, indent=2))
    loop = asyncio.get_running_loop()
    gate = asyncio.Event()
    stop = asyncio.Event()
    ready_ws, ready_sse = loop.create_future(), loop.create_future()
    ws = asyncio.create_task(streams.run_ws(ca, args.reload_seconds + 2, 10, output / f'{label}-ws.csv.gz', stop_event=stop, ready_future=ready_ws, start_gate=gate))
    sse = asyncio.create_task(streams.run_sse(ca, args.reload_seconds + 2, 10, output / f'{label}-sse.csv.gz', stop_event=stop, ready_future=ready_sse, start_gate=gate))
    tasks = [ws, sse]
    calls = []
    start_wall = time.monotonic()
    try:
        ready_counts = await asyncio.wait_for(asyncio.gather(ready_ws, ready_sse), 15)
        if ready_counts != [10, 10]:
            raise RuntimeError('streams_not_ready')
        start = time.monotonic()
        deadline = start + 2 + args.reload_seconds
        load = asyncio.create_task(traffic.round_http('/small.bin', False, True, ca, args.reload_seconds, 2, 8, output / f'{label}-requests.csv.gz'))
        gate.set()
        cache_probe = asyncio.create_task(probe(ca, stop))
        tasks += [load, cache_probe]
        with (output / f'{label}-reloads.csv').open('x', newline='') as raw:
            fields = ['start', 'sent', 'observed', 'end', 'latency_seconds', 'target_age', 'returncode',
                      'generation', 'check_seconds', 'signal_seconds', 'observation_seconds',
                      'config_sha256', 'error', 'mechanism', 'error_output']
            rows = csv.DictWriter(raw, fieldnames=fields)
            rows.writeheader()
            for i in range(args.reload_count):
                target_time = start + 2 + args.reload_interval * (i + 1)
                await asyncio.sleep(max(0, target_time - time.monotonic()))
                if time.monotonic() >= deadline:
                    break
                age = 20 if i % 2 == 0 else 10
                result = await apply(args, target, work, server, age, ca)
                calls.append(result)
                rows.writerow({key: result.get(key) for key in fields})
                raw.flush()
                if result['error']:
                    break
                current_age = age
        await asyncio.sleep(max(0, deadline - time.monotonic()))
        stop.set()
        ws_result, sse_result, probe_result, load_result = await asyncio.gather(ws, sse, cache_probe, load)
        first = calls[0]['start'] if calls else 0
        intervals = windows(output / f'{label}-requests.csv.gz', calls)
        # A stream disconnect is reported separately: retiring a configuration may
        # legitimately close WebSockets, which is not a logical reload failure.
        streams_ok = ws_result['connections_established'] == 10 and sse_result['connections_established'] == 10
        valid = (len(calls) == args.reload_count and all(not item['error'] for item in calls)
                 and streams_ok and probe_result['valid'] and load_result['valid']
                 and ws_result['measurement_start_monotonic'] < first and sse_result['measurement_start_monotonic'] < first
                 and all(item['end'] <= deadline for item in calls)
                 and all(w['samples'] > 0 and w['errors'] == 0 for w in intervals['per_reload'])
                 and container.running(server))
        summary = {'valid': valid, 'setup': setup, 'calls': calls, 'phase_start': start + 2,
                   'phase_deadline': deadline, 'wall_seconds': time.monotonic() - start_wall,
                   'http_load': load_result, 'ws': ws_result, 'sse': sse_result, 'warm_cache_probe': probe_result,
                   'reload_latency': distribution([c['latency_seconds'] for c in calls]), 'http_windows': intervals,
                   'window_definition': 'request start in [reload start, observed or failure end + 1s]',
                   'reload_mechanism': target.name,
                   'stream_disconnects': {
                       'ws': ws_result['unexpected_disconnects'],
                       'sse': sse_result['unexpected_disconnects'],
                       'policy': 'reported separately from logical errors; a disconnect does not invalidate the round',
                   },
                   'error': None if valid else 'invalid_reload_round'}
        return summary, current_age
    finally:
        stop.set()
        gate.set()
        for task in tasks:
            if not task.done():
                task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
