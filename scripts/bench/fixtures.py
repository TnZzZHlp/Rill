import hashlib
import json
import os
from pathlib import Path

CERT_DIR = 'certs'
BACKEND_IP = '172.30.0.2'
BACKEND_PORT = 19090
BACKEND_HOST = f'{BACKEND_IP}:{BACKEND_PORT}'
SERVICE_UID = 10001


def prepare(work, cert, key, ca):
    work.mkdir(mode=0o711, parents=True, exist_ok=True)
    work.chmod(0o711)
    root = work / 'www'
    root.mkdir(mode=0o755)
    root.chmod(0o755)
    hashes = {}
    for name, byte, blocks in [('small.bin', b's', 1), ('large.bin', b'l', 256)]:
        block = byte * (1024 if name == 'small.bin' else 65536)
        digest = hashlib.sha256()
        with (root / name).open('xb') as out:
            for _ in range(blocks):
                out.write(block)
                digest.update(block)
        hashes[name] = digest.hexdigest()
    control = work / 'control'
    control.mkdir(mode=0o700, exist_ok=True)
    control.chmod(0o700)
    certs = work / CERT_DIR
    certs.mkdir(mode=0o711, exist_ok=True)
    certs.chmod(0o711)
    for name, source in (('cert.pem', cert), ('key.pem', key), ('ca.pem', ca)):
        path = certs / name
        path.write_bytes(Path(source).read_bytes())
        # The private key is readable only by the service identity, never world readable.
        if name == 'key.pem':
            path.chmod(0o600)
            own(path)
        else:
            path.chmod(0o644)
    for path in (work, cert, key):
        if any(c in str(path) for c in '\n\r"\\;{}'):
            raise ValueError('unsafe configuration path')
    for age, label in [(10, 'a'), (20, 'b')]:
        for name, text in ((f'rill-{label}.kdl', rill(age, work)),
                           (f'nginx-{label}.conf', nginx(age, work)),
                           (f'caddy-{label}.Caddyfile', caddy(age))):
            path = work / name
            path.write_text(text)
            path.chmod(0o644)
    return hashes


def rill(age, work):
    return f'''server {{ http "0.0.0.0:18080"; https "0.0.0.0:18443"; control-socket "/run/rill/control.sock"; log-level "warn"; }}
tls "default" {{ cert "/work/{CERT_DIR}/cert.pem"; key "/work/{CERT_DIR}/key.pem"; }}
site "bench.test" {{ tls "default";
route "/proxy" {{ proxy "http://{BACKEND_HOST}"; read-timeout "120s"; idle-timeout "120s"; }}
route "/ws" {{ proxy "http://{BACKEND_HOST}"; read-timeout "120s"; idle-timeout "120s"; }}
route "/events" {{ proxy "http://{BACKEND_HOST}"; read-timeout "120s"; idle-timeout "120s"; }}
route "/*" {{ root "/work/www"; cache-control "max-age={age}"; }} }}
'''


def nginx(age, work):
    return f'''worker_processes 2;
daemon off;
pid /work/nginx-temp/nginx.pid;
error_log stderr warn;
events {{ worker_connections 1024; }}
http {{
 access_log off; sendfile on; open_file_cache off;
 client_body_temp_path /work/nginx-temp/client;
 proxy_temp_path /work/nginx-temp/proxy;
 fastcgi_temp_path /work/nginx-temp/fastcgi;
 uwsgi_temp_path /work/nginx-temp/uwsgi;
 scgi_temp_path /work/nginx-temp/scgi;
 map $http_upgrade $connection_upgrade {{ default upgrade; '' ''; }}
 upstream bench_backend {{ server {BACKEND_HOST}; keepalive 32; keepalive_timeout 120s; }}
 server {{
  listen 0.0.0.0:18080; listen 0.0.0.0:18443 ssl; server_name bench.test;
  ssl_certificate /work/{CERT_DIR}/cert.pem; ssl_certificate_key /work/{CERT_DIR}/key.pem;
  ssl_protocols TLSv1.2 TLSv1.3;
  ssl_session_cache off; ssl_session_tickets off;
  root /work/www; add_header Cache-Control "max-age={age}";
  keepalive_timeout 120s;
  location ~ ^/(proxy|ws|events)$ {{
   proxy_pass http://bench_backend; proxy_http_version 1.1;
   proxy_set_header Host $http_host; proxy_set_header Upgrade $http_upgrade;
   proxy_set_header Connection $connection_upgrade;
   proxy_buffering off; proxy_request_buffering off;
   proxy_connect_timeout 5s; proxy_send_timeout 120s; proxy_read_timeout 120s;
  }}
 }}
}}
'''


CADDY_TEMPLATE = """{
	admin 127.0.0.1:2019
	auto_https off
	grace_period 30s
	servers {
		protocols h1
	}
}

(routes) {
	@upstreams path /proxy /ws /events
	handle @upstreams {
		reverse_proxy http://BACKEND {
			transport http {
				keepalive 120s
				keepalive_idle_conns 32
			}
		}
	}
	handle {
		root * /work/www
		header Cache-Control "max-age=AGE"
		file_server
	}
}

http://:18080 {
	import routes
}

https://bench.test:18443 {
	tls /work/CERTS/cert.pem /work/CERTS/key.pem
	import routes
}
"""


def caddy(age):
    """Global options first, then a snippet used by separate HTTP and HTTPS sites.

    Separate site blocks keep the ``tls`` directive from leaking onto the
    cleartext listener, which would otherwise answer plaintext with a redirect.
    """
    return (CADDY_TEMPLATE.replace('BACKEND', BACKEND_HOST)
            .replace('AGE', str(age)).replace('CERTS', CERT_DIR))


def caddy_native(adapted):
    """Add the settings that only exist in native JSON to an adapted configuration."""
    native = json.loads(adapted)
    tls = native['apps'].setdefault('tls', {})
    tls['session_tickets'] = {'disabled': True}
    return json.dumps(native, indent=2)


TEMP_DIRS = ('client', 'proxy', 'fastcgi', 'uwsgi', 'scgi')


def own(path):
    """Fixture files must belong to the container's service identity.

    Changing ownership needs privilege; running unprivileged would leave the 0600
    key owned by the invoking user, which the container identity cannot read. That
    is rejected up front rather than producing a confusing start failure.
    """
    if os.geteuid() != 0:
        raise RuntimeError('fixture ownership requires root; rootless runs are not supported by this harness')
    os.chown(path, SERVICE_UID, SERVICE_UID)


def publish_active(work):
    """Seed the writable active configs from the A templates before first start."""
    active = work / 'active'
    active.mkdir(mode=0o700, exist_ok=True)
    own(active)
    for name, suffix in (('rill', 'kdl'), ('nginx', 'conf')):
        path = active / f'{name}-active.{suffix}'
        path.write_bytes((work / f'{name}-a.{suffix}').read_bytes())
        path.chmod(0o644)
        own(path)


def prepare_runtime(work):
    """Writable paths each target needs; all under /work so a read-only root suffices."""
    owned = []
    for parent in ('nginx-temp', 'caddy'):
        path = work / parent
        path.mkdir(parents=True, exist_ok=True)
        owned.append(path)
    for name in TEMP_DIRS:
        path = work / 'nginx-temp' / name
        path.mkdir(parents=True, exist_ok=True)
        owned.append(path)
    for name in ('config', 'data'):
        path = work / 'caddy' / name
        path.mkdir(parents=True, exist_ok=True)
        owned.append(path)
    control = work / 'control'
    control.mkdir(mode=0o700, exist_ok=True)
    owned.append(control)
    active = work / 'active'
    active.mkdir(mode=0o700, exist_ok=True)
    owned.append(active)
    for path in owned:
        path.chmod(0o700)
        own(path)


def hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as src:
        for block in iter(lambda: src.read(1048576), b''):
            digest.update(block)
    return digest.hexdigest()


def adapt(target, work, label):
    """Adapt one Caddyfile inside the pinned image and write the native JSON."""
    import container
    source = work / f'caddy-{label}.Caddyfile'
    proc = container.one_shot(target, work, ['adapt', '--config', f'/work/{source.name}', '--adapter', 'caddyfile'], timeout=120)
    if proc.returncode != 0:
        raise ValueError(f'caddy adapt failed for {source.name}: {proc.stderr.strip()[:400]}')
    native = json.loads(proc.stdout)
    path = work / f'caddy-{label}.json'
    path.write_text(caddy_native(proc.stdout))
    path.chmod(0o644)
    return native


def prepare_caddy(target, work):
    for label in ('a', 'b'):
        adapt(target, work, label)
    for name in ('caddy-a.json', 'caddy-b.json', 'active/caddy-active.json'):
        path = work / name
        if name.startswith('active/'):
            path.write_bytes((work / 'caddy-a.json').read_bytes())
        path.chmod(0o644)
        own(path)
    publish_active(work)


def verify(work, names=('rill', 'nginx', 'caddy')):
    """Static guarantees re-checked after generation, before any container starts."""
    problems = []
    for label, age in (('a', 10), ('b', 20)):
        exact = f'max-age={age}'
        if 'rill' in names:
            kdl = (work / f'rill-{label}.kdl').read_text()
            if exact not in kdl:
                problems.append(f'rill-{label}: missing {exact}')
        if 'nginx' in names:
            conf = (work / f'nginx-{label}.conf').read_text()
            if exact not in conf:
                problems.append(f'nginx-{label}: missing {exact}')
            if 'ssl_session_cache off' not in conf or 'ssl_session_tickets off' not in conf:
                problems.append('nginx: tls session reuse not disabled')
        if 'caddy' not in names:
            continue
        native = json.loads((work / f'caddy-{label}.json').read_text())
        if exact not in json.dumps(native):
            problems.append(f'caddy-{label}: missing {exact}')
        if native['apps']['tls'].get('session_tickets', {}).get('disabled') is not True:
            problems.append('caddy: session tickets not disabled')
        certificates = native['apps']['tls'].get('certificates', {}).get('load_files', [])
        if not certificates or certificates[0].get('certificate') != f'/work/{CERT_DIR}/cert.pem':
            problems.append('caddy: certificate path')
        servers = native['apps']['http']['servers']
        if any(server.get('automatic_https', {}).get('disable') is not True for server in servers.values()):
            problems.append('caddy: automatic https enabled')
        if any(server.get('protocols') != ['h1'] for server in servers.values()):
            problems.append('caddy: protocols not h1')
        if BACKEND_HOST not in json.dumps(native):
            problems.append('caddy: backend upstream missing')
        if not any(age_value == [exact] for server in servers.values() for age_value in _header_values(server)):
            problems.append('caddy: cache-control header not set on a response')
    return problems


def _header_values(server):
    values = []
    for route in server.get('routes', []):
        for handler in route.get('handle', []):
            for inner in handler.get('routes', []) if handler.get('handler') == 'subroute' else []:
                for member in inner.get('handle', []):
                    if member.get('handler') == 'headers':
                        values.append(member.get('response', {}).get('set', {}).get('Cache-Control'))
    return values


def hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as src:
        for block in iter(lambda: src.read(1048576), b''):
            digest.update(block)
    return digest.hexdigest()
