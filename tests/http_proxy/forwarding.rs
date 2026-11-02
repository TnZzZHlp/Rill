use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::support::*;

#[test]
fn forwarded_and_hop_headers_are_rebuilt_without_changing_the_raw_target() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?;
        assert!(
            request.starts_with("GET /api/%2f?x=%2F HTTP/1.1\r\n"),
            "{request}"
        );
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("host: app.test:8080\r\n"));
        assert!(lower.contains("x-forwarded-for: 127.0.0.1\r\n"));
        assert!(lower.contains("x-forwarded-proto: http\r\n"));
        assert!(
            !lower.contains("evil")
                && !lower.contains("x-remove")
                && !lower.contains("x-forwarded-extra")
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close, X-Remove-Response\r\nX-Remove-Response: hidden\r\n\r\nok")?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET /api/%2f?x=%2F HTTP/1.1\r\nHost: app.test:8080\r\nConnection: close, X-Remove\r\nX-Remove: evil\r\nForwarded: for=evil\r\nX-Forwarded-For: evil\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: evil\r\nX-Forwarded-Extra: evil\r\n\r\n")?;
    let (status, headers, body) = response(&mut stream)?;
    assert_eq!(status, 200);
    assert_eq!(body, b"ok");
    assert!(!headers.to_ascii_lowercase().contains("x-remove-response"));
    proxy.signal()?;
    let (code, logs) = proxy.finish()?;
    assert_eq!(code, 0);
    assert!(!logs.contains("evil") && !logs.contains("/api/"));
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn conflicting_framing_preserves_pingoras_forced_connection_close() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(!request.contains("content-length:"));
        assert!(request.contains("transfer-encoding: chunked"));
        let mut end = [0; 5];
        stream.read_exact(&mut end)?;
        assert_eq!(&end, b"0\r\n\r\n");
        reply(&mut stream, b"ok", false)?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"POST / HTTP/1.1\r\nHost: app.test\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")?;
    let (status, headers, body) = response(&mut stream)?;
    assert_eq!(status, 200);
    assert_eq!(body, b"ok");
    assert!(headers.to_ascii_lowercase().contains("connection: close"));
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.is_empty());
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn trusted_proxy_chain_is_validated_and_regenerated() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; trusted-proxy \"127.0.0.0/8\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(request.contains("x-forwarded-for: 198.51.100.7, 127.0.0.2, 127.0.0.1"));
        assert!(!request.contains("203.0.113.8"));
        assert!(request.contains("x-forwarded-proto: https"));
        assert!(request.contains("x-forwarded-port: 443"));
        reply(&mut stream, b"ok", true)?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nX-Forwarded-For: 203.0.113.8, 198.51.100.7, 127.0.0.2\r\nX-Forwarded-Proto: https\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 200);
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nX-Forwarded-For: invalid\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 400);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn connection_failures_and_header_deadlines_return_gateway_errors() -> TestResult {
    let unavailable = TcpListener::bind("127.0.0.1:0")?;
    let closed = unavailable.local_addr()?;
    drop(unavailable);
    let slow = TcpListener::bind("127.0.0.1:0")?;
    let slow_address = slow.local_addr()?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{\nroute \"/refused\" {{ proxy \"http://{closed}\"; }}\nroute \"/slow\" {{ proxy \"http://{slow_address}\"; request-timeout \"100ms\"; read-timeout \"5s\"; }}\n}}\n"
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&slow)?;
        head(&mut stream)?;
        thread::sleep(Duration::from_millis(400));
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(send(address, "app.test", "/refused")?.0, 502);
    assert_eq!(send(address, "app.test", "/slow")?.0, 504);
    proxy.signal()?;
    let (code, logs) = proxy.finish()?;
    assert_eq!(code, 0);
    assert!(logs.contains("request_timeout") && logs.contains("504"));
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn response_header_deadline_does_not_reset_on_each_upstream_byte() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; request-timeout \"100ms\"; read-timeout \"2s\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nX-Drip: ")?;
        for _ in 0..20 {
            if stream.write_all(b"a").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let start = std::time::Instant::now();
    assert_eq!(send(address, "app.test", "/")?.0, 504);
    assert!(start.elapsed() < Duration::from_secs(1));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn request_and_response_bodies_stream_before_their_producers_finish() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let (arrived, arrival) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let headers = head(&mut stream)?;
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-length: 2097152")
        );
        let mut bytes = [0; 4096];
        stream.read_exact(&mut bytes)?;
        assert!(bytes.iter().all(|byte| *byte == b'a'));
        arrived.send(())?;
        let mut remaining = 2 * 1024 * 1024 - bytes.len();
        while remaining > 0 {
            let n = stream.read(&mut bytes[..remaining.min(4096)])?;
            if n == 0 {
                return Err("truncated request".into());
            }
            remaining -= n;
        }
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1000\r\n",
        )?;
        stream.write_all(&vec![b'b'; 4096])?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        released.recv_timeout(Duration::from_secs(5))?;
        for _ in 0..511 {
            stream.write_all(b"1000\r\n")?;
            stream.write_all(&vec![b'b'; 4096])?;
            stream.write_all(b"\r\n")?;
        }
        stream.write_all(b"0\r\n\r\n")?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"POST /upload HTTP/1.1\r\nHost: app.test\r\nContent-Length: 2097152\r\nConnection: close\r\n\r\n")?;
    stream.write_all(&vec![b'a'; 4096])?;
    arrival.recv_timeout(Duration::from_secs(5))?;
    for _ in 0..511 {
        stream.write_all(&vec![b'a'; 4096])?;
    }
    let headers = head(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 200"));
    let mut first = vec![0; 4104];
    stream.read_exact(&mut first)?;
    assert!(first.windows(16).any(|bytes| bytes == [b'b'; 16]));
    release.send(())?;
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.len() > 2 * 1024 * 1024 - 4096);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}
