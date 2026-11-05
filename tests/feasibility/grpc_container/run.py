import argparse
import json
import http.client
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parents[3]
MANIFEST = Path(__file__).with_name("Cargo.toml")
IMAGE = "docker.io/library/debian:trixie-slim"


def command(args, **kwargs):
    print("$", " ".join(str(arg) for arg in args), flush=True)
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(args):
    return command(args, capture_output=True, text=True).stdout.strip()


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_port(port, process=None):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError("proxy exited before listening")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise TimeoutError(f"port {port} did not become ready")


def certificates(work):
    certs = work / "certs"
    certs.mkdir(mode=0o755)
    with (work / "openssl.log").open("w") as log:
        for prefix in ("ca", "other-ca"):
            command([
                "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                "-subj", f"/CN=Rill acceptance {prefix}",
                "-addext", "basicConstraints=critical,CA:TRUE",
                "-keyout", work / f"{prefix}.key", "-out", certs / f"{prefix}.pem",
            ], stdout=log, stderr=log)
        command([
            "openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=grpc.test",
            "-keyout", certs / "server.key", "-out", work / "server.csr",
        ], stdout=log, stderr=log)
        extensions = work / "server.ext"
        extensions.write_text(
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth\nsubjectAltName=DNS:grpc.test\n"
        )
        command([
            "openssl", "x509", "-req", "-in", work / "server.csr", "-days", "1",
            "-CA", certs / "ca.pem", "-CAkey", work / "ca.key", "-CAcreateserial",
            "-extfile", extensions, "-out", certs / "server.pem",
        ], stdout=log, stderr=log)
    (certs / "server.key").chmod(0o644)
    return certs


class Http1Backend:
    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.events = []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopped.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self.handle, args=(connection,), daemon=True).start()

    def handle(self, connection):
        with connection:
            connection.settimeout(4)
            data = b""
            try:
                while True:
                    chunk = connection.recv(4096)
                    if not chunk:
                        return
                    data += chunk
                    if data.startswith(b"PRI * HTTP/2.0"):
                        self.events.append("h2-new-accept")
                        return
                    while b"\r\n\r\n" in data:
                        request, data = data.split(b"\r\n\r\n", 1)
                        if not request.startswith(b"GET "):
                            return
                        self.events.append("h1-request")
                        connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            except (OSError, TimeoutError):
                return

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.thread.join(timeout=2)


def http1_poolcheck(port, backend):
    for policy, expected in [("plain-h1", 200), ("plain-h1", 200), ("plain-h2", 502), ("plain-h1", 200)]:
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        connection.request("GET", "/", headers={"x-pool-policy": policy})
        response = connection.getresponse()
        body = response.read()
        connection.close()
        if response.status != expected or (expected == 200 and body != b"ok"):
            raise RuntimeError(f"{policy}: unexpected response {response.status} {body!r}")
    if backend.events != ["h1-request", "h1-request", "h2-new-accept", "h1-request"]:
        raise RuntimeError(f"unexpected backend events: {backend.events}")
    print(f"PASS h1 to h2 isolation: {backend.events}", flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--image", default=IMAGE)
    args = parser.parse_args()
    image = output(["podman", "image", "inspect", args.image, "--format", "{{.Id}}"])
    target = ROOT / "target"
    environment = dict(os.environ, CARGO_TARGET_DIR=str(target.resolve()))
    command(["cargo", "build", "--locked", "--offline", "--manifest-path", MANIFEST],
            env=environment)
    binary = target / "debug/rill-grpc-acceptance"
    work = Path(tempfile.mkdtemp(prefix="rill-grpc-acceptance-", dir="/var/tmp"))
    print(f"artifacts={work}", flush=True)
    name = f"rill-grpc-acceptance-{work.name.removeprefix('rill-grpc-acceptance-')}"
    processes = []
    logs = []
    network_created = False
    container_id = None
    result = {"image": image, "scenarios": [], "cleanup": False}
    h1_backend = Http1Backend()
    try:
        certs = certificates(work)
        command(["podman", "network", "create", "--opt", "isolate=true", name])
        network_created = True
        container_id = output([
            "podman", "create", "--pull=never", "--name", name,
            "--network", name, "--read-only", "--cap-drop=all",
            "--security-opt=no-new-privileges", "--user", "65534:65534",
            "--memory", "256m", "--cpus", "1", "--pids-limit", "64",
            "--stop-signal", "SIGINT", "-p", "127.0.0.1::50051",
            "-p", "127.0.0.1::50052", "-p", "127.0.0.1::50053",
            "-v", f"{binary.resolve()}:/acceptance:ro",
            "-v", f"{certs}:/certs:ro", "--entrypoint", "/acceptance", image,
            "backend", "/certs/server.pem", "/certs/server.key",
        ])
        command(["podman", "start", container_id])
        ports = {
            protocol: int(output(["podman", "port", container_id, port]).rsplit(":", 1)[1])
            for protocol, port in [("h2c", "50051/tcp"), ("tls-h2", "50052/tcp"), ("h2c-two", "50053/tcp")]
        }
        result["ports"] = ports
        for port in ports.values():
            wait_port(port)

        def client(label, port, tls=False, mode="ok"):
            with (work / f"{label}.log").open("w") as log:
                command([binary, "client", f"{'https' if tls else 'http'}://127.0.0.1:{port}",
                         certs / "ca.pem", mode], stdout=log, stderr=log, timeout=50)
            print((work / f"{label}.log").read_text(), end="", flush=True)
            result["scenarios"].append(label)

        def proxy(label, protocol, ca="ca.pem", server_name="grpc.test", poolcheck=False):
            port = unused_port()
            log = (work / f"{label}-proxy.log").open("w")
            logs.append(log)
            extra = [str(certs / "other-ca.pem"), f"127.0.0.1:{ports['h2c']}",
                     f"127.0.0.1:{ports['h2c-two']}", f"127.0.0.1:{h1_backend.port}"] if poolcheck else []
            process = subprocess.Popen([
                str(binary), "proxy", f"127.0.0.1:{port}",
                f"127.0.0.1:{ports[protocol]}", protocol, str(certs / ca), server_name,
            ] + extra, stdout=log, stderr=log, start_new_session=True)
            processes.append(process)
            wait_port(port, process)
            return port

        client("direct-h2c", ports["h2c"])
        client("direct-tls-h2", ports["tls-h2"], tls=True)
        client("proxy-h2c", proxy("h2c", "h2c"))
        client("proxy-tls-h2", proxy("tls", "tls-h2"))
        client("reject-wrong-name", proxy("wrong-name", "tls-h2", server_name="wrong.test"), mode="reject")
        client("reject-wrong-ca", proxy("wrong-ca", "tls-h2", ca="other-ca.pem"), mode="reject")
        pool_port = proxy("pool", "tls-h2", poolcheck=True)
        client("poolcheck", pool_port, mode="poolcheck")
        http1_poolcheck(pool_port, h1_backend)
        result["scenarios"].append("h1-to-h2")
        pool_log = (work / "pool-proxy.log").read_text()
        for expected in ("POOL policy=good reused=false", "POOL policy=good reused=true", "Upstream TLSHandshakeFailure"):
            if expected not in pool_log:
                raise RuntimeError(f"missing connection evidence: {expected}")
        if pool_log.count("POOL policy=good reused=true") < 2:
            raise RuntimeError("good policy did not reuse the pool after rejection checks")
        if pool_log.count("Upstream InvalidCert") != 2:
            raise RuntimeError("wrong CA/name did not both fail certificate verification")
        for expected in ("POOL policy=target-one reused=false", "POOL policy=target-one reused=true",
                         "POOL policy=target-two reused=false", "POOL policy=plain-h1 reused=false",
                         "POOL policy=plain-h1 reused=true", "POOL policy=plain-h2 reused=false"):
            if expected not in pool_log:
                raise RuntimeError(f"missing connection evidence: {expected}")
        for policy in ("wrong-ca", "wrong-name", "h1"):
            if f"POOL policy={policy} " in pool_log:
                raise RuntimeError(f"{policy} unexpectedly established or reused a connection")
        mismatch = re.search(r"POOL_IDENTITY_MISMATCH policy=forced-collision reused=true fd=(\d+) expected=(\d+) actual=Some\((\d+)\)", pool_log)
        if not mismatch or mismatch.group(2) == mismatch.group(3):
            raise RuntimeError("forced collision did not hit the exact identity guard")
        before_collision, after_collision = pool_log.split("POOL_IDENTITY_MISMATCH policy=forced-collision", 1)
        good_before = re.findall(r"POOL policy=good reused=(true|false) fd=(\d+)", before_collision)
        if len(good_before) != 3 or good_before[0][0] != "false" or any(
                reused != "true" or fd != good_before[0][1] for reused, fd in good_before[1:]):
            raise RuntimeError(f"good policy did not reuse before collision: {good_before}")
        if mismatch.group(1) != good_before[0][1] or "POOL policy=good " not in after_collision:
            raise RuntimeError("collision did not target good fd or good did not recover")
        for policy in ("target-one", "plain-h1"):
            matches = re.findall(rf"POOL policy={policy} reused=(true|false) fd=(\d+)", pool_log)
            if len(matches) < 2 or matches[0][0] != "false" or any(
                    reused != "true" or fd != matches[0][1] for reused, fd in matches[1:]):
                raise RuntimeError(f"{policy} did not reuse its original connection: {matches}")
        plain_h1_fd = re.search(r"POOL policy=plain-h1 reused=false fd=(\d+)", pool_log).group(1)
        plain_h2_fd = re.search(r"POOL policy=plain-h2 reused=false fd=(\d+)", pool_log).group(1)
        if plain_h1_fd == plain_h2_fd:
            raise RuntimeError("h1 and h2 unexpectedly used the same live fd")
        for label in ("wrong-name", "wrong-ca"):
            diagnostic = (work / f"{label}-proxy.log").read_text()
            if "Upstream InvalidCert" not in diagnostic:
                raise RuntimeError(f"{label} did not fail with a TLS certificate error")
        result["passed"] = True
    finally:
        h1_backend.close()
        cleanup_errors = []
        for process in reversed(processes):
            try:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
            except ProcessLookupError:
                process.wait(timeout=5)
            except Exception as error:
                cleanup_errors.append(str(error))
        for log in logs:
            log.close()
        if container_id is not None:
            with (work / "backend.log").open("w") as log:
                subprocess.run(["podman", "logs", container_id], stdout=log, stderr=log, check=False)
            try:
                command(["podman", "rm", "--force", "--ignore", container_id])
            except subprocess.CalledProcessError as error:
                cleanup_errors.append(str(error))
        if network_created:
            try:
                command(["podman", "network", "rm", name])
            except subprocess.CalledProcessError as error:
                cleanup_errors.append(str(error))
        for path in ("ca.key", "other-ca.key", "server.csr", "certs/server.key"):
            try:
                (work / path).unlink(missing_ok=True)
            except OSError as error:
                cleanup_errors.append(str(error))
        result["cleanup"] = not cleanup_errors
        result["cleanup_errors"] = cleanup_errors
        (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2), flush=True)
        if cleanup_errors:
            raise RuntimeError("acceptance resources were not fully cleaned up")


if __name__ == "__main__":
    main()
