#!/usr/bin/env python3
import base64
import http.server
import os
import socket
import ssl
import struct
import sys
import subprocess
import tempfile
import threading
import time
import urllib.parse

if len(sys.argv) == 1:
    net_fd = os.open('/proc/self/ns/net', os.O_RDONLY)
    mount_fd = os.open('/proc/self/ns/mnt', os.O_RDONLY)
    try:
        result = subprocess.run(['unshare', '--net', '--mount', sys.executable, os.path.abspath(__file__), '--isolated', str(net_fd), str(mount_fd)], pass_fds=(net_fd, mount_fd))
        sys.exit(result.returncode)
    finally:
        os.close(net_fd)
        os.close(mount_fd)
if len(sys.argv) != 4 or sys.argv[1] != '--isolated':
    raise SystemExit('invalid namespace invocation')
for name, fd in [('net', int(sys.argv[2])), ('mnt', int(sys.argv[3]))]:
    parent = os.fstat(fd)
    current = os.stat('/proc/self/ns/' + name)
    if (parent.st_dev, parent.st_ino) == (current.st_dev, current.st_ino):
        raise SystemExit('network and mount namespaces must both differ from parent')
links = subprocess.check_output(['ip', '-o', 'link', 'show'], text=True).splitlines()
if len(links) != 1 or links[0].split(':', 2)[1].strip() != 'lo':
    raise SystemExit('unexpected network interface')
for family in ('-4', '-6'):
    if subprocess.check_output(['ip', family, 'route', 'show', 'table', 'all']).strip():
        raise SystemExit('unexpected network route')

subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
subprocess.run(['ip', 'addr', 'add', '8.8.8.8/32', 'dev', 'lo'], check=True)
assert not subprocess.check_output(['ip', 'route', 'show', 'default']).strip()
seen = []
paths = []
clients = []
connections = []
hold = threading.Event()
sse_hold = threading.Event()
ws_hold = threading.Event()
class Backend(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_GET(self):
        seen.append(dict(self.headers))
        paths.append(self.path)
        clients.append(self.client_address)
        connections.append(self.connection)
        if self.path == '/transform':
            body = b'old.example.test'
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == '/sse':
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Set-Cookie', 'dummy=sensitive')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(b'data: one\n\n')
            self.wfile.flush()
            sse_hold.wait(60)
            self.wfile.write(b'data: two\n\n')
            self.wfile.flush()
            self.close_connection = True
            return
        if self.path == '/ws':
            self.send_response(101)
            self.send_header('Connection', 'Upgrade')
            self.send_header('Upgrade', 'websocket')
            self.send_header('Sec-WebSocket-Accept', 's3pPLMBiTxaQ9kYGzzhZRbK+xOo=')
            self.send_header('Sec-WebSocket-Protocol', 'chat')
            self.send_header('Set-Cookie', 'dummy=sensitive')
            self.end_headers()
            for _ in range(2):
                frame = self.rfile.read(8)
                assert frame == bytes([0x81, 0x82, 1, 2, 3, 4, ord('h') ^ 1, ord('i') ^ 2])
                self.wfile.write(b'\x81\x02hi')
                self.wfile.flush()
            ws_hold.wait(60)
            self.close_connection = True
            return
        if self.path == '/drop':
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            self.close_connection = True
            return
        if self.path == '/hold':
            hold.wait(60)
        self.send_response(302)
        self.send_header('Location', {'/external': 'http://elsewhere.test/out', '/frag': '/next#section', '/absfrag': 'http://8.8.8.8:18080/next#section'}.get(self.path, '/next?x=1'))
        self.send_header('Set-Cookie', 'secret=blocked')
        self.send_header('Content-Length', '0')
        self.end_headers()
    def log_message(self, *_):
        pass
backend = http.server.ThreadingHTTPServer(('8.8.8.8', 18080), Backend)
threading.Thread(target=backend.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix='rill-dynamic-netns-') as directory:
    os.chmod(directory, 0o700)
    resolver = os.path.join(directory, 'resolv.conf')
    with open(resolver, 'w', encoding='ascii') as file:
        file.write('nameserver 127.0.0.1\noptions timeout:1 attempts:1\n')
    subprocess.run(['mount', '--make-rprivate', '/'], check=True)
    subprocess.run(['mount', '--bind', resolver, '/etc/resolv.conf'], check=True)
    key = os.path.join(directory, 'key.pem')
    cert = os.path.join(directory, 'cert.pem')
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert, '-days', '1', '-subj', '/CN=public.test', '-addext', 'subjectAltName=DNS:public.test'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    bad_key = os.path.join(directory, 'bad-key.pem')
    bad_cert = os.path.join(directory, 'bad-cert.pem')
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', bad_key, '-out', bad_cert, '-days', '1', '-subj', '/CN=public.test', '-addext', 'subjectAltName=DNS:public.test'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    bad_backend = http.server.ThreadingHTTPServer(('8.8.8.8', 18444), Backend)
    bad_tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    bad_tls.load_cert_chain(bad_cert, bad_key)
    bad_backend.socket = bad_tls.wrap_socket(bad_backend.socket, server_side=True)
    threading.Thread(target=bad_backend.serve_forever, daemon=True).start()
    tls_backend = http.server.ThreadingHTTPServer(('8.8.8.8', 18443), Backend)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    tls_backend.socket = tls.wrap_socket(tls_backend.socket, server_side=True)
    threading.Thread(target=tls_backend.serve_forever, daemon=True).start()
    stalled = socket.socket()
    stalled.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    stalled.bind(('8.8.8.8', 18445))
    stalled.listen(32)
    stalled.settimeout(.2)
    stalled_sockets = []
    stall_stop = threading.Event()
    def accept_stalled():
        while not stall_stop.is_set():
            try:
                conn, _ = stalled.accept()
                conn.settimeout(.2)
                stalled_sockets.append(conn)
            except TimeoutError:
                continue
    stall_thread = threading.Thread(target=accept_stalled, daemon=True)
    stall_thread.start()
    state = {'rebind': False}
    race_asked = threading.Event()
    race_continue = threading.Event()
    dns = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dns.bind(('127.0.0.1', 53))
    dns.settimeout(.2)
    stop = threading.Event()
    def serve_dns():
        while not stop.is_set():
            try:
                data, addr = dns.recvfrom(4096)
            except TimeoutError:
                continue
            offset = 12
            labels = []
            while data[offset]:
                size = data[offset]
                offset += 1
                labels.append(data[offset:offset + size].decode('ascii'))
                offset += size
            offset += 1
            qtype = struct.unpack('!H', data[offset:offset + 2])[0]
            question = data[12:offset + 4]
            name = '.'.join(labels)
            ips = []
            if qtype == 1:
                ips = {'public.test': ['8.8.8.8'], 'wrong.test': ['8.8.8.8'], 'race.test': ['8.8.8.9'], 'many.test': ['8.8.8.8'] * 65, 'mixed.test': ['8.8.8.8', '10.0.0.1'], 'rebind.test': ['10.0.0.1' if state['rebind'] else '8.8.8.8']}.get(name, [])
                if name.startswith('p') and name.endswith('.test') and name[1:-5].isdigit():
                    ips = ['8.8.8.8']
            if name == 'race.test' and qtype == 1:
                race_asked.set()
                race_continue.wait(3)
            answer = b''.join(b'\xc0\x0c' + struct.pack('!HHIH', 1, 1, 1, 4) + socket.inet_aton(ip) for ip in ips)
            packet = data[:2] + struct.pack('!HHHHH', 0x8180, 1, len(ips), 0, 0) + question + answer
            dns.sendto(packet, addr)
    dns_thread = threading.Thread(target=serve_dns, daemon=True)
    dns_thread.start()
    control = os.path.join(directory, 'control')
    os.mkdir(control, 0o700)
    token_path = os.path.join(directory, 'token')
    with open(token_path, 'w', encoding='ascii') as file:
        file.write('dummy-token')
    os.chmod(token_path, 0o600)
    config = os.path.join(directory, 'config.kdl')
    with open(config, 'w', encoding='utf-8') as file:
        file.write(f'server {{ http "0.0.0.0:18081"; control-socket "{control}/rill.sock"; }}\nsite "app.test" {{ chain {{ rule {{ dynamic-proxy prefix="/fetch/" anonymous=#true {{ allow-port 18080; allow-port 18081; allow-port 18443; allow-port 18444; allow-port 18445; }}; }}; }}; }}\nsite "auth.test" {{ chain {{ rule {{ static-token header="X-Token" file="{token_path}"; body-replace mime="text/html" {{ literal "old.example.test" "new.example.test"; }}; dynamic-proxy prefix="/fetch/" {{ allow-port 18080; }}; }}; }}; }}\nsite "basic.test" {{ chain {{ rule {{ match {{ method "POST"; }}; static-token header="X-Token" file="{token_path}"; continue; }}; rule {{ basic-auth realm="test" {{ user "dummy" hash="$2b$10$bZNofx165EOWoS0dTW8yNeqg/9IrzWZQz8FkIVCF6Tt6RMVxfIEHm"; }}; header "request" {{ set "Cookie" "dummy-readded"; }}; header "response" {{ set "Set-Cookie" "dummy-readded"; }}; dynamic-proxy prefix="/fetch/" {{ allow-port 18080; allow-port 18443; }}; }}; }}; }}\n')
    process = subprocess.Popen([os.path.abspath('target/debug/rill'), 'run', '--config', config], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env={**os.environ, 'SSL_CERT_FILE': cert, 'SSL_CERT_DIR': directory})
    try:
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError(process.stderr.read().decode())
            try:
                sock = socket.create_connection(('127.0.0.1', 18081), .1)
                sock.close()
                break
            except OSError:
                time.sleep(.02)
        else:
            raise RuntimeError('rill did not start')
        def request(target, host='app.test', token='bad', authorization='secret'):
            encoded = urllib.parse.quote(target, safe='')
            with socket.create_connection(('127.0.0.1', 18081), 3) as sock:
                sock.settimeout(60)
                sock.sendall(f'GET /fetch/{encoded} HTTP/1.1\r\nHost: {host}\r\nX-Token: {token}\r\nAuthorization: {authorization}\r\nProxy-Authorization: dummy\r\nCookie: secret=x\r\nForwarded: for=secret\r\nX-Forwarded-For: 1.2.3.4\r\nConnection: close\r\n\r\n'.encode())
                response = b''
                while chunk := sock.recv(65536):
                    response += chunk
                return response
        response = request('http://8.8.8.8:18080/test')
        assert response.startswith(b'HTTP/1.1 302'), response
        assert b'set-cookie:' not in response.lower(), response
        assert b'location: /fetch/http%3a%2f%2f8%2e8%2e8%2e8%3a18080%2fnext%3fx%3d1' in response.lower(), response
        assert len(seen) == 1
        assert not any(key.lower() in ('authorization', 'cookie', 'forwarded', 'x-forwarded-for') for key in seen[0]), seen
        assert seen[0]['host'] == '8.8.8.8:18080', seen
        response = request('http://8.8.8.8:18080/external')
        assert b'location: http://elsewhere.test/out' in response.lower(), response
        assert len(seen) == 2
        response = request('http://8.8.8.8:18080/path?one=1&two=2')
        assert response.startswith(b'HTTP/1.1 302') and paths[-1] == '/path?one=1&two=2', paths
        for path in ['/frag', '/absfrag']:
            response = request('http://8.8.8.8:18080' + path)
            location = next(line.split(b': ', 1)[1].decode() for line in response.split(b'\r\n') if line.lower().startswith(b'location: '))
            assert location.endswith('#section') and '%23' not in location.lower(), location
            with socket.create_connection(('127.0.0.1', 18081), 3) as client:
                client.sendall(f'GET {urllib.parse.urlsplit(location).path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n'.encode())
                client.settimeout(3)
                headers = b''
                while b'\r\n\r\n' not in headers:
                    chunk = client.recv(4096)
                    assert chunk and len(headers) + len(chunk) <= 65536
                    headers += chunk
                assert headers.startswith(b'HTTP/1.1 302'), headers
            assert paths[-1] == '/next'
        race_result = []
        racing = threading.Thread(target=lambda: race_result.append(request('http://race.test:18081/')))
        racing.start()
        assert race_asked.wait(3)
        subprocess.run(['ip', 'addr', 'add', '8.8.8.9/32', 'dev', 'lo'], check=True)
        race_continue.set()
        racing.join(timeout=5)
        assert race_result and race_result[0].startswith(b'HTTP/1.1 403'), race_result
        subprocess.run(['ip', 'addr', 'del', '8.8.8.9/32', 'dev', 'lo'], check=True)
        print('PASS post-DNS local-interface recheck and Location fragment roundtrip')
        response = request('http://8.8.8.8:18081/')
        assert response.startswith(b'HTTP/1.1 403'), response
        print('PASS dynamic HTTP forwarding, header isolation, relative Location and wildcard self denial')
        before = len(seen)
        response = request('http://8.8.8.8:18080/auth', 'auth.test', 'bad')
        assert response.startswith(b'HTTP/1.1 401') and len(seen) == before, response
        response = request('http://8.8.8.8:18080/auth', 'auth.test', 'dummy-token')
        assert response.startswith(b'HTTP/1.1 302') and len(seen) == before + 1, response
        assert not any(key.lower() in ('x-token', 'authorization', 'cookie') for key in seen[-1]), seen[-1]
        print('PASS token authentication and credential isolation')
        response = request('http://8.8.8.8:18080/transform', 'auth.test', 'dummy-token')
        assert response.startswith(b'HTTP/1.1 200') and b'new.example.test' in response and b'old.example.test' not in response, response
        print('PASS authenticated dynamic target and HTML replacement')
        basic = 'Basic ' + base64.b64encode(b'dummy:dummy-password').decode('ascii')
        before = len(seen)
        for auth in ['', 'Basic bad', basic + '\r\nAuthorization: ' + basic]:
            response = request('http://8.8.8.8:18080/basic', 'basic.test', authorization=auth)
            assert response.startswith(b'HTTP/1.1 401') and b'www-authenticate: basic ' in response.lower() and len(seen) == before, response
        for target in ['http://8.8.8.8:18080/basic', 'https://public.test:18443/basic']:
            response = request(target, 'basic.test', authorization=basic)
            assert response.startswith(b'HTTP/1.1 302') and b'set-cookie:' not in response.lower(), response
            assert not any(name.lower() in ('authorization', 'proxy-authorization', 'cookie', 'forwarded', 'x-forwarded-for', 'x-token') for name in seen[-1]), seen[-1]
        print('PASS Basic HTTP/HTTPS challenge, duplicate rejection and final header isolation')
        response = request('https://public.test:18443/test')
        assert response.startswith(b'HTTP/1.1 302'), response
        assert seen[-1]['host'] == 'public.test:18443', seen[-1]
        print('PASS dynamic HTTPS DNS and hostname verification')
        before = len(seen)
        response = request('https://wrong.test:18443/test')
        assert response.startswith(b'HTTP/1.1 502'), response
        assert len(seen) == before
        response = request('https://public.test:18444/test')
        assert response.startswith(b'HTTP/1.1 502'), response
        assert len(seen) == before
        print('PASS HTTPS bad CA and hostname mismatch before business request')
        def begin_stalled():
            sock = socket.create_connection(('127.0.0.1', 18081), 3)
            target = urllib.parse.quote('https://public.test:18445/', safe='')
            sock.sendall(f'GET /fetch/{target} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n'.encode())
            return sock
        pending = []
        for index in range(8):
            pending.append(begin_stalled())
            deadline = time.monotonic() + 3
            while len(stalled_sockets) < index + 1 and time.monotonic() < deadline:
                time.sleep(.01)
            assert len(stalled_sockets) == index + 1, len(stalled_sockets)
        for conn in stalled_sockets[:8]:
            assert conn.recv(4096), 'missing TLS ClientHello'
        ninth = request('https://public.test:18445/')
        assert ninth.startswith(b'HTTP/1.1 503') and len(stalled_sockets) == 8, ninth
        pending[0].close()
        time.sleep(.15)
        try:
            closed = stalled_sockets[0].recv(1) == b''
        except TimeoutError:
            closed = False
        if not closed:
            ninth = request('https://public.test:18445/')
            assert ninth.startswith(b'HTTP/1.1 503') and len(stalled_sockets) == 8, ninth
        for conn in stalled_sockets[:8]:
            conn.close()
        for sock in pending[1:]:
            sock.close()
        time.sleep(.3)
        probe = begin_stalled()
        deadline = time.monotonic() + 3
        while len(stalled_sockets) < 9 and time.monotonic() < deadline:
            time.sleep(.01)
        assert len(stalled_sockets) == 9, len(stalled_sockets)
        probe.close()
        stalled_sockets[-1].close()
        print('PASS handshake pre-reservations, cancellation and release after real socket closure')
        before = len(seen)
        response = request('http://mixed.test:18080/')
        assert response.startswith(b'HTTP/1.1 403'), response
        assert len(seen) == before
        response = request('http://rebind.test:18080/')
        assert response.startswith(b'HTTP/1.1 302'), response
        before = len(seen)
        state['rebind'] = True
        response = request('http://rebind.test:18080/')
        assert response.startswith(b'HTTP/1.1 403'), response
        assert len(seen) == before
        response = request('http://many.test:18080/')
        assert response.startswith(b'HTTP/1.1 503'), response
        print('PASS mixed DNS, 65-candidate capacity and public-to-private rebinding denial')
        assert request('http://p50.test:18080/one').startswith(b'HTTP/1.1 302')
        first = clients[-1]
        assert request('http://p50.test:18080/two').startswith(b'HTTP/1.1 302')
        assert clients[-1] == first, (first, clients[-1])
        assert request('http://p51.test:18080/two').startswith(b'HTTP/1.1 302')
        assert clients[-1] != first, (first, clients[-1])
        print('PASS same origin connection reuse and cross-origin isolation')
        hold.clear()
        before = len(seen)
        idle_results = []
        idle_workers = [threading.Thread(target=lambda: idle_results.append(request('https://public.test:18443/hold'))) for _ in range(8)]
        for worker in idle_workers:
            worker.start()
        deadline = time.monotonic() + 5
        while len(seen) < before + 8 and time.monotonic() < deadline:
            time.sleep(.01)
        assert len(seen) == before + 8 and len(set(clients[before:before + 8])) == 8, clients[before:]
        idle_clients = set(clients[before:before + 8])
        idle_sockets = connections[before:before + 8]
        hold.set()
        for worker in idle_workers:
            worker.join(timeout=5)
        assert len(idle_results) == 8
        assert request('https://public.test:18443/reuse').startswith(b'HTTP/1.1 302')
        assert clients[-1] in idle_clients
        with open(bad_cert, 'rb') as file:
            extra_ca = file.read()
        with open(cert, 'ab') as file:
            file.write(extra_ca)
        subprocess.run([os.path.abspath('target/debug/rill'), 'reload', '--socket', os.path.join(control, 'rill.sock')], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        before = len(seen)
        assert request('https://public.test:18443/new-trust').startswith(b'HTTP/1.1 503')
        assert len(seen) == before
        idle_sockets[0].shutdown(socket.SHUT_RDWR)
        idle_sockets[0].close()
        deadline = time.monotonic() + 5
        while True:
            response = request('https://public.test:18443/new-trust')
            if response.startswith(b'HTTP/1.1 302'):
                break
            assert response.startswith(b'HTTP/1.1 503'), response
            assert time.monotonic() < deadline
            time.sleep(.1)
        assert clients[-1] not in idle_clients
        print('PASS eight observed idle sockets, reuse, changed CA across reload, death recovery')
        hold.clear()
        before = len(seen)
        results = []
        workers = [threading.Thread(target=lambda: results.append(request('http://8.8.8.8:18080/hold'))) for _ in range(8)]
        for worker in workers:
            worker.start()
        deadline = time.monotonic() + 5
        while len(seen) < before + 8 and time.monotonic() < deadline:
            time.sleep(.01)
        assert len(seen) == before + 8, len(seen)
        with open(config, encoding='utf-8') as file:
            valid_config = file.read()
        with open(config, 'w', encoding='utf-8') as file:
            file.write(valid_config + '\ninvalid-node;\n')
        failed = subprocess.run([os.path.abspath('target/debug/rill'), 'reload', '--socket', os.path.join(control, 'rill.sock')], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        assert failed.returncode != 0
        assert request('http://8.8.8.8:18080/hold').startswith(b'HTTP/1.1 503')
        with open(config, 'w', encoding='utf-8') as file:
            file.write(valid_config)
        subprocess.run([os.path.abspath('target/debug/rill'), 'reload', '--socket', os.path.join(control, 'rill.sock')], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        response = request('http://8.8.8.8:18080/hold')
        assert response.startswith(b'HTTP/1.1 503'), response
        hold.set()
        for worker in workers:
            worker.join(timeout=5)
        assert len(results) == 8 and all(r.startswith(b'HTTP/1.1 302') for r in results), results
        assert request('http://8.8.8.8:18080/reuse').startswith(b'HTTP/1.1 302')
        print('PASS failed and successful reload preserve eight old requests, ninth denied, idle reuse')
        def open_stream(path, websocket=False):
            sock = socket.create_connection(('127.0.0.1', 18081), 3)
            sock.settimeout(20)
            target = urllib.parse.quote('http://8.8.8.8:18080/' + path, safe='')
            extra = 'Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat\r\n' if websocket else 'Connection: close\r\nAccept: text/event-stream\r\n'
            sock.sendall(f'GET /fetch/{target} HTTP/1.1\r\nHost: app.test\r\nAuthorization: dummy\r\nCookie: dummy\r\nForwarded: for=dummy\r\n{extra}\r\n'.encode())
            header = b''
            while not header.endswith(b'\r\n\r\n'):
                chunk = sock.recv(1)
                assert chunk
                header += chunk
            assert b'set-cookie:' not in header.lower(), header
            assert not any(n.lower() in ('authorization', 'cookie', 'forwarded') for n in seen[-1]), seen[-1]
            return sock, header
        sse, header = open_stream('sse')
        assert header.startswith(b'HTTP/1.1 200'), header
        first_sse = b''
        while b'data: one\n\n' not in first_sse:
            chunk = sse.recv(1)
            assert chunk
            first_sse += chunk
        ws, header = open_stream('ws', True)
        assert header.startswith(b'HTTP/1.1 101') and b'sec-websocket-protocol: chat' in header.lower(), header
        def ws_roundtrip():
            ws.sendall(bytes([0x81, 0x82, 1, 2, 3, 4, ord('h') ^ 1, ord('i') ^ 2]))
            frame = b''
            while len(frame) < 4:
                chunk = ws.recv(4 - len(frame))
                assert chunk
                frame += chunk
            assert frame == b'\x81\x02hi', frame
        ws_roundtrip()
        with open(config, encoding='utf-8') as file:
            stream_config = file.read()
        with open(config, 'w', encoding='utf-8') as file:
            file.write(stream_config.replace('prefix="/fetch/" anonymous=#true', 'prefix="/other/" anonymous=#true', 1))
        subprocess.run([os.path.abspath('target/debug/rill'), 'reload', '--socket', os.path.join(control, 'rill.sock')], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        assert request('http://p99.test:18080/after-reload').startswith(b'HTTP/1.1 400')
        ws_roundtrip()
        with open(config, 'w', encoding='utf-8') as file:
            file.write(stream_config)
        subprocess.run([os.path.abspath('target/debug/rill'), 'reload', '--socket', os.path.join(control, 'rill.sock')], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        hold.clear()
        before = len(seen)
        results = []
        workers = []
        for index in range(126):
            name = f'p{index // 8}.test'
            worker = threading.Thread(target=lambda name=name: results.append(request(f'http://{name}:18080/hold')))
            worker.start()
            workers.append(worker)
            deadline = time.monotonic() + 5
            while len(seen) < before + index + 1 and time.monotonic() < deadline:
                time.sleep(.005)
            assert len(seen) >= before + index + 1, (index, len(results))
        response = request('http://p16.test:18080/hold')
        assert response.startswith(b'HTTP/1.1 503'), response
        hold.set()
        for worker in workers:
            worker.join(timeout=10)
        assert len(results) == 126 and all(r.startswith(b'HTTP/1.1 302') for r in results), len(results)
        sse_hold.set()
        second_sse = b''
        while b'data: two' not in second_sse:
            chunk = sse.recv(256)
            assert chunk
            second_sse += chunk
        sse.close()
        ws_hold.set()
        ws.close()
        print('PASS SSE and WebSocket across reload and held active capacity')
        print('PASS 128 active requests and immediate capacity denial')
        before = len(paths)
        response = request('http://8.8.8.8:18080/drop')
        assert response.startswith(b'HTTP/1.1 502'), response
        time.sleep(.5)
        assert paths[before:].count('/drop') == 1, paths[before:]
        print('PASS no replay after upstream receives business request')
    finally:
        hold.set()
        sse_hold.set()
        ws_hold.set()
        race_continue.set()
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        stop.set()
        dns_thread.join(timeout=2)
        dns.close()
        stall_stop.set()
        stall_thread.join(timeout=2)
        for conn in stalled_sockets:
            conn.close()
        stalled.close()
        bad_backend.shutdown()
        bad_backend.server_close()
        tls_backend.shutdown()
        tls_backend.server_close()
        subprocess.run(['umount', '/etc/resolv.conf'], check=True)
        backend.shutdown()
        backend.server_close()
