import argparse
import asyncio
import base64
import gzip
import hashlib
import json
import os
import platform
import resource
import signal
import socket
import sys
import time
from pathlib import Path

import metrics
import traffic
import static_ab_transfer


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(65536), b''):
            h.update(block)
    return h.hexdigest()


def cpus(value):
    parts = value.split(',')
    if not parts or any(not p.isdigit() for p in parts):
        raise argparse.ArgumentTypeError('expected comma-separated CPU numbers')
    result = {int(p) for p in parts}
    if len(result) != len(parts):
        raise argparse.ArgumentTypeError('duplicate CPU')
    return result


def arguments():
    p = argparse.ArgumentParser(description='Run under externally supplied isolation; cache hits are not measured.')
    p.add_argument('--rill', required=True, type=Path)
    p.add_argument('--workdir', required=True, type=Path)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--seconds', type=float, default=30)
    p.add_argument('--repeats', type=int, default=3)
    p.add_argument('--warmup', type=float, default=2)
    p.add_argument('--concurrency', type=int, default=8)
    p.add_argument('--server-cpus', type=cpus, default=cpus('0,1'))
    p.add_argument('--client-cpus', type=cpus, default=cpus('2,3'))
    p.add_argument('--sidecars-json', required=True, type=Path)
    p.add_argument('--only', choices=('all', 'identity', 'transfer'), default='all')
    return p.parse_args()


def fixtures(path):
    data = json.loads(path.read_text())
    result = {name: base64.b64decode(data[name + '_base64'], validate=True) for name in ('identity', 'gzip', 'br')}
    if result['identity'] != b's' * 1024 or gzip.decompress(result['gzip']) != result['identity'] or not result['br']:
        raise ValueError('fixture')
    if any(len(v) > 65536 for v in result.values()):
        raise ValueError('fixture size')
    return result


def quote(path):
    return json.dumps(str(path), ensure_ascii=True)


def port_free():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 18080))


async def ready(proc):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if proc.returncode is not None:
            raise RuntimeError('server exited')
        try:
            reader, writer = await asyncio.wait_for(asyncio.open_connection('127.0.0.1', 18080), .25)
            await traffic.close(writer)
            return
        except (OSError, asyncio.TimeoutError):
            await asyncio.sleep(.05)
    raise TimeoutError('startup')


async def stop(proc):
    forced = False
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 35
    while time.monotonic() < deadline:
        try:
            os.killpg(proc.pid, 0)
        except ProcessLookupError:
            break
        await asyncio.sleep(.1)
    else:
        forced = True
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    await proc.wait()
    return {'exit_code': proc.returncode, 'forced': forced}


async def one(args, work, output, number, kind, mode, name, content):
    prefix = f'round-{number:02d}-{kind}-{mode}-{name}'
    control = work / (prefix + '-control')
    control.mkdir(mode=0o700)
    config = work / (prefix + '.kdl')
    budget = 16777216 if mode == 'on' else 0
    precompressed = '#true' if kind == 'transfer' else '#false'
    text = f'server {{\n http "127.0.0.1:18080"\n control-socket {quote(control / "control.sock")}\n static-cache-bytes {budget}\n}}\nsite "bench.test" {{\n root {quote(work / "www")}\n precompressed {precompressed}\n}}\n'
    config.write_text(text)
    result = {'round': number, 'kind': kind, 'mode': mode, 'case': name, 'config_sha256': digest(config), 'config': str(config), 'cache_budget_bytes': budget}
    port_free()
    env = {k: os.environ[k] for k in ('PATH', 'HOME', 'TMPDIR', 'LANG', 'LC_ALL') if k in os.environ}
    env['RUST_LOG'] = 'error'
    with (work / (prefix + '.log')).open('xb') as log:
        proc = await asyncio.create_subprocess_exec(str(args.rill), 'run', '--config', str(config), cwd=work, env=env, stdout=log, stderr=log, start_new_session=True, preexec_fn=lambda: os.sched_setaffinity(0, args.server_cpus))
        sampler = None
        sampling = None
        try:
            await ready(proc)
            sampler = metrics.Sampler(output / (prefix + '-resources.csv'), roots={'server': ('pid', proc.pid), 'client': ('pid-self', os.getpid())})
            sampling = asyncio.create_task(sampler.run())
            window_path = output / (prefix + '-window.json')
            if kind == 'transfer':
                await static_ab_transfer.check(content)
                result['http'] = await static_ab_transfer.run(name, content[name], args.seconds, args.warmup, args.concurrency, output / (prefix + '-requests.csv.gz'), window_path)
            else:
                result['http'] = await traffic.round_http('/' + name + '.bin', False, True, None, args.seconds, args.warmup, args.concurrency, output / (prefix + '-requests.csv.gz'), window_path=window_path)
            if sampling.done():
                sampling.result()
            result['resources'] = sampler.peaks
            result['client_warning'] = sampler.peaks.get('client', {}).get('cpu_percent', 0) >= 90 or result['http']['event_loop_lag_max_seconds'] >= .1
        finally:
            if sampling:
                sampling.cancel()
                await asyncio.gather(sampling, return_exceptions=True)
            if sampler:
                sampler.close()
            result['stop'] = await stop(proc)
    result['valid'] = result['http']['valid'] and result['stop']['exit_code'] == 0 and not result['stop']['forced'] and not (control / 'control.sock').exists()
    return result


async def main():
    args = arguments()
    output = None
    summary = {'status': 'invalid', 'rounds': []}
    loop = asyncio.get_running_loop()
    task = asyncio.current_task()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, task.cancel)
    try:
        if not (0 < args.seconds <= 3600 and 0 <= args.warmup <= 3600 and 1 <= args.repeats <= 100 and 1 <= args.concurrency <= 128):
            raise ValueError('parameters')
        allowed = os.sched_getaffinity(0)
        if len(args.server_cpus) != 2 or len(args.client_cpus) != 2 or args.server_cpus & args.client_cpus or not (args.server_cpus | args.client_cpus) <= allowed:
            raise ValueError('affinity')
        args.rill = args.rill.resolve(strict=True)
        args.sidecars_json = args.sidecars_json.resolve(strict=True)
        content = fixtures(args.sidecars_json)
        work = args.workdir.absolute()
        output_path = args.output.absolute()
        if work == output_path or work in output_path.parents or output_path in work.parents:
            raise ValueError('directories')
        work.mkdir(mode=0o700)
        output_path.mkdir(mode=0o700)
        output = output_path
        os.sched_setaffinity(0, args.client_cpus)
        if os.sched_getaffinity(0) != args.client_cpus:
            raise ValueError('client affinity')
        www = work / 'www'
        www.mkdir()
        (www / 'small.bin').write_bytes(content['identity'])
        with (www / 'large.bin').open('xb') as f:
            for _ in range(256):
                f.write(b'l' * 65536)
        (www / 'small.bin.gz').write_bytes(content['gzip'])
        (www / 'small.bin.br').write_bytes(content['br'])
        params = vars(args).copy()
        params.update(rill=str(args.rill), workdir=str(work), output=str(output), sidecars_json=str(args.sidecars_json), server_cpus=sorted(args.server_cpus), client_cpus=sorted(args.client_cpus))
        summary.update(parameters=params, binary_sha256=digest(args.rill), fixture_sha256=digest(args.sidecars_json), kernel=platform.release(), python=sys.version, original_affinity=sorted(allowed), limits={key: list(resource.getrlimit(getattr(resource, key))) for key in ('RLIMIT_AS', 'RLIMIT_CPU', 'RLIMIT_NPROC', 'RLIMIT_NOFILE', 'RLIMIT_FSIZE')}, clock='time.monotonic', offered_load='closed-loop', cache_note='Configured budget only; no cache hit metrics. Separate functional tests establish cache behavior.', memory_note='RSS sums may double-count shared pages; HWM is historical, not round peak; PSS and cgroup metrics unavailable.', fixture_note='Driver verifies identity and gzip; brotli compared to externally verified fixture bytes, driver does not decode brotli.')
        number = 0
        for repeat in range(args.repeats):
            if args.only in ('all', 'identity'):
                for mode in (('off', 'on') if repeat % 2 == 0 else ('on', 'off')):
                    for name in ('small', 'large'):
                        number += 1
                        result = await one(args, work, output, number, 'identity', mode, name, content)
                        result['repeat'] = repeat + 1
                        summary['rounds'].append(result)
                        if not result['valid']:
                            raise ValueError('invalid round')
            if args.only in ('all', 'transfer'):
                for name in ('identity', 'gzip', 'br'):
                    number += 1
                    result = await one(args, work, output, number, 'transfer', 'off', name, content)
                    result['repeat'] = repeat + 1
                    summary['rounds'].append(result)
                    if not result['valid']:
                        raise ValueError('invalid round')
        summary['status'] = 'complete' if args.only == 'all' else 'partial'
        return 0
    except (Exception, asyncio.CancelledError) as exc:
        summary['error_type'] = type(exc).__name__
        return 1
    finally:
        if output is not None and output.is_dir():
            (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
