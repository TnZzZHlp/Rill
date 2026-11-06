use super::http::read_body;
use crate::support::*;
use std::{
    fs,
    io::{Read, Write},
};

#[test]
fn encode_only_preserves_sidecar_validators_while_online_and_listing_use_get_metadata() -> TestResult
{
    let directory = Directory::new()?;
    fs::write(directory.0.join("index.html"), b"old cat")?;
    fs::create_dir(directory.0.join("files"))?;
    fs::write(directory.0.join("files/old.txt"), b"old")?;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
    gzip.write_all(b"old cat")?;
    let sidecar = gzip.finish()?;
    fs::write(directory.0.join("index.html.gz"), &sidecar)?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; static-cache-bytes 65536; }}
site "encode.test" {{ chain {{ rule {{ encode mime="text/html" {{ gzip level=3; zstd level=3; }}; serve-files root="{}" {{ precompressed #true; browse #true; }}; }}; }}; }}
site "replace.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; serve-files root="{}" {{ precompressed #true; browse #true; }}; }}; }}; }}"#,
        directory.0.display(),
        directory.0.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET /index.html HTTP/1.1\r\nHost: encode.test\r\nConnection: close\r\nAccept-Encoding: gzip\r\n\r\n")?;
    let (headers, body) = read_body(&mut stream)?;
    assert_eq!(body, sidecar);
    let validator = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("etag"))
                .map(|(_, value)| value.trim())
        })
        .ok_or("sidecar ETag")?;
    let mut stream = connect(address)?;
    write!(
        stream,
        "GET /index.html HTTP/1.1\r\nHost: encode.test\r\nConnection: close\r\nAccept-Encoding: gzip\r\nIf-None-Match: {validator}\r\n\r\n"
    )?;
    let headers = head(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 304 "), "{headers}");
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.is_empty());
    for path in ["/index.html", "/files/"] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: encode.test\r\nConnection: close\r\nAccept-Encoding: zstd, identity;q=0\r\nIf-None-Match: *\r\n\r\n"
        )?;
        let (headers, body) = read_body(&mut stream)?;
        assert!(
            headers.starts_with("HTTP/1.1 200 ")
                && headers
                    .to_ascii_lowercase()
                    .contains("content-encoding: zstd"),
            "{headers}"
        );
        assert!(
            !headers.to_ascii_lowercase().contains("etag:")
                && !headers.to_ascii_lowercase().contains("last-modified:"),
            "{headers}"
        );
        let decoded = zstd::stream::decode_all(body.as_slice())?;
        assert!(decoded.windows(3).any(|w| w == b"old"));
    }
    for _ in 0..2 {
        let mut stream = connect(address)?;
        stream.write_all(b"GET /index.html HTTP/1.1\r\nHost: replace.test\r\nConnection: close\r\nAccept-Encoding: gzip, identity\r\nIf-None-Match: *\r\n\r\n")?;
        let (headers, body) = read_body(&mut stream)?;
        assert!(
            headers.starts_with("HTTP/1.1 200 ")
                && !headers.to_ascii_lowercase().contains("content-encoding:"),
            "{headers}"
        );
        assert_eq!(body, b"new cat");
    }
    let mut stream = connect(address)?;
    stream.write_all(b"GET /files/ HTTP/1.1\r\nHost: replace.test\r\nConnection: close\r\nIf-None-Match: *\r\n\r\n")?;
    let (headers, body) = read_body(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
    assert!(body.windows(7).any(|w| w == b"new.txt") && !body.windows(7).any(|w| w == b"old.txt"));
    let mut stream = connect(address)?;
    stream.write_all(b"GET /index.html HTTP/1.1\r\nHost: replace.test\r\nConnection: close\r\nAccept-Encoding: identity;q=0\r\nIf-None-Match: *\r\n\r\n")?;
    let headers = head(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 406 "), "{headers}");
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.is_empty());
    let mut stream = connect(address)?;
    stream.write_all(b"HEAD /index.html HTTP/1.1\r\nHost: encode.test\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n")?;
    let headers = head(&mut stream)?.to_ascii_lowercase();
    assert!(
        headers.contains("content-length: 7") && headers.contains("etag:"),
        "{headers}"
    );
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest)?;
    assert!(rest.is_empty());
    Ok(())
}
