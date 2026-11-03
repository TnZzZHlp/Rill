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
import sys
import time

import container
from container import Container
import fixtures
import metrics
import targets
import traffic
import streams
import reloading

GROUPS = ('http', 'streams', 'idle', 'reload')
BACKEND_NAME = 'bench-run-backend'
WORK = None
RUN_ID = 'bench-run'


def cpus(value):
    try:
        values = {int(part) for part in value.split(',')}
    except ValueError as exc:
        raise argparse.ArgumentTypeError('expected comma-separated CPU numbers') from exc
    if not values or not values <= os.sched_getaffinity(0):
        raise argparse.ArgumentTypeError('expected available CPUs')
    return values


def positive(value):
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError('must be positive')
    return result


def check_ports():
    for port in (18080, 18443):
        with socket.socket() as probe:
            try:
                probe.bind(('127.0.0.1', port))
            except OSError as exc:
                raise RuntimeError(f'port {port} is in use') from exc


async def wait_ready(target, name):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if not container.running(name):
            raise RuntimeError(f'{target.name} exited before readiness: {container.logs(name, 40)}')
        try:
            await traffic.correctness(Path(WORK) / 'certs' / 'ca.pem')
            return
        except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, ValueError):
            await asyncio.sleep(.1)
    raise RuntimeError(f'{target.name} readiness timed out')


async def wait_backend(name):
    """The backend publishes nothing; probe its listener from inside its own netns."""
    probe = ['-c', f'import socket\ns=socket.create_connection(("127.0.0.1",{container.BACKEND_PORT}),2)\ns.close()']
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if not container.running(name):
            raise RuntimeError(f'backend exited: {container.logs(name, 40)}')
        rc, _, _ = await container.exec_(name, [container.CLIENT_ENTRYPOINT, *probe], timeout=10)
        if rc == 0:
            return
        await asyncio.sleep(.1)
    raise RuntimeError('backend readiness timed out')


def sampler_for(name, label, output):
    return metrics.Sampler(
        output / f'{label}-resources.csv',
        {'server': ('container', name),
         'backend': ('container', BACKEND_NAME),
         'client': ('pid', os.getpid())},
        {'server': container.cgroup_path(name)})


def target_version(target, name):
    proc = container.run(['exec', name, *target.version_argv()], timeout=60)
    text = (proc.stdout + proc.stderr).strip()
    return text[:512] if proc.returncode == 0 else f'exit {proc.returncode}: {text[:200]}'


def identity(target, name):
    data = container.environment(name) or {}
    return {
        'image': target.image,
        'image_id': data.get('image_id'),
        'image_digest': container.image_digest(target.image),
        'container_id': data.get('id'),
        'container_pid': data.get('pid'),
        'cgroup': data.get('cgroup'),
        'version': target_version(target, name),
        'entrypoint': target.entrypoint,
        'command': list(target.command),
        'cpu_set': container.SERVER_CPUS,
    }


async def check_configs(target, work, server_name):
    """Check both A and B templates before load, so a bad rollback target fails early."""
    results = {}
    for label in ('a', 'b'):
        config = target.template(work, label)
        rc, stdout, stderr = await target.check(work, config, server_name, in_container=f'/work/{config.name}')
        results[label] = {'returncode': rc, 'output': (stdout + stderr).decode('utf-8', 'replace')[:2000]}
        if rc != 0:
            detail = (stdout + stderr).decode('utf-8', 'replace').strip()[:800]
            raise RuntimeError(f'{target.name} config check failed for {config.name}: {detail}')
    return results


async def execute(args, work, output):
    global WORK
    WORK = work
    hashes = fixtures.prepare(work, Path(args.cert).resolve(), Path(args.key).resolve(), Path(args.ca).resolve())
    fixtures.prepare_runtime(work)
    prepare_backend_context(work)
    ca = (work / 'certs' / 'ca.pem').resolve()
    records = []
    owned = []
    created = []
    leftovers = []
    network_owned = False
    try:
        container.create_network()
        network_owned = True
        created.extend(f'{RUN_ID}-{name}' for name in args.targets)
        created.append(BACKEND_NAME)
        if 'caddy' in args.targets:
            fixtures.prepare_caddy(targets.get('caddy'), work)
        fixtures.publish_active(work)
        await run(args, work, output, ca, hashes, records, owned, created)
    finally:
        for instance in reversed(owned):
            if instance.discard_record is not None:
                # Start failed; the discard attempt was the only cleanup needed.
                records.append(instance.discard_record)
                if not instance.discard_record.get('removed'):
                    leftovers.append(f'unexpected:{instance.name}')
                continue
            if instance.removed:
                continue
            if instance.identifier is None:
                # Never created: podman reported the failure before an id existed.
                continue
            if instance.failure:
                leftovers.append(f'discarded-but-present:{instance.name}')
            try:
                record = instance.stop()
                records.append(record)
                if not record.get('removed'):
                    leftovers.append(f'unexpected:{instance.name}')
            except Exception as exc:
                records.append({'name': instance.name, 'id': instance.identifier,
                                'cleanup_error': f'{type(exc).__name__}: {exc}'[:300]})
                leftovers.append(f'unexpected:{instance.name}')
        for step, action in (('residue', lambda: container.residue(created)),
                             ('network', lambda: network_owned and container.remove_network()),
                             ('network_check', container.network_exists)):
            try:
                outcome = action()
            except Exception as exc:
                leftovers.append(f'{step}_failed:{type(exc).__name__}: {exc}'[:300])
                continue
            if step == 'residue':
                leftovers = sorted(set(leftovers) | set(outcome))
            elif step == 'network_check' and outcome:
                leftovers.append(f'network:{container.NETWORK}')
        output.joinpath('process-exits.json').write_text(
            json.dumps({'containers': records, 'residue': leftovers}, indent=2))
    if leftovers:
        raise RuntimeError(f'container residue detected: {leftovers}')
    for record in records:
        if record.get('cleanup_error'):
            raise RuntimeError(f'container cleanup failed: {record}')
        if record.get('errors'):
            raise RuntimeError(f'container stop failed: {record}')
        if record.get('stop_returncode') not in (0, None):
            raise RuntimeError(f'container stop returned nonzero: {record}')
        if not record.get('removed'):
            raise RuntimeError(f'container not removed: {record}')
        if record.get('discarded'):
            # Never served; the start failure itself already invalidated the run.
            continue
        if 'exit_code' not in record or record['exit_code'] is None:
            raise RuntimeError(f'container exit state unknown: {record}')
        if record['exit_code'] != 0 or record.get('forced'):
            raise RuntimeError(f'unexpected container exit: {record}')


async def run(args, work, output, ca, hashes, records, owned, created):
    environment = {
        'status': 'running', 'missing': list(GROUPS), 'selected_groups': args.only,
        'targets_requested': args.targets, 'python': sys.version, 'platform': platform.platform(),
        'cpu_model': next((line.strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')), 'unknown'),
        'cpu_topology': topology(),
        'cpu_affinity': {'server': container.SERVER_CPUS, 'backend': container.BACKEND_CPUS,
                         'client': sorted(container.CLIENT_CPUS)},
        'limits': {name: resource.getrlimit(getattr(resource, name)) for name in ('RLIMIT_NOFILE', 'RLIMIT_NPROC', 'RLIMIT_AS')},
        'container_limits': container.LIMITS,
        'container_network': {'name': container.NETWORK, 'subnet': container.NETWORK_SUBNET,
                              'gateway': container.NETWORK_GATEWAY, 'publish': ['127.0.0.1:18080', '127.0.0.1:18443'],
                              'backend': f'{container.BACKEND_IP}:{container.BACKEND_PORT}',
                              'backend_published': False,
                              'note': 'load generator reaches published ports from the host; server reaches the backend over the bridge'},

        'runtime': {'podman': podman_version(), 'cgroup': 'v2', 'rootless': podman_rootless()},
        'arguments': {k: sorted(v) if isinstance(v, set) else v for k, v in vars(args).items()},
        'fixture_sha256': hashes, 'ca_sha256': fixtures.hash_file(ca),
        'tls': 'CA and hostname verified; TLS >= 1.2; session cache and tickets disabled',
        'clock': 'monotonic', 'load': 'closed-loop HTTP/1.1; not saturation capacity',
        'cold_definition': 'new TCP and TLS connection per request; page cache not dropped',
        'disclosures': targets.DISCLOSURES,
    }
    output.joinpath('environment.json').write_text(json.dumps(environment, indent=2))
    check_ports()
    shared = Container(BACKEND_NAME, work)
    owned.append(shared)
    shared.start_backend()
    await wait_backend(BACKEND_NAME)
    for name in args.targets:
        target = targets.get(name)
        problems = fixtures.verify(work, tuple(args.targets))
        if problems:
            raise RuntimeError(f'fixture verification failed: {problems[:3]}')
        server_name = f'{RUN_ID}-{name}'
        instance = Container(server_name, work)
        owned.append(instance)
        instance.start(target)
        await wait_ready(target, server_name)
        environment.setdefault('targets', {})[name] = identity(target, server_name)
        environment.setdefault('config_check', {})[name] = await check_configs(target, work, server_name)
        environment.setdefault('config_sha256', {})[name] = {
            path.name: fixtures.hash_file(path)
            for path in sorted(work.glob(f'{name}-*.{target.config_suffix}'))
        }
        output.joinpath('environment.json').write_text(json.dumps(environment, indent=2))
        for kind, func in (('ws', streams.run_ws), ('sse', streams.run_sse)):
            result = await func(ca, 3, 1, output / f'{name}-{kind}-correctness.csv.gz')
            if not result['valid']:
                raise RuntimeError(f'{name} {kind} correctness failed')
        if 'http' in args.only:
            await http_rounds(args, output, name, server_name, ca)
        if 'streams' in args.only:
            await stream_rounds(args, output, name, server_name, ca)
        if 'idle' in args.only:
            await idle_rounds(args, output, name, server_name)
        if 'reload' in args.only:
            await reload_rounds(args, output, work, name, server_name, ca)
        outcome = instance.stop()
        records.append(outcome)
        if not outcome.get('removed'):
            # The cleanup finally retries via owned; the failure stays in the report.
            raise RuntimeError(f'container {server_name} could not be removed after its rounds')


def prepare_backend_context(work):
    """Backend script is mounted read-only; the mount source must exist before start."""
    source = Path(__file__).with_name('backend.py')
    target = work / 'bench'
    target.mkdir(mode=0o755, exist_ok=True)
    target.chmod(0o755)
    path = target / 'backend.py'
    path.write_bytes(source.read_bytes())
    path.chmod(0o644)


def topology():
    pairs = {}
    for path in sorted(Path('/sys/devices/system/cpu').glob('cpu[0-9]*/topology/thread_siblings_list')):
        cpu = path.parts[-3]
        if cpu == 'cpu' or not cpu[3:].isdigit():
            continue
        pairs[int(cpu[3:])] = path.read_text().strip()
    return {'thread_siblings': pairs,
            'server_cpus_share_core': shared_core(container.SERVER_CPUS),
            'backend_cpus_share_core': shared_core(container.BACKEND_CPUS)}


def shared_core(spec):
    pairs = {}
    for part in spec.split(','):
        for path in Path('/sys/devices/system/cpu').glob(f'cpu{part}/topology/thread_siblings_list'):
            pairs[part] = path.read_text().strip()
    unique = {value.split('-')[0].split(',')[0] for value in pairs.values()}
    return {'cpus': pairs, 'physical_cores': len(unique), 'smt_shared': len(unique) < len(pairs)}


def podman_version():
    proc = container.run(['--version'], timeout=30)
    return proc.stdout.strip()[:120]


def podman_rootless():
    proc = container.run(['info', '--format', '{{.Host.Security.Rootless}}'], timeout=60)
    return proc.stdout.strip() == 'true'


async def http_rounds(args, output, name, server, ca):
    for tls in (False, True):
        for path in ('/proxy', '/small.bin', '/large.bin'):
            for warm in (False, True):
                for repeat in range(1, args.repeats + 1):
                    if not container.running(server):
                        raise RuntimeError('service exited during benchmark')
                    label = f'{name}-{"https" if tls else "http"}-{path[1:].replace(".bin", "")}-{"warm" if warm else "cold"}-{repeat}'
                    sampler = sampler_for(server, label, output)
                    sample_task = asyncio.create_task(sampler.run())
                    try:
                        result = await traffic.round_http(path, tls, warm, ca, args.http_seconds, args.warmup_seconds, args.concurrency, output / f'{label}-requests.csv.gz')
                    finally:
                        sample_task.cancel()
                        await asyncio.gather(sample_task, return_exceptions=True)
                        sampler.close()
                    if sample_task.done() and not sample_task.cancelled():
                        sample_task.result()
                    result.update(finish(result, sampler))
                    result['valid'] = (result['valid'] and container.running(server)
                                       and result['resource_integrity'] == 'ok')
                    result['scenario'] = label
                    output.joinpath(f'{label}-summary.json').write_text(json.dumps(result, indent=2))
                    if not result['valid']:
                        raise RuntimeError(f'invalid round: {label}')


def finish(result, sampler):
    missing = (sampler.peaks.get('server') or {}).get('missing_root_samples', 0)
    return {'resource_peaks': sampler.peaks,
            'server_missing_root_samples': missing,
            'backend_peak': sampler.peaks.get('backend'),
            'client_limit_warning': sampler.peaks.get('client', {}).get('cpu_percent', 0) >= 90
            or result.get('event_loop_lag_max_seconds', 0) >= .1,
            'resource_accounting': 'process tree summed over the container init pid; cgroup v2 series in -cgroup.csv',
            'resource_integrity': 'ok' if missing == 0 else 'server process tree empty in some samples'}


async def sampled(output, label, server, operation):
    sampler = sampler_for(server, label, output)
    task = asyncio.create_task(sampler.run())
    try:
        result = await operation
        if task.done():
            task.result()
        return result, sampler
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        sampler.close()


async def stream_rounds(args, output, name, server, ca):
    for kind, func in (('ws', streams.run_ws), ('sse', streams.run_sse)):
        for count in args.stream_connections:
            for repeat in range(1, args.repeats + 1):
                if not container.running(server):
                    raise RuntimeError('service_exited')
                label = f'{name}-{kind}-{count}-{repeat}'
                result, sampler = await sampled(output, label, server, func(ca, args.stream_seconds, count, output / f'{label}-events.csv.gz'))
                result['scenario'] = label
                result.update(finish(result, sampler))
                result['valid'] = (result['valid'] and container.running(server)
                                   and result['resource_integrity'] == 'ok')
                output.joinpath(f'{label}-summary.json').write_text(json.dumps(result, indent=2))
                if not result['valid']:
                    raise RuntimeError('invalid_stream_round')


async def idle_rounds(args, output, name, server):
    for repeat in range(1, args.repeats + 1):
        label = f'{name}-idle-{repeat}'
        if not container.running(server):
            raise RuntimeError('service_exited')
        _, sampler = await sampled(output, label, server, asyncio.sleep(args.idle_seconds))
        with output.joinpath(f'{label}-resources.csv').open(newline='') as src:
            rows = [row for row in csv.DictReader(src) if row['group'] == 'server']
        integrity = finish({'event_loop_lag_max_seconds': 0}, sampler)
        valid = (bool(rows) and all(int(row['processes']) > 0 for row in rows) and container.running(server)
                 and integrity['resource_integrity'] == 'ok')
        result = {'scenario': label, 'valid': valid, 'samples': len(rows),
                  'resource_peaks': sampler.peaks,
                  'server_max_threads': max((int(row['threads']) for row in rows), default=None),
                  'server_max_fds': max((int(row['fds']) for row in rows), default=None),
                  'latency': None, 'throughput': None,
                  **integrity}
        output.joinpath(f'{label}-summary.json').write_text(json.dumps(result, indent=2))
        if not valid:
            raise RuntimeError('invalid_idle_round')


async def reload_rounds(args, output, work, name, server, ca):
    age = 10
    for repeat in range(1, args.repeats + 1):
        if not container.running(server):
            raise RuntimeError('service_exited')
        label = f'{name}-reload-{repeat}'
        target = targets.get(name)
        result, sampler = await sampled(output, label, server,
            reloading.run(args, target, work, output, label, server, ca, age))
        summary, age = result
        summary['scenario'] = label
        summary.update(finish(summary, sampler))
        lag = max((summary.get(key, {}).get('event_loop_lag_max_seconds', 0) for key in ('http_load', 'ws', 'sse')), default=0)
        summary['client_limit_warning'] = summary['client_limit_warning'] or lag >= .1
        summary['valid'] = (summary['valid'] and container.running(server)
                            and summary['resource_integrity'] == 'ok')
        output.joinpath(f'{label}-summary.json').write_text(json.dumps(summary, indent=2))
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
        return list(GROUPS)
    if not values or len(values) != len(set(values)) or not set(values) <= set(GROUPS):
        raise argparse.ArgumentTypeError('expected all or comma-separated http,streams,idle,reload')
    return values


def target_list(value):
    values = value.split(',')
    if not values or len(values) != len(set(values)) or not set(values) <= set(targets.TARGETS):
        raise argparse.ArgumentTypeError(f'expected comma-separated subset of {",".join(targets.ORDER)}')
    return [name for name in targets.ORDER if name in values]


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
    parser.add_argument('--targets', type=target_list, default=list(targets.ORDER))
    parser.add_argument('--workdir', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--cert', required=True, help='leaf certificate covering bench.test')
    parser.add_argument('--key', required=True, help='leaf private key; never committed')
    parser.add_argument('--ca', required=True, help='trusted CA bundle for the fixture chain')
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
    parser.add_argument('--only', type=groups, default=list(GROUPS))
    args = parser.parse_args()
    if args.reload_interval * args.reload_count >= args.reload_seconds:
        parser.error('reload interval times count must be less than reload seconds')
    work, output = Path(args.workdir).absolute(), Path(args.output).absolute()
    global RUN_ID, BACKEND_NAME
    RUN_ID = f'bench-{work.name}'[:56]
    BACKEND_NAME = f'{RUN_ID}-backend'
    try:
        work.mkdir(mode=0o711, parents=True)
        output.mkdir(mode=0o700)
    except OSError:
        print('baseline invalid: output_setup', file=sys.stderr)
        return 1
    environment_path = output / 'environment.json'
    environment_path.write_text(json.dumps({'status': 'running', 'selected_groups': args.only,
                                            'missing': list(GROUPS), 'targets_requested': args.targets}, indent=2))
    try:
        os.sched_setaffinity(0, container.CLIENT_CPUS)
        asyncio.run(run_with_signals(args, work, output))
    except (Exception, asyncio.CancelledError) as exc:
        environment = json.loads(environment_path.read_text())
        environment['status'] = 'invalid'
        environment['error'] = f'{type(exc).__name__}: {exc}'[:2000]
        environment_path.write_text(json.dumps(environment, indent=2))
        print(f'baseline invalid: {type(exc).__name__}: {exc}', file=sys.stderr)
        return 1
    environment = json.loads(environment_path.read_text())
    environment['missing'] = []
    environment['status'] = 'complete' if args.only == list(GROUPS) else 'partial'
    environment_path.write_text(json.dumps(environment, indent=2))
    print(json.dumps({'status': environment['status'], 'targets': args.targets, 'groups': args.only}))
    return 0


if __name__ == '__main__':
    sys.exit(main())
