import argparse
import asyncio
import csv
import json
import os
from pathlib import Path
import platform
import resource
import signal
import socket
import subprocess
import sys
import time

import fixtures
import metrics
import traffic
import streams
import reloading


def cpus(value):
    try:
        values = {int(part) for part in value.split(',')}
    except ValueError as exc:
        raise argparse.ArgumentTypeError('expected two comma-separated CPU numbers') from exc
    if len(values) != 2 or not values <= os.sched_getaffinity(0):
        raise argparse.ArgumentTypeError('expected two distinct available CPUs')
    return values


def positive(value):
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError('must be positive')
    return result


def launch(command, affinity, logfile):
    def setup():
        os.sched_setaffinity(0, affinity)
    return subprocess.Popen(command, stdout=logfile, stderr=subprocess.STDOUT,
                            start_new_session=True, preexec_fn=setup)


def group_has_live_members(pgid):
    for path in Path('/proc').iterdir():
        if not path.name.isdigit():
            continue
        try:
            text = (path / 'stat').read_text()
            fields = text[text.rfind(')') + 2:].split()
            if int(fields[2]) == pgid and fields[0] != 'Z':
                return True
        except (OSError, ValueError, IndexError):
            continue
    return False


def stop(process, name, records):
    if process is None:
        return
    pgid = process.pid
    process.poll()
    forced = False
    if group_has_live_members(pgid):
        try:
            os.killpg(pgid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 35
    while group_has_live_members(pgid) and time.monotonic() < deadline:
        process.poll()
        time.sleep(.05)
    if group_has_live_members(pgid):
        forced = True
        try:
            os.killpg(pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 2
        while group_has_live_members(pgid) and time.monotonic() < deadline:
            process.poll()
            time.sleep(.05)
    process.poll()
    remaining = group_has_live_members(pgid)
    records.append({'name': name, 'forced_kill': forced, 'remaining_live': remaining, 'returncode': process.returncode})


def check_ports():
    sockets = []
    try:
        for port in (18080, 18443, 19090):
            sock = socket.socket()
            sockets.append(sock)
            sock.bind(('127.0.0.1', port))
    finally:
        for sock in sockets:
            sock.close()


async def backend_ready(process):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError('backend exited before readiness')
        try:
            reader, writer = await asyncio.open_connection('127.0.0.1', 19090)
            await traffic.close(writer)
            return
        except OSError:
            await asyncio.sleep(.1)
    raise RuntimeError('backend readiness timed out')


async def ready(process, ca):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError('server exited before readiness')
        try:
            await traffic.correctness(ca)
            return
        except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError):
            await asyncio.sleep(.1)
    raise RuntimeError('server readiness timed out')


async def execute(args, work, output):
    cert, key, ca = (Path(args.cert).resolve(), Path(args.key).resolve(), Path(args.ca).resolve())
    hashes = fixtures.prepare(work, cert, key)
    environment = {'status': 'running', 'missing': ['http', 'streams', 'idle', 'reload'], 'selected_groups': args.only,
                   'python': sys.version, 'platform': platform.platform(),
                   'cpu_model': next((line.strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')), 'unknown'),
                   'cpu_affinity': {'server': sorted(args.server_cpus), 'client': sorted(args.client_cpus), 'backend': sorted(args.backend_cpus)},
                   'limits': {name: resource.getrlimit(getattr(resource, name)) for name in ('RLIMIT_NOFILE', 'RLIMIT_NPROC', 'RLIMIT_AS')},
                   'arguments': {k: sorted(v) if isinstance(v, set) else v for k, v in vars(args).items()},
                   'fixture_sha256': hashes, 'rill_sha256': fixtures.hash_file(args.rill),
                   'nginx_sha256': fixtures.hash_file(args.nginx), 'ca_sha256': fixtures.hash_file(ca),
                   'cert_sha256': fixtures.hash_file(cert), 'tls': 'CA and hostname verified; TLS >= 1.2; session cache and tickets disabled',
                   'nginx_sendfile': True, 'nginx_open_file_cache': False,
                   'config_sha256': {p.name: fixtures.hash_file(p) for p in work.glob('*.kdl')} | {p.name: fixtures.hash_file(p) for p in work.glob('*.conf')},
                   'clock': 'monotonic', 'load': 'closed-loop HTTP/1.1; not saturation capacity'}
    (output / 'environment.json').write_text(json.dumps(environment, indent=2))
    for name, command in [('rill', [args.rill, '--version']), ('nginx', [args.nginx_loader, '--library-path', '/reference/lib:/reference/usr/lib', args.nginx, '-v'])]:
        rc, stdout, stderr = await reloading.command(command, 10)
        if rc != 0:
            raise RuntimeError('version_failed')
        environment[name + '_version'] = (stdout + stderr).decode('utf-8', 'replace').strip()[:512]
    (output / 'environment.json').write_text(json.dumps(environment, indent=2))
    records = []
    backend = server = None
    try:
        check_ports()
        os.sched_setaffinity(0, args.client_cpus)
        with (output / 'backend.log').open('x') as blog:
            backend = launch([sys.executable, str(Path(__file__).with_name('backend.py')), '--host', '127.0.0.1', '--port', '19090'], args.backend_cpus, blog)
            await backend_ready(backend)
            for name in ('rill', 'nginx'):
                command = ([args.rill, 'run', '--config', str(work / 'rill-active.kdl')] if name == 'rill' else
                           [args.nginx_loader, '--library-path', '/reference/lib:/reference/usr/lib', args.nginx, '-e', 'stderr', '-p', str(work) + '/', '-c', str(work / 'nginx-active.conf')])
                with (output / f'{name}.log').open('x') as log:
                    server = launch(command, args.server_cpus, log)
                    try:
                        await ready(server, ca)
                        for kind, func in (('ws', streams.run_ws), ('sse', streams.run_sse)):
                            result = await func(ca, 3, 1, output / f'{name}-{kind}-correctness.csv.gz')
                            if not result['valid']:
                                raise RuntimeError('stream_correctness_failed')
                        if 'http' in args.only:
                            await http_rounds(args, output, name, server, backend, ca)
                        if 'streams' in args.only:
                            await stream_rounds(args, output, name, server, backend, ca)
                        if 'idle' in args.only:
                            await idle_rounds(args, output, name, server, backend)
                        if 'reload' in args.only:
                            await reload_rounds(args, output, work, name, server, backend, ca)
                    finally:
                        stop(server, name, records)
                        server = None
    finally:
        stop(server, 'server', records)
        stop(backend, 'backend', records)
        (output / 'process-exits.json').write_text(json.dumps(records, indent=2))
    if any(record['forced_kill'] or record['remaining_live'] or record['returncode'] != 0 for record in records):
        raise RuntimeError('unexpected_process_exit')


async def http_rounds(args, output, name, server, backend, ca):
    for tls in (False, True):
        for path in ('/proxy', '/small.bin', '/large.bin'):
            for warm in (False, True):
                for repeat in range(1, args.repeats + 1):
                    if server.poll() is not None or backend.poll() is not None:
                        raise RuntimeError('service exited during benchmark')
                    label = f'{name}-{ "https" if tls else "http"}-{path[1:].replace(".bin", "")}-{ "warm" if warm else "cold"}-{repeat}'
                    sampler = metrics.Sampler(output / f'{label}-resources.csv', {'server': server.pid, 'backend': backend.pid, 'client': os.getpid()})
                    sample_task = asyncio.create_task(sampler.run())
                    try:
                        result = await traffic.round_http(path, tls, warm, ca, args.http_seconds, args.warmup_seconds, args.concurrency, output / f'{label}-requests.csv.gz')
                    finally:
                        sample_task.cancel()
                        await asyncio.gather(sample_task, return_exceptions=True)
                        sampler.close()
                    if sample_task.done() and not sample_task.cancelled():
                        sample_task.result()
                    result['valid'] = result['valid'] and server.poll() is None and backend.poll() is None
                    result['resource_peaks'] = sampler.peaks
                    result['client_limit_warning'] = sampler.peaks.get('client', {}).get('cpu_percent', 0) >= 90 or result['event_loop_lag_max_seconds'] >= .1
                    result['scenario'] = label
                    (output / f'{label}-summary.json').write_text(json.dumps(result, indent=2))
                    if not result['valid']:
                        raise RuntimeError(f'invalid round: {label}')


async def sampled(output, label, server, backend, operation):
    sampler = metrics.Sampler(output / f'{label}-resources.csv', {'server': server.pid, 'backend': backend.pid, 'client': os.getpid()})
    task = asyncio.create_task(sampler.run())
    try:
        result = await operation
        if task.done():
            task.result()
        return result, sampler.peaks
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        sampler.close()


async def stream_rounds(args, output, name, server, backend, ca):
    for kind, func in (('ws', streams.run_ws), ('sse', streams.run_sse)):
        for count in args.stream_connections:
            for repeat in range(1, args.repeats + 1):
                if server.poll() is not None or backend.poll() is not None:
                    raise RuntimeError('service_exited')
                label = f'{name}-{kind}-{count}-{repeat}'
                result, peaks = await sampled(output, label, server, backend, func(ca, args.stream_seconds, count, output / f'{label}-events.csv.gz'))
                result['scenario'] = label
                result['resource_peaks'] = peaks
                result['client_limit_warning'] = peaks.get('client', {}).get('cpu_percent', 0) >= 90 or result['event_loop_lag_max_seconds'] >= .1
                result['valid'] = result['valid'] and server.poll() is None and backend.poll() is None
                (output / f'{label}-summary.json').write_text(json.dumps(result, indent=2))
                if not result['valid']:
                    raise RuntimeError('invalid_stream_round')


async def idle_rounds(args, output, name, server, backend):
    for repeat in range(1, args.repeats + 1):
        label = f'{name}-idle-{repeat}'
        if server.poll() is not None or backend.poll() is not None:
            raise RuntimeError('service_exited')
        _, peaks = await sampled(output, label, server, backend, asyncio.sleep(args.idle_seconds))
        with (output / f'{label}-resources.csv').open(newline='') as src:
            rows = [row for row in csv.DictReader(src) if row['group'] == 'server']
        valid = bool(rows) and all(int(row['processes']) > 0 for row in rows) and server.poll() is None and backend.poll() is None
        result = {'scenario': label, 'valid': valid, 'samples': len(rows), 'resource_peaks': peaks,
                  'server_max_threads': max((int(row['threads']) for row in rows), default=None),
                  'server_max_fds': max((int(row['fds']) for row in rows), default=None),
                  'latency': None, 'throughput': None}
        (output / f'{label}-summary.json').write_text(json.dumps(result, indent=2))
        if not valid:
            raise RuntimeError('invalid_idle_round')


async def reload_rounds(args, output, work, name, server, backend, ca):
    age = 10
    for repeat in range(1, args.repeats + 1):
        if server.poll() is not None or backend.poll() is not None:
            raise RuntimeError('service_exited')
        label = f'{name}-reload-{repeat}'
        result, peaks = await sampled(output, label, server, backend,
            reloading.run(args, name, work, output, label, server, ca, age))
        summary, age = result
        summary['scenario'] = label
        summary['resource_peaks'] = peaks
        lag = max((summary.get(key, {}).get('event_loop_lag_max_seconds', 0) for key in ('http_load', 'ws', 'sse')), default=0)
        summary['client_limit_warning'] = peaks.get('client', {}).get('cpu_percent', 0) >= 90 or lag >= .1
        summary['valid'] = summary['valid'] and server.poll() is None and backend.poll() is None
        (output / f'{label}-summary.json').write_text(json.dumps(summary, indent=2))
        if not summary['valid']:
            raise RuntimeError('invalid_reload_round')


def connection_counts(value):
    try:
        numbers = [positive(part) for part in value.split(',')]
    except (ValueError, argparse.ArgumentTypeError) as exc:
        raise argparse.ArgumentTypeError('expected positive comma-separated connection counts') from exc
    if len(set(numbers)) != len(numbers) or any(n > 256 for n in numbers):
        raise argparse.ArgumentTypeError('counts must be unique and at most 256')
    return numbers


def groups(value):
    values = value.split(',')
    if values == ['all']:
        return ['http', 'streams', 'idle', 'reload']
    if not values or len(values) != len(set(values)) or not set(values) <= {'http', 'streams', 'idle', 'reload'}:
        raise argparse.ArgumentTypeError('expected all or comma-separated http,streams,idle,reload')
    return values


async def run_with_signals(args, work, output):
    loop = asyncio.get_running_loop()
    task = asyncio.current_task()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, task.cancel)
    try:
        await execute(args, work, output)
    finally:
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.remove_signal_handler(sig)


def main():
    parser = argparse.ArgumentParser(description='HTTP, streams, idle and reload baseline')
    parser.add_argument('--rill', required=True)
    parser.add_argument('--nginx', default='/reference/usr/sbin/nginx')
    parser.add_argument('--nginx-loader', default='/reference/lib/ld-musl-x86_64.so.1')
    parser.add_argument('--workdir', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--cert', default='/fixture/cert.pem')
    parser.add_argument('--key', default='/fixture/key.pem')
    parser.add_argument('--ca', default='/fixture/ca.pem')
    parser.add_argument('--server-cpus', required=True, type=cpus)
    parser.add_argument('--client-cpus', required=True, type=cpus)
    parser.add_argument('--backend-cpus', required=True, type=cpus)
    parser.add_argument('--repeats', type=positive, default=3)
    parser.add_argument('--http-seconds', type=positive, default=30)
    parser.add_argument('--warmup-seconds', type=positive, default=2)
    parser.add_argument('--concurrency', type=positive, default=8)
    parser.add_argument('--stream-seconds', type=positive, default=60)
    parser.add_argument('--idle-seconds', type=positive, default=60)
    parser.add_argument('--stream-connections', type=connection_counts, default=[10, 100])
    parser.add_argument('--reload-seconds', type=positive, default=60)
    parser.add_argument('--reload-interval', type=positive, default=10)
    parser.add_argument('--reload-count', type=positive, default=5)
    parser.add_argument('--only', type=groups, default=['http', 'streams', 'idle', 'reload'])
    args = parser.parse_args()
    if args.reload_interval * args.reload_count >= args.reload_seconds:
        parser.error('reload interval times count must be less than reload seconds')
    if args.server_cpus & args.client_cpus or args.server_cpus & args.backend_cpus or args.client_cpus & args.backend_cpus:
        parser.error('CPU sets must be disjoint')
    work, output = Path(args.workdir).absolute(), Path(args.output).absolute()
    try:
        work.mkdir(mode=0o700)
        output.mkdir(mode=0o700)
    except OSError:
        print('baseline invalid: output_setup', file=sys.stderr)
        return 1
    environment_path = output / 'environment.json'
    environment_path.write_text(json.dumps({'status': 'running', 'selected_groups': args.only, 'missing': ['http', 'streams', 'idle', 'reload']}, indent=2))
    try:
        asyncio.run(run_with_signals(args, work, output))
    except (Exception, asyncio.CancelledError) as exc:
        environment = json.loads(environment_path.read_text())
        environment['status'] = 'invalid'
        environment['error'] = type(exc).__name__
        environment_path.write_text(json.dumps(environment, indent=2))
        print('baseline invalid: execution_failed', file=sys.stderr)
        return 1
    environment = json.loads(environment_path.read_text())
    environment['missing'] = [group for group in ('http', 'streams', 'idle', 'reload') if group not in args.only]
    environment['status'] = 'partial' if environment['missing'] else 'complete'
    environment_path.write_text(json.dumps(environment, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
