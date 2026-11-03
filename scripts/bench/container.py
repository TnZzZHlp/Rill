"""Podman lifecycle for benchmark targets.

Only this module talks to the container runtime. Callers work with a container
name and the primitives below; they never parse podman output themselves.
"""

import asyncio
import json
import subprocess
from pathlib import Path

PODMAN = 'podman'
NETWORK = 'rill-bench'
NETWORK_SUBNET = '172.30.0.0/24'
NETWORK_GATEWAY = '172.30.0.1'
BACKEND_IP = '172.30.0.2'
CGROUP_ROOT = Path('/sys/fs/cgroup')

LIMITS = {
    'memory': '512m',
    'memory_swap': '512m',
    'pids_limit': '64',
    'nofile': '4096',
    'stop_timeout': '35',
}

CLIENT_IMAGE = 'docker.io/library/python:3.13-alpine'
CLIENT_ENTRYPOINT = '/usr/local/bin/python3'

SERVER_CPUS = '0,1'
BACKEND_CPUS = '4,5'
CLIENT_CPUS = {2, 3}

PUBLISH = ('18080', '18443')
BACKEND_PORT = 19090


class ContainerError(RuntimeError):
    pass


def run(args, timeout=120, check=False):
    proc = subprocess.run([PODMAN, *args], capture_output=True, text=True, timeout=timeout)
    if check and proc.returncode != 0:
        raise ContainerError(f'podman {" ".join(args)} failed: {proc.stderr.strip()[:400]}')
    return proc


def field(name, template, allow_missing=False):
    """Read one inspect field. A failure is an error unless absence is expected.

    Silently returning None would turn an inspection failure into "not running"
    and then into a force-removal reported as a clean result.
    """
    proc = run(['inspect', '-f', template, name], timeout=30)
    if proc.returncode != 0:
        if allow_missing and 'no such' in proc.stderr.lower():
            return None
        raise ContainerError(f'podman inspect {template} {name} failed: {proc.stderr.strip()[:300]}')
    return proc.stdout.strip()


def exists(name):
    proc = run(['container', 'exists', name], timeout=30)
    if proc.returncode == 0:
        return True
    if proc.returncode == 1:
        return False
    raise ContainerError(f'podman container exists {name} failed: {proc.stderr.strip()[:300]}')


def running(name, allow_missing=False):
    value = field(name, '{{.State.Running}}', allow_missing=allow_missing)
    if value is None:
        return False
    if value not in ('true', 'false'):
        raise ContainerError(f'container {name} running state unclear: {value!r}')
    return value == 'true'


def host_pid(name):
    value = field(name, '{{.State.Pid}}')
    if value is None or not value.isdigit() or int(value) <= 0:
        raise ContainerError(f'no host pid for container {name}')
    return int(value)


def exit_code(name, allow_missing=False):
    value = field(name, '{{.State.ExitCode}}', allow_missing=allow_missing)
    if value is None:
        return None
    if not value.lstrip('-').isdigit():
        raise ContainerError(f'container {name} exit code unclear: {value!r}')
    return int(value)


def image_digest(image):
    return field(image, '{{.Digest}}')


def network_exists(name=NETWORK):
    proc = run(['network', 'exists', name], timeout=30)
    if proc.returncode == 0:
        return True
    if proc.returncode == 1:
        return False
    raise ContainerError(f'podman network exists {name} failed: {proc.stderr.strip()[:300]}')


def network_gateway(name=NETWORK):
    proc = run(['network', 'inspect', name, '--format', '{{(index .Subnets 0).Gateway}}'], timeout=30, check=True)
    gateway = proc.stdout.strip()
    if not gateway:
        raise ContainerError(f'no gateway for network {name}')
    return gateway


def create_network(name=NETWORK):
    """Fixed subnet so the backend address is stable and no target needs a DNS lookup."""
    if network_exists(name):
        raise ContainerError(f'network {name} already exists')
    run(['network', 'create', '--subnet', NETWORK_SUBNET, '--gateway', NETWORK_GATEWAY, name], timeout=120, check=True)
    try:
        gateway = network_gateway(name)
        if gateway != NETWORK_GATEWAY:
            raise ContainerError(f'unexpected gateway {gateway} for {NETWORK_SUBNET}')
    except BaseException:
        remove_network(name)
        raise
    return gateway


def remove_network(name=NETWORK):
    """Remove only an empty network: --force would detach foreign containers."""
    if not network_exists(name):
        return
    proc = run(['network', 'rm', name], timeout=120)
    if proc.returncode != 0:
        raise ContainerError(f'podman network rm {name} failed: {proc.stderr.strip()[:300]}')


def cgroup_path(name):
    """Namespace-visible cgroup directory holding only this container's processes."""
    scope = field(name, '{{.State.CgroupPath}}')
    if not scope:
        return None
    scope_dir = CGROUP_ROOT / scope.lstrip('/')
    inner = scope_dir / 'container'
    if (inner / 'cpu.stat').exists():
        return inner
    if (scope_dir / 'cpu.stat').exists():
        return scope_dir
    return None


def start_backend(name, work, owner):
    args = [
        '--network', NETWORK,
        '--ip', BACKEND_IP,
        '--cpuset-cpus', BACKEND_CPUS,
        '--memory', '512m', '--memory-swap', '512m',
        '--pids-limit', '256',
        '--ulimit', 'nofile=4096:4096',
        '--stop-timeout', '35',
        '--read-only',
        '--cap-drop=ALL',
        '--security-opt=no-new-privileges',
        '--user', '10001:10001',
        '--tmpfs', '/tmp:rw,noexec,nosuid,size=16m',
        '--mount', f'type=bind,src={work}/bench,dst=/bench,ro',
        '--entrypoint', CLIENT_ENTRYPOINT,
        CLIENT_IMAGE,
        '/bench/backend.py', '--host', '0.0.0.0', '--port', str(BACKEND_PORT),
    ]
    return start_container(name, args, 'backend', owner)


def limit_args():
    return [
        '--memory', LIMITS['memory'],
        '--memory-swap', LIMITS['memory_swap'],
        '--pids-limit', LIMITS['pids_limit'],
        '--ulimit', f'nofile={LIMITS["nofile"]}:{LIMITS["nofile"]}',
        '--stop-timeout', LIMITS['stop_timeout'],
        '--read-only',
        '--cap-drop=ALL',
        '--security-opt=no-new-privileges',
        '--user', '10001:10001',
    ]


def container_id(name, allow_missing=True):
    value = field(name, '{{.Id}}', allow_missing=allow_missing)
    return value or None


def remove_id(identifier):
    """Remove by id: a name may already point at a different container."""
    if not identifier:
        return False
    proc = run(['rm', '--force', identifier], timeout=120)
    if proc.returncode == 0:
        return True
    if 'no such container' in proc.stderr.lower():
        return True
    raise ContainerError(f'podman rm --force {identifier} failed: {proc.stderr.strip()[:300]}')


def create_container(name, args):
    """Run the container and return (id, proc). The id is known before any inspection."""
    if exists(name):
        raise ContainerError(f'container {name} already exists')
    proc = run(['run', '-d', '--name', name, *args], timeout=180)
    # `podman run -d` prints the new container id; that id, not the name, is the
    # ownership token, so a same-named container created by someone else is safe.
    identifier = proc.stdout.strip().splitlines()[-1].strip() if proc.stdout.strip() else ''
    return identifier, proc


def start_container(name, args, description, owner):
    """Start a container, handing the id to its owner before any inspection.

    Every removal goes back through the owner so the outcome is recorded once,
    instead of an unreported force-removal followed by a second cleanup attempt.
    """
    identifier, proc = create_container(name, args)
    if identifier:
        owner.record(identifier)
    if proc.returncode != 0 or not identifier:
        detail = proc.stderr.strip()[:400]
        removed = owner.discard('podman run failed before the container was usable')
        raise ContainerError(f'podman run --name {name} failed: {detail}; partial container removed={removed}')
    if not running(name):
        exit_state = exit_code(name)
        removed = owner.discard(f'{description} exited during start (exit={exit_state})')
        raise ContainerError(f'{description} {name} exited during start (exit={exit_state}); removed={removed}')
    return identifier


def start(name, target, work, owner):
    """Create and start a target container. Returns the container id."""
    args = [
        '--network', NETWORK,
        '--publish', '127.0.0.1:18080:18080',
        '--publish', '127.0.0.1:18443:18443',
        '--cpuset-cpus', SERVER_CPUS,
    ]
    args += limit_args()
    args += ['--tmpfs', '/tmp:rw,noexec,nosuid,size=16m']
    for mount in target.mounts(work):
        args += ['--mount', mount]
    for key, value in target.env.items():
        args += ['--env', f'{key}={value}']
    args += ['--entrypoint', target.entrypoint, target.image]
    args += target.command
    return start_container(name, args, 'container', owner)


def stop_owned(name, identifier, timeout=90):
    """Stop and remove one container created by this run, keyed by id.

    Stop and removal are separate outcomes so a retry repeats only what is still
    unfinished, and a failed stop is never reported as a clean exit.
    """
    result = {'name': name, 'id': identifier, 'removed': False, 'stopped': False, 'errors': []}
    if not identifier:
        result['errors'].append('no container id')
        return result
    if container_id(name) not in (None, identifier):
        result['errors'].append('container name no longer refers to this run\'s container')
        return result
    try:
        if running(name):
            proc = run(['stop', '--time', LIMITS['stop_timeout'], identifier], timeout=timeout)
            result['stop_returncode'] = proc.returncode
            if proc.returncode != 0:
                result['errors'].append(f'podman stop failed: {proc.stderr.strip()[:300]}')
        result['stopped'] = not running(name)
    except ContainerError as exc:
        result['errors'].append(f'stop check failed: {exc}'[:300])
    result['exit_code'] = exit_code(identifier)
    result['forced'] = result['exit_code'] is not None and result['exit_code'] < 0
    try:
        result['removed'] = remove_id(identifier)
    except ContainerError as exc:
        result['errors'].append(f'removal failed: {exc}'[:300])
    if not result['removed'] and not result['errors']:
        result['errors'].append('container still present after removal')
    if not result['errors']:
        del result['errors']
    return result


async def exec_(name, argv, timeout=40, workdir=None):
    prefix = ['exec'] + (['-w', str(workdir)] if workdir else [])
    proc = await asyncio.create_subprocess_exec(
        PODMAN, *prefix, name, *argv,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout)
        return proc.returncode, stdout, stderr
    except BaseException:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
        raise


def one_shot(target, work, argv, timeout=180):
    """Run a short command with the same isolation, mounts and limits as the server."""
    args = ['run', '--rm', '--network', NETWORK] + limit_args()
    for mount in target.mounts(work):
        args += ['--mount', mount]
    for key, value in target.env.items():
        args += ['--env', f'{key}={value}']
    args += ['--entrypoint', target.entrypoint, target.image]
    args += argv
    return run(args, timeout=timeout)


def logs(name, tail=100):
    proc = run(['logs', '--tail', str(tail), name], timeout=60)
    return (proc.stdout + proc.stderr)[-20000:]


def residue(names):
    return [name for name in names if exists(name)]


def environment(name):
    raw = field(name, '{{json .}}')
    if not raw:
        return None
    data = json.loads(raw)
    return {
        'id': data.get('Id'),
        'image': data.get('ImageName'),
        'image_id': data.get('Image'),
        'cgroup': data.get('State', {}).get('CgroupPath'),
        'pid': data.get('State', {}).get('Pid'),
    }


class Container:
    """A container this run owns, identified by the id podman returned.

    Ownership is recorded before any inspection so a container that starts and
    then cannot be inspected is still cleaned up rather than leaked.
    """

    def __init__(self, name, work):
        self.name = name
        self.work = work
        self.identifier = None
        self.removed = False
        self.failure = None
        self.discard_record = None

    def record(self, identifier):
        self.identifier = identifier

    def start(self, target):
        return start(self.name, target, self.work, self)

    def start_backend(self):
        return start_backend(self.name, self.work, self)

    def stop(self):
        record = stop_owned(self.name, self.identifier)
        self.removed = bool(record.get('removed'))
        return record

    def discard(self, reason):
        """Remove a container this run created but could not use, recording the outcome."""
        if not self.identifier:
            return False
        try:
            removed = remove_id(self.identifier)
            error = None
        except ContainerError as exc:
            removed = False
            error = f'{type(exc).__name__}: {exc}'[:300]
        self.removed = removed
        self.failure = reason if not removed else None
        self.discard_record = {'name': self.name, 'id': self.identifier, 'removed': removed,
                               'discarded': reason, 'stopped': False, 'exit_code': None}
        if error:
            self.discard_record['errors'] = [error]
        return removed

    def running(self):
        return running(self.name)

    def logs(self, tail=40):
        return logs(self.name, tail)
