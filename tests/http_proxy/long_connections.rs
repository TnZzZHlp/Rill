use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::support::*;

const KEY: &str = "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";
const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const MASKED: &[u8] = &[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
const CLOSE: &[u8] = &[0x88, 0x80, 1, 2, 3, 4];

fn config(
    address: std::net::SocketAddr,
    backend: &TcpListener,
    options: &str,
) -> TestResult<String> {
    Ok(format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; {options} }}\n",
        backend.local_addr()?
    ))
}

fn handshake(stream: &mut TcpStream, extra: &str) -> TestResult {
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: app.test:8080\r\nConnection: Upgrade, X-Leak\r\nUpgrade: websocket\r\nX-Leak: secret\r\nSec-WebSocket-Extensions: permessage-deflate\r\n{KEY}{extra}\r\n"
    )?;
    Ok(())
}

fn switching(stream: &mut TcpStream) -> TestResult {
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(())
}

fn exact(stream: &mut TcpStream, expected: &[u8]) -> TestResult {
    let mut bytes = vec![0; expected.len()];
    stream.read_exact(&mut bytes)?;
    assert_eq!(bytes, expected);
    Ok(())
}

#[test]
fn websocket_frames_survive_request_budget_and_sigterm() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let cfg = config(
        address,
        &backend,
        "request-timeout \"50ms\"; read-timeout \"2s\"; idle-timeout \"2s\";",
    )?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(request.contains("host: app.test:8080\r\n"), "{request}");
        assert!(
            request.contains("forwarded: for=\"127.0.0.1\";proto=http;host=\"app.test:8080\"\r\n"),
            "{request}"
        );
        assert!(
            request.contains("x-forwarded-for: 127.0.0.1\r\n"),
            "{request}"
        );
        assert!(request.contains("x-forwarded-proto: http\r\n"), "{request}");
        assert!(
            !request.contains("x-leak:") && !request.contains("sec-websocket-extensions:"),
            "{request}"
        );
        switching(&mut stream)?;
        for _ in 0..3 {
            exact(&mut stream, MASKED)?;
            stream.write_all(b"\x81\x02hi")?;
        }
        exact(&mut stream, CLOSE)?;
        stream.write_all(b"\x88\x00")?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &cfg)?;
    let mut client = connect(address)?;
    handshake(&mut client, "")?;
    let headers = head(&mut client)?;
    assert!(headers.starts_with("HTTP/1.1 101 "), "{headers}");
    for round in 0..3 {
        if round == 1 {
            thread::sleep(Duration::from_millis(120));
        }
        if round == 2 {
            proxy.signal()?;
        }
        client.write_all(MASKED)?;
        exact(&mut client, b"\x81\x02hi")?;
    }
    client.write_all(CLOSE)?;
    exact(&mut client, b"\x88\x00")?;
    drop(client);
    worker.join().map_err(|_| "backend panicked")??;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn websocket_request_gate_never_contacts_backend() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    backend.set_nonblocking(true)?;
    let address = vacant()?;
    for disabled in [false, true] {
        let options = if disabled { "websocket #false;" } else { "" };
        let mut proxy = Proxy::start(address, &config(address, &backend, options)?)?;
        let cases: Vec<(String, u16)> = if disabled {
            vec![(KEY.into(), 403)]
        } else {
            vec![
                ("Sec-WebSocket-Version: 13\r\n".into(), 400),
                (
                    "Sec-WebSocket-Key: invalid\r\nSec-WebSocket-Version: 13\r\n".into(),
                    400,
                ),
                (
                    format!("{KEY}Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"),
                    400,
                ),
                (
                    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n".into(),
                    400,
                ),
                (KEY.replace("13", "12"), 400),
                (format!("{KEY}Sec-WebSocket-Version: 13\r\n"), 400),
                (format!("{KEY}Content-Length: 1\r\n"), 400),
            ]
        };
        for (fields, status) in cases {
            let mut client = connect(address)?;
            write!(
                client,
                "GET / HTTP/1.1\r\nHost: app.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n{fields}\r\n"
            )?;
            assert_eq!(response(&mut client)?.0, status);
        }
        if !disabled {
            for (connection, upgrade, status) in [
                ("Upgrade, Sec-WebSocket-Key", "websocket", 400),
                ("Upgrade", "h2c", 501),
            ] {
                let mut client = connect(address)?;
                write!(
                    client,
                    "GET / HTTP/1.1\r\nHost: app.test\r\nConnection: {connection}\r\nUpgrade: {upgrade}\r\n{KEY}\r\n"
                )?;
                assert_eq!(response(&mut client)?.0, status);
            }
        }
        assert_eq!(
            backend.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    Ok(())
}

#[test]
fn invalid_upstream_switching_protocols_is_rejected() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let cfg = config(address, &backend, "read-timeout \"1s\";")?;
    let worker = thread::spawn(move || -> TestResult {
        for fields in [
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: wrong\r\n"
                .to_string(),
            format!(
                "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\nSec-WebSocket-Accept: {ACCEPT}\r\n"
            ),
            format!("Connection: Upgrade\r\nUpgrade: h2c\r\nSec-WebSocket-Accept: {ACCEPT}\r\n"),
            format!(
                "Connection: Upgrade, Sec-WebSocket-Accept\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\n"
            ),
            format!(
                "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\nSec-WebSocket-Protocol: unexpected\r\n"
            ),
            format!(
                "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\nSec-WebSocket-Extensions: permessage-deflate\r\n"
            ),
            format!(
                "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {ACCEPT}\r\n"
            ),
        ] {
            let mut stream = accept(&backend)?;
            head(&mut stream)?;
            write!(stream, "HTTP/1.1 101 Switching Protocols\r\n{fields}\r\n")?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &cfg)?;
    for index in 0..7 {
        let mut client = connect(address)?;
        if index == 6 {
            client.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
        } else {
            handshake(&mut client, "")?;
        }
        let (status, headers, _) = response(&mut client)?;
        assert_eq!(status, 502, "{headers}");
    }
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

fn chunk(stream: &mut TcpStream) -> TestResult<Vec<u8>> {
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
        if line.len() > 100 {
            return Err("chunk header too long".into());
        }
    }
    let size = usize::from_str_radix(
        std::str::from_utf8(&line[..line.len() - 2])?
            .split(';')
            .next()
            .ok_or("chunk size")?,
        16,
    )?;
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    exact(stream, b"\r\n")?;
    Ok(data)
}

#[test]
fn sse_chunks_stream_past_request_budget_and_drain_on_sigterm() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let cfg = config(
        address,
        &backend,
        "request-timeout \"50ms\"; read-timeout \"1s\";",
    )?;
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: 1\n\n\r\n")?;
        stream.flush()?;
        rx.recv_timeout(Duration::from_secs(3))?;
        thread::sleep(Duration::from_millis(120));
        stream.write_all(b"9\r\ndata: 2\n\n\r\n")?;
        stream.flush()?;
        rx.recv_timeout(Duration::from_secs(3))?;
        stream.write_all(b"9\r\ndata: 3\n\n\r\n0\r\n\r\n")?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &cfg)?;
    let mut client = connect(address)?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut client)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ")
            && headers.contains("text/event-stream")
            && headers.contains("transfer-encoding: chunked"),
        "{headers}"
    );
    assert_eq!(chunk(&mut client)?, b"data: 1\n\n");
    tx.send(())?;
    assert_eq!(chunk(&mut client)?, b"data: 2\n\n");
    proxy.signal()?;
    tx.send(())?;
    assert_eq!(chunk(&mut client)?, b"data: 3\n\n");
    assert!(chunk(&mut client)?.is_empty());
    worker.join().map_err(|_| "backend panicked")??;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn websocket_idle_connection_closes_within_timeout() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let cfg = config(
        address,
        &backend,
        "request-timeout \"50ms\"; read-timeout \"100ms\"; idle-timeout \"100ms\";",
    )?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        head(&mut stream)?;
        switching(&mut stream)?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(()),
            other => Err(format!("backend did not close: {other:?}").into()),
        }
    });
    let mut proxy = Proxy::start(address, &cfg)?;
    let mut client = connect(address)?;
    client.set_read_timeout(Some(Duration::from_secs(3)))?;
    handshake(&mut client, "")?;
    assert!(head(&mut client)?.starts_with("HTTP/1.1 101 "));
    let start = Instant::now();
    let mut byte = [0];
    match client.read(&mut byte) {
        Ok(0) => (),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => (),
        other => return Err(format!("client did not close: {other:?}").into()),
    }
    assert!(start.elapsed() <= Duration::from_secs(3));
    worker.join().map_err(|_| "backend panicked")??;
    assert!(proxy.child.try_wait()?.is_none());
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}
