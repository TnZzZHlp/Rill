import argparse
import asyncio
import base64
import binascii
import hashlib
import signal
import struct
import sys


HEADER_LIMIT = 65536
FRAME_LIMIT = 65536
IDLE_TIMEOUT = 120
BODY = b"p" * 1024
GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class BadRequest(Exception):
    pass


class BadFrame(Exception):
    pass


async def read_exact(reader, size):
    return await asyncio.wait_for(reader.readexactly(size), IDLE_TIMEOUT)


async def write(writer, data):
    writer.write(data)
    await asyncio.wait_for(writer.drain(), IDLE_TIMEOUT)


async def response(writer, status, body=b"", close=True, extra=b""):
    connection = b"close" if close else b"keep-alive"
    await write(
        writer,
        b"HTTP/1.1 " + status + b"\r\n"
        + b"Content-Length: " + str(len(body)).encode() + b"\r\n"
        + b"Connection: " + connection + b"\r\n"
        + extra + b"\r\n" + body,
    )


async def request(reader):
    try:
        raw = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
    except asyncio.LimitOverrunError as exc:
        raise BadRequest from exc
    if len(raw) > HEADER_LIMIT:
        raise BadRequest
    lines = raw[:-4].split(b"\r\n")
    parts = lines[0].split(b" ")
    if len(parts) != 3 or not all(parts) or parts[2] != b"HTTP/1.1":
        raise BadRequest
    method, target, _ = parts
    headers = {}
    for line in lines[1:]:
        if b":" not in line or line[:1] in (b" ", b"\t"):
            raise BadRequest
        name, value = line.split(b":", 1)
        if not name or any(
            c not in b"!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
            for c in name
        ):
            raise BadRequest
        value = value.strip(b" \t")
        if any(c < 32 and c != 9 or c == 127 for c in value):
            raise BadRequest
        headers.setdefault(name.lower(), []).append(value)
    if len(headers.get(b"host", [])) != 1 or not headers[b"host"][0]:
        raise BadRequest
    if b"transfer-encoding" in headers:
        raise BadRequest
    lengths = headers.get(b"content-length", [])
    if len(lengths) > 1 or lengths and (
        not lengths[0] or any(c < 48 or c > 57 for c in lengths[0])
        or any(c != 48 for c in lengths[0])
    ):
        raise BadRequest
    if b"expect" in headers:
        raise BadRequest
    return method, target.split(b"?", 1)[0], headers


def tokens(headers, name):
    return {
        token.strip().lower()
        for value in headers.get(name, [])
        for token in value.split(b",")
        if token.strip()
    }


def websocket_accept(headers):
    if b"upgrade" not in tokens(headers, b"connection"):
        raise BadRequest
    if tokens(headers, b"upgrade") != {b"websocket"}:
        raise BadRequest
    if headers.get(b"sec-websocket-version") != [b"13"]:
        raise BadRequest
    keys = headers.get(b"sec-websocket-key", [])
    if len(keys) != 1:
        raise BadRequest
    try:
        decoded = base64.b64decode(keys[0], validate=True)
    except (binascii.Error, ValueError) as exc:
        raise BadRequest from exc
    if len(decoded) != 16 or base64.b64encode(decoded) != keys[0]:
        raise BadRequest
    return base64.b64encode(hashlib.sha1(keys[0] + GUID).digest())


async def read_frame(reader):
    first, second = await read_exact(reader, 2)
    opcode = first & 15
    if first & 0x70 or not first & 0x80 or not second & 0x80:
        raise BadFrame
    if opcode not in (1, 2, 8, 9, 10):
        raise BadFrame
    length = second & 127
    if length == 126:
        length = struct.unpack("!H", await read_exact(reader, 2))[0]
        if length < 126:
            raise BadFrame
    elif length == 127:
        length = struct.unpack("!Q", await read_exact(reader, 8))[0]
        if length < 65536 or length & (1 << 63):
            raise BadFrame
    if length > FRAME_LIMIT or opcode >= 8 and length > 125:
        raise BadFrame
    mask = await read_exact(reader, 4)
    payload = await read_exact(reader, length)
    payload = bytes(value ^ mask[i % 4] for i, value in enumerate(payload))
    if opcode == 1:
        try:
            payload.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise BadFrame from exc
    if opcode == 8:
        if len(payload) == 1:
            raise BadFrame
        if payload:
            code = struct.unpack("!H", payload[:2])[0]
            if not (code in (1000, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011)
                    or 3000 <= code <= 4999):
                raise BadFrame
            try:
                payload[2:].decode("utf-8")
            except UnicodeDecodeError as exc:
                raise BadFrame from exc
    return opcode, payload


async def send_frame(writer, opcode, payload):
    length = len(payload)
    if length < 126:
        header = bytes((0x80 | opcode, length))
    elif length <= 65535:
        header = bytes((0x80 | opcode, 126)) + struct.pack("!H", length)
    else:
        header = bytes((0x80 | opcode, 127)) + struct.pack("!Q", length)
    await write(writer, header + payload)


async def websocket(reader, writer, headers):
    accept = websocket_accept(headers)
    await write(
        writer,
        b"HTTP/1.1 101 Switching Protocols\r\n"
        b"Upgrade: websocket\r\nConnection: Upgrade\r\n"
        b"Sec-WebSocket-Accept: " + accept + b"\r\n\r\n",
    )
    while True:
        opcode, payload = await read_frame(reader)
        if opcode == 8:
            await send_frame(writer, 8, payload)
            return
        if opcode == 9:
            await send_frame(writer, 10, payload)
        elif opcode in (1, 2):
            await send_frame(writer, opcode, payload)


async def events(reader, writer):
    await write(
        writer,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
        b"Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    disconnect = asyncio.create_task(reader.read(1))
    try:
        sequence = 0
        while True:
            done, _ = await asyncio.wait({disconnect}, timeout=1)
            if done:
                return
            sequence += 1
            payload = b"data: " + str(sequence).encode() + b"\n\n"
            await write(
                writer,
                format(len(payload), "x").encode() + b"\r\n"
                + payload + b"\r\n",
            )
    finally:
        disconnect.cancel()
        await asyncio.gather(disconnect, return_exceptions=True)
        if not writer.is_closing():
            try:
                await asyncio.wait_for(write(writer, b"0\r\n\r\n"), 1)
            except (ConnectionError, OSError, asyncio.TimeoutError):
                pass


class Backend:
    def __init__(self, maximum):
        self.maximum = maximum
        self.semaphore = asyncio.Semaphore(maximum)
        self.tasks = set()
        self.rejected = 0
        self.errors = 0

    def accept(self, reader, writer):
        if len(self.tasks) >= self.maximum:
            self.rejected += 1
            writer.close()
            return
        task = asyncio.create_task(self.handle(reader, writer))
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)

    async def handle(self, reader, writer):
        try:
            async with self.semaphore:
                await self.serve(reader, writer)
        except (asyncio.IncompleteReadError, ConnectionError, BrokenPipeError):
            pass
        except (asyncio.TimeoutError, BadFrame, OSError):
            self.errors += 1
        except Exception:
            self.errors += 1
        finally:
            writer.close()
            try:
                await asyncio.wait_for(writer.wait_closed(), 1)
            except (ConnectionError, OSError, asyncio.TimeoutError):
                pass

    async def serve(self, reader, writer):
        while True:
            try:
                method, path, headers = await request(reader)
            except BadRequest:
                self.errors += 1
                await response(writer, b"400 Bad Request")
                return
            if method != b"GET":
                await response(
                    writer, b"405 Method Not Allowed",
                    extra=b"Allow: GET\r\n",
                )
                return
            close = b"close" in tokens(headers, b"connection")
            if path == b"/proxy":
                await response(
                    writer, b"200 OK", BODY, close,
                    b"Content-Type: application/octet-stream\r\n",
                )
                if close:
                    return
            elif path == b"/events":
                await events(reader, writer)
                return
            elif path == b"/ws":
                try:
                    await websocket(reader, writer, headers)
                except BadRequest:
                    self.errors += 1
                    await response(writer, b"400 Bad Request")
                return
            else:
                await response(writer, b"404 Not Found", close=close)
                if close:
                    return


async def run(args):
    backend = Backend(args.max_connections)
    server = await asyncio.start_server(
        backend.accept, args.host, args.port, limit=HEADER_LIMIT,
        backlog=args.max_connections,
    )
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    print(
        f"backend listening host={args.host} port={args.port} "
        f"max_connections={args.max_connections}",
        file=sys.stderr, flush=True,
    )
    try:
        await stop.wait()
    finally:
        server.close()
        await server.wait_closed()
        tasks = list(backend.tasks)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        print(
            f"backend summary errors={backend.errors} rejected={backend.rejected}",
            file=sys.stderr, flush=True,
        )


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=19090)
    parser.add_argument("--max-connections", type=positive, default=256)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("--port must be between 1 and 65535")
    try:
        asyncio.run(run(args))
    except OSError:
        print("backend startup failed", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
