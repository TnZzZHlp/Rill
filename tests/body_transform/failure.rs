use crate::support::*;
use std::{
    fs,
    io::{Read, Write},
    thread,
    time::Duration,
};

#[test]
fn expansion_failure_aborts_after_headers_without_second_response_or_success_eof() -> TestResult {
    let directory = Directory::new()?;
    fs::write(directory.0.join("large.html"), vec![b'a'; 65536])?;
    let address = vacant()?;
    let configuration = format!(
        r#"server {{ http "{address}"; observability {{ access-log #true; }}; }}
site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "a" "{}"; }}; serve-files root="{}"; }}; }}; }}"#,
        "x".repeat(2048),
        directory.0.display()
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET /large.html HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut stream)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ") && headers.contains("transfer-encoding: chunked"),
        "{headers}"
    );
    let mut tail = [0; 5];
    let mut used = 0usize;
    let mut total = 0usize;
    let mut buffer = [0; 65536];
    loop {
        let n = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        };
        assert!(
            !buffer[..n].windows(5).any(|w| w == b"HTTP/"),
            "second response after committed headers"
        );
        for &byte in &buffer[..n] {
            tail[used % 5] = byte;
            used += 1;
        }
        total += n;
    }
    assert!(
        total > 0 && total < 66 * 1024 * 1024,
        "unexpected output: {total}"
    );
    let last: Vec<u8> = (0..5).map(|i| tail[(used + i) % 5]).collect();
    assert_ne!(
        last, b"0\r\n\r\n",
        "failed transform emitted a successful chunked EOF"
    );
    thread::sleep(Duration::from_millis(50));
    proxy.signal()?;
    let (_, logs) = proxy.finish()?;
    assert!(logs.contains("\"outcome\":\"error\""), "{logs}");
    assert!(!logs.contains("\"outcome\":\"completed\""), "{logs}");
    Ok(())
}

#[test]
fn finalized_transform_headers_cannot_exceed_the_shared_header_limit() -> TestResult {
    let address = vacant()?;
    let configuration = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ header "response" {{ set "x-pad" "{}"; }}; continue; }}
 rule {{ match {{ path-glob "/vary"; }}; encode mime="text/html" {{ gzip level=3; }}; respond 200 text="old" content-type="text/html"; }}
 rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; respond 200 text="old" content-type="text/html"; }}
 handle-errors {{ rule {{ respond 404 text="wrong"; }}; }}
}} }}"#,
        "a".repeat(65_483)
    );
    let _proxy = Proxy::start(address, &configuration)?;
    for (method, path, encoding) in [
        ("GET", "/", "identity"),
        ("HEAD", "/", "identity"),
        ("GET", "/", "*;q=0"),
        ("GET", "/vary", "identity"),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\nAccept-Encoding: {encoding}\r\n\r\n"
        )?;
        let headers = head(&mut stream)?;
        assert!(
            headers.starts_with("HTTP/1.1 503 "),
            "{method} {path}: finalized oversized headers were sent: {headers}"
        );
        let mut body = Vec::new();
        stream.read_to_end(&mut body)?;
        assert!(!body.windows(5).any(|w| w == b"HTTP/"));
        assert!(!headers.to_ascii_lowercase().contains("x-pad:"));
    }
    Ok(())
}

#[test]
fn preheader_transform_resource_failure_returns_one_503_without_entering_error_chain() -> TestResult
{
    let address = vacant()?;
    let configuration = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "{}" "new"; }}; respond 200 text="old" content-type="text/html"; }}; handle-errors {{ rule {{ respond 404 text="wrong"; }}; }}; }}; }}"#,
        "a".repeat(200_000)
    );
    let _proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 503 "), "{headers}");
    let mut body = Vec::new();
    stream.read_to_end(&mut body)?;
    assert!(!body.windows(5).any(|w| w == b"HTTP/"));
    Ok(())
}
