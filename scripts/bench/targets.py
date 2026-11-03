"""Per-target description: image, launch command, mounts, reload and check.

The harness never branches on a target name outside this module.
"""

import container

CONTROL = '/run/rill'


class Target:
    name = ''
    image = ''
    entrypoint = ''
    command = ()
    config_suffix = ''
    env = {}
    writable = ()
    needs_control = False

    def mounts(self, work):
        """Fixture tree is read-only; only the declared writable paths are mounted back."""
        base = [f'type=bind,src={work},dst=/work,ro']
        for name in ('active',) + tuple(self.writable):
            base.append(f'type=bind,src={work / name},dst=/work/{name},rw')
        if self.needs_control:
            base.append(f'type=bind,src={work}/control,dst={CONTROL},rw')
        return base

    def active(self, work):
        return work / 'active' / f'{self.name}-active.{self.config_suffix}'

    def template(self, work, label):
        return work / f'{self.name}-{label}.{self.config_suffix}'

    def check(self, work, config, server_name, in_container):
        raise NotImplementedError

    def reload(self, work, config, server_name):
        raise NotImplementedError

    def version_argv(self):
        return ()


class Rill(Target):
    name = 'rill'
    image = 'localhost/rill:bench'
    entrypoint = '/usr/local/bin/rill'
    command = ('run', '--config', '/work/active/rill-active.kdl')
    config_suffix = 'kdl'
    writable = ()
    needs_control = True

    def check(self, work, config, server_name, in_container):
        return container.exec_(server_name, [self.entrypoint, 'check', '--config', in_container])

    def reload_argv(self, work):
        return [self.entrypoint, 'reload', '--socket', f'{CONTROL}/control.sock', '--timeout', '35s']

    def reload(self, work, config, server_name):
        return container.exec_(server_name, self.reload_argv(work), timeout=40)

    def version_argv(self):
        return (self.entrypoint, '--version')


class Nginx(Target):
    name = 'nginx'
    image = 'docker.io/library/nginx:stable-alpine'
    entrypoint = '/usr/sbin/nginx'
    config_suffix = 'conf'
    command = ('-e', 'stderr', '-p', '/work/', '-c', '/work/active/nginx-active.conf')
    writable = ('nginx-temp',)

    def check(self, work, config, server_name, in_container):
        return container.exec_(server_name, [self.entrypoint, '-t', '-p', '/work/', '-c', in_container])

    async def reload(self, work, config, server_name):
        rc, stdout, stderr = await self.check(work, config, server_name, f'/work/active/{config.name}')
        if rc != 0:
            return rc, stdout, stderr
        proc = container.run(['kill', '--signal', 'HUP', server_name], timeout=30)
        return proc.returncode, proc.stdout.encode(), proc.stderr.encode()

    def version_argv(self):
        return (self.entrypoint, '-v')


class Caddy(Target):
    name = 'caddy'
    image = 'localhost/caddy:bench'
    entrypoint = '/usr/bin/caddy'
    config_suffix = 'json'
    command = ('run', '--config', '/work/active/caddy-active.json')
    writable = ('caddy',)
    # GOMAXPROCS is set from the cpuset the container actually gets, so Caddy does not
    # size its own thread pool from the host's 12 CPUs while the other targets see 2.
    env = {'XDG_CONFIG_HOME': '/work/caddy/config', 'XDG_DATA_HOME': '/work/caddy/data',
           'GOMAXPROCS': str(len(container.SERVER_CPUS.split(',')))}

    def check(self, work, config, server_name, in_container):
        return container.exec_(server_name, [self.entrypoint, 'validate', '--config', in_container])

    def reload(self, work, config, server_name):
        return container.exec_(server_name, [self.entrypoint, 'reload', '--config', f'/work/active/{config.name}'], timeout=40)

    def version_argv(self):
        return (self.entrypoint, 'version')


TARGETS = {target.name: target for target in (Rill(), Nginx(), Caddy())}
ORDER = ('rill', 'nginx', 'caddy')


def get(name):
    try:
        return TARGETS[name]
    except KeyError as exc:
        raise ValueError(f'unknown target {name}') from exc


DISCLOSURES = {
    'parallelism': {
        'rill': 'tokio worker_threads = min(available_parallelism, 8); container cpuset has 2 CPUs',
        'nginx': 'worker_processes 2 (explicit), sendfile on',
        'caddy': 'GOMAXPROCS set to the cpuset size (2)',
    },
    'image_differences': {
        'rill': 'debian trixie-slim (glibc) + vendored OpenSSL build of pingora',
        'nginx': 'alpine (musl), openssl from the base image',
        'caddy': 'alpine (musl), Go runtime with GC',
    },
    'note': 'worker counts and TLS library versions differ by design; all three receive the same cpuset, memory, pids and nofile limits and the same network path',
}
