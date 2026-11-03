import hashlib
import json
from pathlib import Path


def prepare(work, cert, key):
    root = work / 'www'
    root.mkdir(mode=0o700)
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
    control.mkdir(mode=0o700)
    for path in (work, cert, key):
        if any(c in str(path) for c in '\n\r"\\;{}'):
            raise ValueError('unsafe configuration path')
    for age, label in [(10, 'a'), (20, 'b')]:
        rill = f'''server {{ http "127.0.0.1:18080"; https "127.0.0.1:18443"; control-socket "{control}/control.sock"; log-level "warn"; }}
tls "default" {{ cert "{cert}"; key "{key}"; }}
site "bench.test" {{ tls "default";
route "/proxy" {{ proxy "http://127.0.0.1:19090"; read-timeout "120s"; idle-timeout "120s"; }}
route "/ws" {{ proxy "http://127.0.0.1:19090"; read-timeout "120s"; idle-timeout "120s"; }}
route "/events" {{ proxy "http://127.0.0.1:19090"; read-timeout "120s"; idle-timeout "120s"; }}
route "/*" {{ root "{root}"; cache-control "max-age={age}"; }} }}
'''
        nginx = f'''worker_processes 2;
daemon off;
pid {work}/nginx.pid;
error_log {work}/nginx-error.log warn;
events {{ worker_connections 1024; }}
http {{
 access_log off; sendfile on; open_file_cache off;
 client_body_temp_path {work}/client_temp;
 proxy_temp_path {work}/proxy_temp;
 fastcgi_temp_path {work}/fastcgi_temp;
 uwsgi_temp_path {work}/uwsgi_temp;
 scgi_temp_path {work}/scgi_temp;
 map $http_upgrade $connection_upgrade {{ default upgrade; '' ''; }}
 upstream backend {{ server 127.0.0.1:19090; keepalive 32; keepalive_timeout 120s; }}
 server {{
  listen 127.0.0.1:18080; listen 127.0.0.1:18443 ssl; server_name bench.test;
  ssl_certificate {cert}; ssl_certificate_key {key}; ssl_protocols TLSv1.2 TLSv1.3;
  ssl_session_cache off; ssl_session_tickets off;
  root {root}; add_header Cache-Control "max-age={age}";
  keepalive_timeout 120s;
  location ~ ^/(proxy|ws|events)$ {{
   proxy_pass http://backend; proxy_http_version 1.1;
   proxy_set_header Host $http_host; proxy_set_header Upgrade $http_upgrade;
   proxy_set_header Connection $connection_upgrade;
   proxy_buffering off; proxy_request_buffering off; proxy_connect_timeout 5s; proxy_send_timeout 120s; proxy_read_timeout 120s;
  }}
 }}
}}
'''
        (work / f'rill-{label}.kdl').write_text(rill)
        (work / f'nginx-{label}.conf').write_text(nginx)
    (work / 'rill-active.kdl').write_bytes((work / 'rill-a.kdl').read_bytes())
    (work / 'nginx-active.conf').write_bytes((work / 'nginx-a.conf').read_bytes())
    return hashes


def hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as src:
        for chunk in iter(lambda: src.read(65536), b''):
            digest.update(chunk)
    return digest.hexdigest()
