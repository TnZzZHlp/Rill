use crate::support::*;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

pub(super) fn read_body(stream: &mut TcpStream) -> TestResult<(String, Vec<u8>)> {
    let headers = head(stream)?;
    let mut body = Vec::new();
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let length = usize::from_str_radix(line.trim(), 16)?;
            if length == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + length, 0);
            reader.read_exact(&mut body[start..])?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf)?;
            assert_eq!(&crlf, b"\r\n");
        }
    } else {
        let length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse().ok())
            })
            .unwrap_or(0);
        body.resize(length, 0);
        stream.read_exact(&mut body)?;
    }
    Ok((headers, body))
}

#[test]
fn respond_static_sidecar_range_head_and_error_chain_apply_actual_declarations() -> TestResult {
    let dir = Directory::new()?;
    fs::write(dir.0.join("index.html"), b"old cat")?;
    fs::write(dir.0.join("index.html.gz"), b"sidecar")?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-glob "/local"; }}; body-replace mime="text/html" {{ literal "old" "new"; regex "(cat)" "$1!"; }}; encode mime="text/html" {{ gzip level=3; zstd level=3; }}; respond 200 text="old cat" content-type="text/html"; }}
 rule {{ match {{ path-glob "/sidecar/*"; }}; serve-files root="{}" {{ precompressed #true; }}; }}
 rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; serve-files root="{}" {{ precompressed #true; }}; }}
 handle-errors {{ rule {{ body-replace mime="text/html" {{ literal "new" "error"; }}; respond 404 text="old" content-type="text/html"; }}; }}
}} }}"#,
        dir.0.display(),
        dir.0.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    for (path, encoding, expected) in [
        ("/local", "identity", &b"new cat!"[..]),
        ("/local", "gzip", &b"new cat!"[..]),
        ("/local", "zstd", &b"new cat!"[..]),
        (
            "/local",
            "gzip;q=0.2, zstd;q=0.9, identity;q=0",
            &b"new cat!"[..],
        ),
        (
            "/local",
            "gzip;q=1, gzip;q=0, identity;q=1",
            &b"new cat!"[..],
        ),
        ("/index.html", "gzip, identity", &b"new cat"[..]),
        ("/missing", "identity", &b"error"[..]),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nAccept-Encoding: {encoding}\r\nIf-None-Match: *\r\n\r\n"
        )?;
        let (headers, body) = read_body(&mut stream)?;
        assert!(
            headers.starts_with(if path == "/missing" {
                "HTTP/1.1 404 "
            } else {
                "HTTP/1.1 200 "
            }),
            "{headers}"
        );
        assert!(!headers.to_ascii_lowercase().contains("etag:"), "{headers}");
        assert!(
            !headers.to_ascii_lowercase().contains("last-modified:"),
            "{headers}"
        );
        if path == "/local" && encoding == "gzip" {
            assert!(
                headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .any(
                        |(name, value)| name.eq_ignore_ascii_case("content-encoding")
                            && value.trim().eq_ignore_ascii_case("gzip")
                    ),
                "{headers}"
            );
        }
        let mut decoded = Vec::new();
        if headers
            .to_ascii_lowercase()
            .contains("content-encoding: gzip")
        {
            flate2::read::GzDecoder::new(body.as_slice()).read_to_end(&mut decoded)?;
        } else if headers
            .to_ascii_lowercase()
            .contains("content-encoding: zstd")
        {
            decoded = zstd::stream::decode_all(body.as_slice())?;
        } else {
            decoded = body;
        }
        assert_eq!(decoded, expected);
    }
    let mut stream = connect(address)?;
    write!(
        stream,
        "HEAD /index.html HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n"
    )?;
    let headers = head(&mut stream)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ") && !headers.contains("content-length:"),
        "{headers}"
    );
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.is_empty());
    let mut stream = connect(address)?;
    write!(
        stream,
        "GET /index.html HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nRange: bytes=0-2\r\n\r\n"
    )?;
    let (headers, body) = read_body(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 206 "), "{headers}");
    assert_eq!(body, b"old");
    let mut stream = connect(address)?;
    write!(
        stream,
        "GET /local HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nAccept-Encoding: *;q=0\r\n\r\n"
    )?;
    let (headers, body) = read_body(&mut stream)?;
    assert!(
        headers.starts_with("HTTP/1.1 406 ") && headers.to_ascii_lowercase().contains("vary:"),
        "{headers}"
    );
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn legacy_proxy_preserves_upstream_headers_above_the_policy_limit() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\nX-Pad: {}\r\n\r\ncomplete body",
            "a".repeat(80 * 1024)
        )?;
        Ok(())
    });
    let config = format!(
        r#"server {{ http "{address}"; }}; site "app.test" {{ proxy "http://{upstream}"; }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let mut response = Vec::new();
    stream.take(96 * 1024 + 1).read_to_end(&mut response)?;
    assert!(response.len() <= 96 * 1024, "oversized test response");
    let boundary = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or("missing response headers")?;
    let headers = std::str::from_utf8(&response[..boundary])?;
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
    assert!(headers.lines().any(|line| {
        line.strip_prefix("X-Pad: ")
            .or_else(|| line.strip_prefix("x-pad: "))
            .is_some_and(|value| value.len() == 80 * 1024 && value.bytes().all(|byte| byte == b'a'))
    }));
    assert_eq!(&response[boundary + 4..], b"complete body");
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn proxy_replaces_case_insensitive_identity_content_encoding() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: Identity\r\nContent-Length: 3\r\nConnection: close\r\n\r\nold")?;
        Ok(())
    });
    let config = format!(
        r#"server {{ http "{address}"; }}; site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; proxy "http://{upstream}"; }}; }}; }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let (headers, body) = read_body(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
    assert_eq!(body, b"new");
    assert!(
        !headers.to_ascii_lowercase().contains("content-encoding:"),
        "{headers}"
    );
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn proxy_streams_before_eof_and_strips_only_cache_validators_on_its_single_copy() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let (advance, wait) = mpsc::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(
            !request.contains("if-none-match:") && !request.contains("if-modified-since:"),
            "{request}"
        );
        assert!(
            request.contains("if-match: business")
                && request.contains("if-unmodified-since: business"),
            "{request}"
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\nETag: W/\"old\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nContent-MD5: old\r\n\r\n7\r\nold cat\r\n")?;
        stream.flush()?;
        wait.recv_timeout(Duration::from_secs(3))?;
        stream.write_all(b"4\r\n old\r\n0\r\n\r\n")?;
        Ok(())
    });
    let config = format!(
        r#"server {{ http "{address}"; }}; site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; proxy "http://{upstream}"; }}; }}; }}"#
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nIf-None-Match: source\r\nIf-Modified-Since: source\r\nIf-Match: business\r\nIf-Unmodified-Since: business\r\n\r\n")?;
    let headers = head(&mut stream)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ")
            && !headers.contains("etag:")
            && !headers.contains("last-modified:")
            && !headers.contains("content-md5:"),
        "{headers}"
    );
    let mut first = [0; 10];
    stream.read_exact(&mut first)?;
    assert!(first.windows(7).any(|w| w == b"new cat"), "{first:?}");
    advance.send(())?;
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.windows(4).any(|w| w == b" new"), "{rest:?}");
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}
