import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
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
            "-p", "127.0.0.1::50052",
            "-v", f"{binary.resolve()}:/acceptance:ro",
            "-v", f"{certs}:/certs:ro", "--entrypoint", "/acceptance", image,
            "backend", "/certs/server.pem", "/certs/server.key",
        ])
        command(["podman", "start", container_id])
        ports = {
            protocol: int(output(["podman", "port", container_id, port]).rsplit(":", 1)[1])
            for protocol, port in [("h2c", "50051/tcp"), ("tls-h2", "50052/tcp")]
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

        def proxy(label, protocol, ca="ca.pem", server_name="grpc.test"):
            port = unused_port()
            log = (work / f"{label}-proxy.log").open("w")
            logs.append(log)
            process = subprocess.Popen([
                str(binary), "proxy", f"127.0.0.1:{port}",
                f"127.0.0.1:{ports[protocol]}", protocol, str(certs / ca), server_name,
            ], stdout=log, stderr=log, start_new_session=True)
            processes.append(process)
            wait_port(port, process)
            return port

        client("direct-h2c", ports["h2c"])
        client("direct-tls-h2", ports["tls-h2"], tls=True)
        client("proxy-h2c", proxy("h2c", "h2c"))
        client("proxy-tls-h2", proxy("tls", "tls-h2"))
        client("reject-wrong-name", proxy("wrong-name", "tls-h2", server_name="wrong.test"), mode="reject")
        client("reject-wrong-ca", proxy("wrong-ca", "tls-h2", ca="other-ca.pem"), mode="reject")
        for label in ("wrong-name", "wrong-ca"):
            diagnostic = (work / f"{label}-proxy.log").read_text()
            if "Upstream InvalidCert" not in diagnostic:
                raise RuntimeError(f"{label} did not fail with a TLS certificate error")
        result["passed"] = True
    finally:
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
