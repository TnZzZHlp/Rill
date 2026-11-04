import argparse
import asyncio
import hashlib
import json
import os
import platform
import re
import resource
import signal
import socket
import sys
import time
from pathlib import Path

import metrics
import traffic

MODES = ('off', 'access', 'metrics', 'trace', 'all')
COUNTERS = ('rill_log_queue_drop_total', 'rill_trace_queue_drop_total',
            'rill_log_export_failure_total', 'rill_trace_export_failure_total')
GAUGE = 'rill_generation'
LIMIT_AS = 2 * 1024**3
COLLECTOR_LIMIT_AS = 8 * 1024**3
LIMIT_FSIZE = 512 * 1024**2
COLLECTOR_CONFIG = '''receivers:
  otlp:
    protocols:
      http:
        endpoint: 127.0.0.1:4318
exporters:
  debug:
    verbosity: basic
    sampling_initial: 1000000
    sampling_thereafter: 1
service:
  telemetry:
    logs:
      encoding: json
      level: info
    metrics:
      level: none
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [debug]
'''


def sha256(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(65536), b''):
            h.update(block)
    return h.hexdigest()


def cpus(value):
    parts = value.split(',')
    if any(not p.isdigit() for p in parts) or len(set(parts)) != len(parts):
        raise argparse.ArgumentTypeError('expected distinct comma-separated CPUs')
    return {int(p) for p in parts}


def arguments():
    p = argparse.ArgumentParser()
    for name in ('rill', 'collector', 'workdir', 'output', 'provenance-json'):
        p.add_argument('--' + name, required=True, type=Path)
    p.add_argument('--seconds', type=float, default=30)
    p.add_argument('--repeats', type=int, default=3)
    p.add_argument('--warmup', type=float, default=2)
    p.add_argument('--concurrency', type=int, default=8)
    for name, default in (('server', '4,5'), ('client', '6,7'), ('backend', '8,9'), ('collector', '10,11')):
        p.add_argument('--' + name + '-cpus', type=cpus, default=cpus(default))
    p.add_argument('--only', help='comma-separated subset of off,access,metrics,trace,all')
    return p.parse_args()


def limits():
    return {name: list(resource.getrlimit(getattr(resource, name))) for name in
            ('RLIMIT_AS', 'RLIMIT_CPU', 'RLIMIT_NPROC', 'RLIMIT_NOFILE', 'RLIMIT_FSIZE')}


def bound_rlimit(kind, soft_cap, hard_cap, use_hard_for_soft=False):
    soft, hard = resource.getrlimit(kind)
    hard = hard_cap if hard == resource.RLIM_INFINITY else min(hard, hard_cap)
    if use_hard_for_soft or soft == resource.RLIM_INFINITY:
        soft = min(soft_cap, hard)
    else:
        soft = min(soft, soft_cap, hard)
    resource.setrlimit(kind, (soft, hard))


def child_setup(affinity, collector):
    def setup():
        os.sched_setaffinity(0, affinity)
        as_cap = COLLECTOR_LIMIT_AS if collector else LIMIT_AS
        bound_rlimit(resource.RLIMIT_AS, as_cap, as_cap, use_hard_for_soft=collector)
        bound_rlimit(resource.RLIMIT_FSIZE, LIMIT_FSIZE, LIMIT_FSIZE)
    return setup


def free_ports():
    for port in (18080, 18081, 19090, 4318):
        with socket.socket() as s:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind(('127.0.0.1', port))


async def tcp_ready(proc, port):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if proc.returncode is not None:
            raise RuntimeError('process exited during startup')
        try:
            _, writer = await asyncio.wait_for(asyncio.open_connection('127.0.0.1', port), .25)
            await traffic.close(writer)
            return
        except (OSError, asyncio.TimeoutError):
            await asyncio.sleep(.05)
    raise TimeoutError('startup port ' + str(port))


async def collector_ready(proc, port, logpath):
    await tcp_ready(proc, port)
    deadline = time.monotonic() + 10
    accepted_markers = ('everything is ready',)
    pending = b''
    with logpath.open('rb') as log:
        while time.monotonic() < deadline:
            if proc.returncode is not None:
                raise RuntimeError('collector exited before ready marker')
            if os.fstat(log.fileno()).st_size > 8 * 1024**2:
                raise ValueError('collector readiness log exceeds 8 MiB')
            chunk = log.read(65536)
            if chunk:
                pending += chunk
                while b'\n' in pending:
                    line, pending = pending.split(b'\n', 1)
                    if len(line) > 65536:
                        raise ValueError('collector readiness log line exceeds 64 KiB')
                    try:
                        event = json.loads(line)
                    except (ValueError, UnicodeDecodeError):
                        continue
                    message = event.get('msg') if isinstance(event, dict) else None
                    if isinstance(message, str) and message.casefold().startswith(accepted_markers):
                        if proc.returncode is not None:
                            raise RuntimeError('collector exited at ready marker')
                        return
                if len(pending) > 65536:
                    raise ValueError('collector readiness log line exceeds 64 KiB')
            else:
                await asyncio.sleep(.05)
    raise TimeoutError('collector service ready marker not observed')


async def server_ready(proc):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if proc.returncode is not None:
            raise RuntimeError('server exited during startup')
        try:
            reader, writer, _, _ = await traffic.connect(False, None)
            try:
                await traffic.exchange(reader, writer, '/proxy', False)
                return
            finally:
                await traffic.close(writer)
        except (OSError, asyncio.TimeoutError, traffic.EmptyResponse, traffic.HeaderReset,
                asyncio.IncompleteReadError, ValueError):
            await asyncio.sleep(.05)
    raise TimeoutError('proxy correctness readiness')


async def stop(proc, timeout):
    forced = False
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.returncode is None:
            try:
                await asyncio.wait_for(asyncio.shield(proc.wait()), .1)
            except asyncio.TimeoutError:
                pass
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
    try:
        os.killpg(proc.pid, 0)
        remaining = True
    except ProcessLookupError:
        remaining = False
    return {'exit_code': proc.returncode, 'forced': forced, 'group_remaining': remaining}


async def scrape():
    async def get():
        reader, writer = await asyncio.open_connection('127.0.0.1', 19090, limit=1048577)
        try:
            writer.write(b'GET /metrics HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n')
            await writer.drain()
            chunks = []
            size = 0
            while True:
                chunk = await reader.read(min(65536, 1048577 - size))
                if not chunk:
                    break
                chunks.append(chunk)
                size += len(chunk)
                if size > 1048576:
                    raise ValueError('metrics response too large')
            data = b''.join(chunks)
            if not data.startswith(b'HTTP/1.1 200 '):
                raise ValueError('metrics response')
            body = data.split(b'\r\n\r\n', 1)[1].decode('utf-8')
            values = {}
            for line in body.splitlines():
                parts = line.split()
                if len(parts) == 2 and parts[0] in COUNTERS + (GAUGE,):
                    if parts[0] in values:
                        raise ValueError('duplicate metric')
                    values[parts[0]] = float(parts[1])
            if set(values) != set(COUNTERS + (GAUGE,)):
                raise ValueError('missing metrics')
            return values
        finally:
            await traffic.close(writer)
    return await asyncio.wait_for(get(), 2)


def backend_summary(path):
    if path.stat().st_size > 256 * 1024**2:
        raise ValueError('backend log exceeds read bound')
    text = path.read_text(errors='replace')
    matches = re.findall(r'^backend summary errors=(\d+) rejected=(\d+)$', text, re.M)
    if len(matches) != 1:
        raise ValueError('missing backend summary')
    return {'errors': int(matches[0][0]), 'rejected': int(matches[0][1])}


async def one(args, work, output, number, mode):
    prefix = f'round-{number:02d}-{mode}'
    control = work / (prefix + '-control')
    control.mkdir(mode=0o700)
    config = work / (prefix + '.kdl')
    metric_line = 'metrics "127.0.0.1:19090";' if mode in ('metrics', 'all') else ''
    config.write_text(f'''server {{
 http "127.0.0.1:18080"; control-socket {json.dumps(str(control / 'control.sock'))};
 upstream-allow "127.0.0.0/8"; log-level "off";
 observability {{ access-log #{str(mode in ('access', 'all')).lower()}; access-sample-per-mille 1000;
 {metric_line} tracing #{str(mode in ('trace', 'all')).lower()}; trace-sample-per-mille 100;
 otlp-endpoint "http://127.0.0.1:4318/v1/traces"; }};
}}
site "bench.test" {{ proxy "http://127.0.0.1:18081"; }}
''')
    result = {'round': number, 'mode': mode, 'config': str(config), 'config_sha256': sha256(config),
              'valid': False, 'stop': {}, 'counter_availability': 'available' if metric_line else 'unavailable_no_metrics_endpoint',
              'metrics_before': None, 'metrics_after': None, 'counter_delta': None,
              'metrics_before_monotonic': None, 'metrics_after_monotonic': None, 'child_limits': {}}
    procs, files = {}, []
    sampler = sampling = None
    try:
        free_ports()
        env = {k: os.environ[k] for k in ('PATH', 'HOME', 'TMPDIR', 'LANG', 'LC_ALL') if k in os.environ}
        async def launch(name, command, affinity, extra_env=None):
            out = (work / (prefix + '-' + name + '-stdout.log')).open('xb')
            err = (work / (prefix + '-' + name + '-stderr.log')).open('xb')
            files.extend((out, err))
            procs[name] = await asyncio.create_subprocess_exec(*command, cwd=work, env=dict(env, **(extra_env or {})),
                stdout=out, stderr=err, start_new_session=True, preexec_fn=child_setup(affinity, name == 'collector'))
            proc = procs[name]
            if proc.returncode is not None:
                raise RuntimeError(name + ' exited before limit inspection')
            result['child_limits'][name] = {key: list(resource.prlimit(proc.pid, getattr(resource, key)))
                                            for key in ('RLIMIT_AS', 'RLIMIT_FSIZE')}
            for key, cap in (('RLIMIT_AS', COLLECTOR_LIMIT_AS if name == 'collector' else LIMIT_AS),
                             ('RLIMIT_FSIZE', LIMIT_FSIZE)):
                if any(value == resource.RLIM_INFINITY or value > cap for value in result['child_limits'][name][key]):
                    raise RuntimeError(name + ' limit exceeds cap: ' + key)
            if proc.returncode is not None:
                raise RuntimeError(name + ' exited during limit inspection')
        await launch('backend', [sys.executable, str(Path(__file__).resolve().with_name('backend.py')), '--host', '127.0.0.1', '--port', '18081', '--max-connections', '128'], args.backend_cpus)
        await tcp_ready(procs['backend'], 18081)
        if mode in ('trace', 'all'):
            await launch('collector', [str(args.collector), '--config', str(work / 'collector.yaml')], args.collector_cpus, {'GOMAXPROCS': '2', 'GOMEMLIMIT': '128MiB'})
            await collector_ready(procs['collector'], 4318, work / (prefix + '-collector-stderr.log'))
        await launch('server', [str(args.rill), 'run', '--config', str(config)], args.server_cpus)
        await server_ready(procs['server'])
        if metric_line:
            start = time.monotonic()
            result['metrics_before'] = await scrape()
            result['metrics_before_monotonic'] = {'start': start, 'end': time.monotonic()}
        roots = {'server': ('pid', procs['server'].pid), 'backend': ('pid', procs['backend'].pid), 'client': ('pid-self', os.getpid())}
        if 'collector' in procs:
            roots['collector'] = ('pid', procs['collector'].pid)
        sampler = metrics.Sampler(output / (prefix + '-resources.csv'), roots=roots)
        sampler.sample()
        if any(sampler.peaks[name]['missing_root_samples'] for name in roots):
            raise RuntimeError('missing initial sampling root')
        async def periodic_sampling():
            await asyncio.sleep(1)
            await sampler.run()
        sampling = asyncio.create_task(periodic_sampling())
        result['http'] = await traffic.round_http('/proxy', False, True, None, args.seconds, args.warmup, args.concurrency,
            output / (prefix + '-requests.csv.gz'), window_path=output / (prefix + '-window.json'), allow_retries=False)
        if sampling.done():
            sampling.result()
        sampler.sample()
        result['resources'] = sampler.peaks
        result['client_warning'] = sampler.peaks['client']['cpu_percent'] >= 90 or result['http']['event_loop_lag_max_seconds'] >= .1
        if metric_line:
            start = time.monotonic()
            result['metrics_after'] = await scrape()
            result['metrics_after_monotonic'] = {'start': start, 'end': time.monotonic()}
            result['counter_delta'] = {k: result['metrics_after'][k] - result['metrics_before'][k] for k in COUNTERS}
            if any(v < 0 for v in result['counter_delta'].values()):
                raise ValueError('counter decreased')
    except (Exception, asyncio.CancelledError) as exc:
        result['error'] = f'{type(exc).__name__}: {exc}'
    finally:
        if sampling:
            sampling.cancel()
            await asyncio.gather(sampling, return_exceptions=True)
        if sampler:
            sampler.close()
        for name, timeout in (('server', 35), ('collector', 5), ('backend', 5)):
            if name in procs:
                result['stop'][name] = await stop(procs[name], timeout)
        for f in files:
            f.close()
    try:
        if 'backend' in procs:
            result['backend_summary'] = backend_summary(work / (prefix + '-backend-stderr.log'))
        free_ports()
        result['ports_removed'] = True
        result['valid'] = (not result.get('error') and result.get('http', {}).get('valid', False)
            and all(x['exit_code'] == 0 and not x['forced'] and not x['group_remaining'] for x in result['stop'].values())
            and result.get('backend_summary') == {'errors': 0, 'rejected': 0}
            and not (control / 'control.sock').exists()
            and all(v['missing_root_samples'] == 0 for v in result.get('resources', {}).values()))
    except Exception as exc:
        result['cleanup_error'] = f'{type(exc).__name__}: {exc}'
    return result


async def main():
    args = arguments()
    summary = {'status': 'invalid', 'rounds': []}
    output = None
    loop = asyncio.get_running_loop()
    task = asyncio.current_task()
    def interrupt():
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.remove_signal_handler(sig)
            signal.signal(sig, signal.SIG_IGN)
        task.cancel()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, interrupt)
    try:
        if not (0 < args.seconds <= 3600 and 0 <= args.warmup <= 3600 and 1 <= args.repeats <= 100 and 1 <= args.concurrency <= 128):
            raise ValueError('invalid parameters')
        chosen = list(MODES) if args.only is None else args.only.split(',')
        if not chosen or len(set(chosen)) != len(chosen) or any(m not in MODES for m in chosen):
            raise ValueError('invalid modes')
        chosen = [m for m in MODES if m in chosen]
        groups = [args.server_cpus, args.client_cpus, args.backend_cpus, args.collector_cpus]
        allowed = os.sched_getaffinity(0)
        if any(len(g) != 2 for g in groups) or len(set.union(*groups)) != 8 or not set.union(*groups) <= allowed:
            raise ValueError('expected eight distinct allowed CPUs, two per group')
        args.rill = args.rill.resolve(strict=True)
        args.collector = args.collector.resolve(strict=True)
        work = args.workdir.resolve()
        output_path = args.output.resolve()
        if work == output_path or work in output_path.parents or output_path in work.parents:
            raise ValueError('workdir and output must not overlap')
        work.mkdir(mode=0o700)
        output_path.mkdir(mode=0o700)
        output = output_path
        inherited_limits = limits()
        for kind, cap in ((resource.RLIMIT_CPU, 900), (resource.RLIMIT_NPROC, 128), (resource.RLIMIT_NOFILE, 512)):
            soft, _ = resource.getrlimit(kind)
            if soft == resource.RLIM_INFINITY or soft > cap:
                raise ValueError('external CPU, NPROC and NOFILE limits must be bounded')
        bound_rlimit(resource.RLIMIT_AS, LIMIT_AS, COLLECTOR_LIMIT_AS)
        bound_rlimit(resource.RLIMIT_FSIZE, LIMIT_FSIZE, LIMIT_FSIZE)
        os.sched_setaffinity(0, args.client_cpus)
        if os.sched_getaffinity(0) != args.client_cpus:
            raise ValueError('client affinity mismatch')
        (work / 'collector.yaml').write_text(COLLECTOR_CONFIG)
        with args.provenance_json.open('rb') as source:
            provenance_bytes = source.read(65537)
        if len(provenance_bytes) > 65536:
            raise ValueError('provenance exceeds 64 KiB')
        (output / 'provenance.json').write_bytes(provenance_bytes)
        summary['provenance_sha256'] = hashlib.sha256(provenance_bytes).hexdigest()
        provenance = json.loads(provenance_bytes)
        if not isinstance(provenance, dict):
            raise ValueError('provenance must be an object')
        binary_hash, collector_hash = sha256(args.rill), sha256(args.collector)
        if provenance.get('binary_sha256') != binary_hash or provenance.get('collector_sha256') != collector_hash:
            raise ValueError('provenance binary hash mismatch')
        packages = provenance.get('sdk_packages')
        required_packages = ('opentelemetry', 'opentelemetry_sdk', 'opentelemetry-otlp', 'opentelemetry-http', 'reqwest')
        if (not isinstance(packages, dict) or any(not isinstance(packages.get(k), str) or not packages[k] for k in required_packages)
                or not isinstance(provenance.get('collector_version'), str) or not provenance['collector_version']):
            raise ValueError('provenance package versions or collector version missing')
        manifest_hash = provenance.get('source_manifest_sha256')
        if manifest_hash is not None and (not isinstance(manifest_hash, str) or not re.fullmatch(r'[0-9a-f]{64}', manifest_hash)):
            raise ValueError('invalid source manifest hash')
        params = {k: (str(v) if isinstance(v, Path) else sorted(v) if isinstance(v, set) else v) for k, v in vars(args).items()}
        summary.update(parameters=params, selected_modes=chosen, rill_sha256=binary_hash,
            collector_sha256=collector_hash, collector_config_sha256=sha256(work / 'collector.yaml'),
            collector_version=provenance['collector_version'], sdk_versions={k: packages[k] for k in required_packages},
            source_manifest_sha256=manifest_hash,
            provenance_scope='Versions and source manifest supplied by parent; binary hashes matched, package metadata not extracted from binary or independently verified.',
            kernel=platform.release(), python=sys.version, environment_keys=[k for k in ('PATH', 'HOME', 'TMPDIR', 'LANG', 'LC_ALL') if k in os.environ],
            inherited_affinity=sorted(allowed), inherited_limits=inherited_limits, client_limits=limits(),
            collector_as_cap=COLLECTOR_LIMIT_AS, clock='time.monotonic', offered_load='closed-loop; RPS is not capacity',
            nominal_payload_bytes=1024,
            sampling_scope='immediately before warmup through request completion including drain; excludes shutdown; CPU peaks are one-second samples',
            memory_scope='RSS sums may double count shared pages; HWM is historical; no PSS or cgroup accounting',
            counter_scope='Snapshots are cumulative since process start and may include readiness, warmup, cohort and asynchronous callbacks before the after scrape. Delta excludes events already reflected in the before snapshot. Neither is an exact measurement-window count; after scrape precedes SDK shutdown, so final shutdown outcomes and tail failures may be missing. Generation is a gauge, not a counter.',
            readiness_scope='correctness proxy request precedes metrics baseline and measurement, and may emit access records and spans',
            logs_scope='raw whole-round logs include readiness, warmup, measurement and shutdown; no window counts inferred',
            off_scope='off may still have provider threads or other resource use; not zero-resource',
            collector_readiness='TCP connect and live process plus JSON service ready marker in collector stderr; receiver protocol correctness independently validated')
        number = 0
        for repeat in range(args.repeats):
            order = chosen[repeat % len(chosen):] + chosen[:repeat % len(chosen)]
            if repeat % 2:
                order.reverse()
            for mode in order:
                number += 1
                result = await one(args, work, output, number, mode)
                result['repeat'] = repeat + 1
                summary['rounds'].append(result)
                if not result['valid']:
                    raise RuntimeError('invalid round ' + str(number))
        summary['status'] = 'complete' if len(chosen) == len(MODES) else 'partial'
        return 0
    except (Exception, asyncio.CancelledError) as exc:
        summary['error'] = f'{type(exc).__name__}: {exc}'
        return 1
    finally:
        if output is not None:
            (output / 'summary.json').write_text(json.dumps(summary, indent=2, allow_nan=False) + '\n')
