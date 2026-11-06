use crate::support::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

#[test]
fn proxy_bypass_uses_original_metadata_and_preserves_range_business_conditions() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let worker = thread::spawn(move || -> TestResult {
        for kind in [
            "sse",
            "grpc",
            "no-transform",
            "range",
            "unknown",
            "mime",
            "304",
        ] {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?.to_ascii_lowercase();
            assert!(
                request.contains("if-match: business")
                    && request.contains("if-unmodified-since: business"),
                "{request}"
            );
            if kind == "range" {
                assert!(
                    request.contains("range: bytes=0-2")
                        && request.contains("if-range: source")
                        && request.contains("if-none-match: source")
                        && request.contains("if-modified-since: source"),
                    "{request}"
                );
            } else {
                assert!(
                    !request.contains("if-none-match:") && !request.contains("if-modified-since:"),
                    "{request}"
                );
            }
            let mime = match kind {
                "sse" => "text/event-stream",
                "grpc" => "application/grpc",
                "mime" => "application/octet-stream",
                _ => "text/html",
            };
            let extra = match kind {
                "no-transform" => "Cache-Control: no-transform\r\n",
                "unknown" => "Content-Encoding: br\r\n",
                "range" => "Content-Range: bytes 0-2/3\r\n",
                _ => "",
            };
            let status = match kind {
                "range" => 206,
                "304" => 304,
                _ => 200,
            };
            write!(
                stream,
                "HTTP/1.1 {status} OK\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: {mime}\r\nETag: W/\"source\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n{extra}\r\n",
                if kind == "304" { 0 } else { 3 }
            )?;
            if kind != "304" {
                stream.write_all(b"old")?;
            }
        }
        Ok(())
    });
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{
 body-replace mime="text/html" {{ literal "old" "new"; }}
 header "response" {{ set "content-type" "text/html"; delete "cache-control"; }}
 proxy "http://{upstream}"
}} }} }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    for kind in [
        "sse",
        "grpc",
        "no-transform",
        "range",
        "unknown",
        "mime",
        "304",
    ] {
        let mut stream = connect(address)?;
        let range = if kind == "range" {
            "Range: bytes=0-2\r\nIf-Range: source\r\n"
        } else {
            ""
        };
        write!(
            stream,
            "GET /{kind} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nIf-None-Match: source\r\nIf-Modified-Since: source\r\nIf-Match: business\r\nIf-Unmodified-Since: business\r\n{range}\r\n"
        )?;
        let (status, headers, body) = response(&mut stream)?;
        assert_eq!(
            status,
            match kind {
                "range" => 206,
                "304" => 304,
                _ => 200,
            }
        );
        assert!(
            headers.to_ascii_lowercase().contains("etag: w/\"source\""),
            "{headers}"
        );
        assert!(
            headers.to_ascii_lowercase().contains("last-modified:"),
            "{headers}"
        );
        assert_eq!(body, if kind == "304" { &b""[..] } else { &b"old"[..] });
    }
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn registered_transform_does_not_modify_upgraded_websocket_frames() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?;
        assert!(request.to_ascii_lowercase().contains("upgrade: websocket"));
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nContent-Type: text/html\r\n\r\n\x81\x03old")?;
        let mut close = [0; 6];
        stream.read_exact(&mut close)?;
        assert_eq!(close, [0x88, 0x80, 1, 2, 3, 4]);
        stream.write_all(b"\x88\x00")?;
        Ok(())
    });
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; proxy "http://{upstream}"; }}; }}; }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    assert!(head(&mut stream)?.starts_with("HTTP/1.1 101 "));
    let mut frame = [0; 5];
    stream.read_exact(&mut frame)?;
    assert_eq!(&frame, b"\x81\x03old");
    stream.write_all(&[0x88, 0x80, 1, 2, 3, 4])?;
    let mut close = [0; 2];
    stream.read_exact(&mut close)?;
    assert_eq!(close, [0x88, 0]);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}
