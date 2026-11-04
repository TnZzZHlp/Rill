use super::support::{Fixture, TestResult, header};
use std::fs::{self, File};
use std::io::Write;
use std::time::{Duration, SystemTime};

#[test]
fn metadata_methods_and_conditional_requests() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("a.txt"), b"abcdef")?;
    fs::write(f.root.join("empty"), b"")?;
    File::open(f.root.join("a.txt"))?.set_modified(SystemTime::now() - Duration::from_secs(120))?;
    f.start("cache-control \"public, max-age=60\";")?;
    let (status, h, body) = f.request("GET", "/a.txt", "")?;
    assert_eq!((status, body.as_slice()), (200, b"abcdef".as_slice()));
    assert_eq!(
        header(&h, "content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(header(&h, "cache-control"), Some("public, max-age=60"));
    let modified = header(&h, "last-modified")
        .ok_or("missing last-modified")?
        .to_owned();
    let (status, h, body) = f.request("HEAD", "/a.txt", "Range: bytes=0-1\r\n")?;
    assert_eq!(status, 200);
    assert_eq!(header(&h, "content-length"), Some("6"));
    assert!(body.is_empty());
    assert_eq!(f.request("GET", "/empty", "")?.2, b"");
    let (status, h, _) = f.request("POST", "/a.txt", "")?;
    assert_eq!(status, 405);
    assert_eq!(header(&h, "allow"), Some("GET, HEAD"));
    assert_eq!(
        f.request(
            "GET",
            "/a.txt",
            &format!("If-Modified-Since: {modified}\r\n")
        )?
        .0,
        304
    );
    File::open(f.root.join("a.txt"))?.set_modified(SystemTime::now())?;
    assert_eq!(
        f.request(
            "GET",
            "/a.txt",
            &format!("If-Modified-Since: {modified}\r\n")
        )?
        .0,
        200
    );
    Ok(())
}

#[test]
fn ranges_and_if_range_dates() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("data"), b"abcdef")?;
    fs::write(f.root.join("empty"), b"")?;
    File::open(f.root.join("data"))?.set_modified(SystemTime::now() - Duration::from_secs(120))?;
    f.start("")?;
    let (_, h, _) = f.request("GET", "/data", "")?;
    let date = header(&h, "last-modified").ok_or("missing last-modified")?;
    for (range, status, body, content_range) in [
        ("bytes=1-3", 206, "bcd", Some("bytes 1-3/6")),
        ("bytes=3-", 206, "def", Some("bytes 3-5/6")),
        ("bytes=-2", 206, "ef", Some("bytes 4-5/6")),
        ("bytes=99-", 416, "", Some("bytes */6")),
        ("bytes=0-1,3-4", 200, "abcdef", None),
        ("bytes=bad", 200, "abcdef", None),
        (
            "bytes=-999999999999999999999999",
            206,
            "abcdef",
            Some("bytes 0-5/6"),
        ),
        (
            "bytes=999999999999999999999999-",
            416,
            "",
            Some("bytes */6"),
        ),
        (
            "bytes=2-999999999999999999999999",
            206,
            "cdef",
            Some("bytes 2-5/6"),
        ),
    ] {
        let (s, h, b) = f.request("GET", "/data", &format!("Range: {range}\r\n"))?;
        assert_eq!(s, status, "{range}");
        assert_eq!(b, body.as_bytes(), "{range}");
        assert_eq!(header(&h, "content-range"), content_range, "{range}");
    }
    let (s, h, _) = f.request("GET", "/empty", "Range: bytes=0-1\r\n")?;
    assert_eq!(s, 416);
    assert_eq!(header(&h, "content-range"), Some("bytes */0"));
    assert_eq!(
        f.request(
            "GET",
            "/data",
            &format!("Range: bytes=0-1\r\nIf-Range: {date}\r\n")
        )?
        .0,
        206
    );
    for value in ["invalid", "W/\"etag\"", "Thu, 01 Jan 1970 00:00:00 GMT"] {
        assert_eq!(
            f.request(
                "GET",
                "/data",
                &format!("Range: bytes=0-1\r\nIf-Range: {value}\r\n")
            )?
            .0,
            200
        );
    }
    Ok(())
}

#[test]
fn spa_fallback_requires_navigation() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("index.html"), b"index")?;
    fs::create_dir(f.root.join("directory"))?;
    f.start("spa #true;")?;
    let (status, _, body) = f.request("GET", "/route", "Accept: text/html;q=0.5\r\n")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"index");
    let (status, _, body) = f.request("HEAD", "/route", "Accept: text/html;q=0.5\r\n")?;
    assert_eq!(status, 200);
    assert!(body.is_empty());
    for (path, headers, status) in [
        ("/route", "Accept: text/html;q=0\r\n", 404),
        ("/route", "", 404),
        ("/app.js", "Accept: text/html\r\n", 404),
        ("/app.css", "Accept: text/html\r\n", 404),
        ("/image.png", "Accept: text/html\r\n", 404),
        ("/route", "Accept: text/html\r\nRange: bytes=0-1\r\n", 404),
        ("/directory/", "Accept: text/html\r\n", 404),
        ("/%2e%2e", "Accept: text/html\r\n", 400),
    ] {
        assert_eq!(f.request("GET", path, headers)?.0, status, "{path}");
    }
    Ok(())
}

#[test]
fn large_file_range_crosses_block_boundary() -> TestResult {
    let mut f = Fixture::new()?;
    let mut file = File::create(f.root.join("large"))?;
    for block in 0..256u16 {
        let data = vec![block as u8; 65536];
        file.write_all(&data)?;
    }
    drop(file);
    f.start("")?;
    let (status, headers, body) = f.request("GET", "/large", "Range: bytes=1048583-1114118\r\n")?;
    assert_eq!(status, 206);
    assert_eq!(
        header(&headers, "content-range"),
        Some("bytes 1048583-1114118/16777216")
    );
    assert_eq!(header(&headers, "content-length"), Some("65536"));
    assert_eq!(body.len(), 65536);
    assert!(body[..65529].iter().all(|&byte| byte == 16));
    assert!(body[65529..].iter().all(|&byte| byte == 17));
    Ok(())
}

#[test]
fn precompressed_negotiation_and_fallback() -> TestResult {
    let mut f = Fixture::new()?;
    let original = b"original javascript";
    let gzip = b"gzip fixture bytes";
    let brotli = b"brotli fixture bytes";
    fs::write(f.root.join("asset.js"), original)?;
    fs::write(f.root.join("asset.js.gz"), gzip)?;
    fs::write(f.root.join("asset.js.br"), brotli)?;
    let old = SystemTime::now() - Duration::from_secs(120);
    File::open(f.root.join("asset.js"))?.set_modified(old)?;
    f.start("precompressed #true;")?;
    for (headers, encoding, expected) in [
        ("", None, original.as_slice()),
        ("Accept-Encoding: \r\n", None, original.as_slice()),
        (
            "Accept-Encoding: gzip, br\r\n",
            Some("br"),
            brotli.as_slice(),
        ),
        (
            "Accept-Encoding: gzip;q=1, br;q=0.5\r\n",
            Some("gzip"),
            gzip.as_slice(),
        ),
        (
            "Accept-Encoding: gzip;q=0, br;q=0\r\n",
            None,
            original.as_slice(),
        ),
        (
            "Accept-Encoding: br;q=0.9\r\nAccept-Encoding: br;q=0.1, identity;q=0\r\n",
            Some("br"),
            brotli.as_slice(),
        ),
    ] {
        let (status, h, body) = f.request("GET", "/asset.js", headers)?;
        assert_eq!(status, 200, "{headers}");
        assert_eq!(body, expected, "{headers}");
        assert_eq!(header(&h, "content-encoding"), encoding);
        assert_eq!(
            header(&h, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(header(&h, "vary"), Some("Accept-Encoding"));
    }
    let (status, h, body) = f.request(
        "HEAD",
        "/asset.js",
        "Accept-Encoding: br\r\nRange: bytes=0-1\r\n",
    )?;
    assert_eq!(status, 200);
    assert_eq!(header(&h, "content-encoding"), Some("br"));
    assert_eq!(header(&h, "content-length"), Some("20"));
    assert!(body.is_empty());
    let (status, h, body) = f.request("GET", "/asset.js", "Accept-Encoding: *;q=0\r\n")?;
    assert_eq!(status, 406);
    assert_eq!(header(&h, "vary"), Some("Accept-Encoding"));
    assert!(body.is_empty());
    fs::remove_file(f.root.join("asset.js.br"))?;
    assert_eq!(
        f.request("GET", "/asset.js", "Accept-Encoding: br, identity;q=0\r\n")?
            .0,
        406
    );
    File::open(f.root.join("asset.js.gz"))?.set_modified(old - Duration::from_secs(1))?;
    assert_eq!(
        f.request("GET", "/asset.js", "Accept-Encoding: gzip\r\n")?
            .2,
        original
    );
    assert_eq!(
        f.request(
            "GET",
            "/asset.js",
            "Accept-Encoding: gzip, identity;q=0\r\n"
        )?
        .0,
        406
    );
    let mut disabled = Fixture::new()?;
    fs::write(disabled.root.join("asset.js"), original)?;
    fs::write(disabled.root.join("asset.js.br"), brotli)?;
    disabled.start("")?;
    assert_eq!(
        disabled
            .request("GET", "/asset.js", "Accept-Encoding: br, identity;q=0\r\n")?
            .0,
        406
    );
    let (_, h, _) = disabled.request("GET", "/asset.js", "")?;
    assert!(header(&h, "etag").is_some());
    assert_eq!(header(&h, "vary"), Some("Accept-Encoding"));
    Ok(())
}

#[test]
fn encoded_validators_ranges_and_conditional_precedence() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("data.txt"), b"original")?;
    fs::write(f.root.join("data.txt.gz"), b"encoded bytes")?;
    let old = SystemTime::now() - Duration::from_secs(120);
    File::open(f.root.join("data.txt"))?.set_modified(old - Duration::from_secs(1))?;
    File::open(f.root.join("data.txt.gz"))?.set_modified(old)?;
    f.start("precompressed #true;")?;
    let (_, identity, _) = f.request("GET", "/data.txt", "")?;
    let identity_tag = header(&identity, "etag").ok_or("identity etag")?;
    let (_, encoded, body) = f.request("GET", "/data.txt", "Accept-Encoding: gzip\r\n")?;
    assert_eq!(body, b"encoded bytes");
    let etag = header(&encoded, "etag").ok_or("encoded etag")?;
    let date = header(&encoded, "last-modified").ok_or("modified")?;
    assert!(etag.starts_with("W/\""));
    assert_ne!(identity_tag, etag);
    let (status, h, body) = f.request(
        "GET",
        "/data.txt",
        &format!("Accept-Encoding: gzip\r\nIf-None-Match: {etag}\r\nRange: bytes=0-1\r\n"),
    )?;
    assert_eq!(status, 304);
    assert!(body.is_empty());
    assert_eq!(header(&h, "content-length"), None);
    assert_eq!(header(&h, "content-encoding"), Some("gzip"));
    assert_eq!(header(&h, "vary"), Some("Accept-Encoding"));
    assert_eq!(header(&h, "etag"), Some(etag));
    for condition in [
        format!("If-None-Match: {identity_tag}\r\nIf-Modified-Since: {date}\r\n"),
        format!("If-None-Match: invalid\r\nIf-Modified-Since: {date}\r\n"),
    ] {
        assert_eq!(
            f.request(
                "GET",
                "/data.txt",
                &format!("Accept-Encoding: gzip\r\n{condition}")
            )?
            .0,
            200
        );
    }
    assert_eq!(
        f.request(
            "GET",
            "/data.txt",
            "Accept-Encoding: gzip\r\nIf-None-Match: *\r\n"
        )?
        .0,
        304
    );
    let strong = etag.strip_prefix("W/").ok_or("weak etag")?;
    assert_eq!(
        f.request(
            "GET",
            "/data.txt",
            &format!(
                "Accept-Encoding: gzip\r\nIf-None-Match: \"other\"\r\nIf-None-Match: {strong}\r\n"
            )
        )?
        .0,
        304
    );
    assert_eq!(
        f.request(
            "GET",
            "/data.txt",
            &format!("Accept-Encoding: gzip\r\nIf-Modified-Since: {date}\r\n")
        )?
        .0,
        304
    );
    let (status, h, body) = f.request(
        "GET",
        "/data.txt",
        "Accept-Encoding: gzip\r\nRange: bytes=1-3\r\n",
    )?;
    assert_eq!(status, 206);
    assert_eq!(body, b"nco");
    assert_eq!(header(&h, "content-range"), Some("bytes 1-3/13"));
    assert_eq!(header(&h, "etag"), Some(etag));
    for value in [etag, etag.strip_prefix("W/").ok_or("weak etag")?] {
        assert_eq!(
            f.request(
                "GET",
                "/data.txt",
                &format!("Accept-Encoding: gzip\r\nRange: bytes=0-1\r\nIf-Range: {value}\r\n")
            )?
            .0,
            200
        );
    }
    assert_eq!(
        f.request(
            "GET",
            "/data.txt",
            &format!("Accept-Encoding: gzip\r\nRange: bytes=0-1\r\nIf-Range: {date}\r\n")
        )?
        .0,
        206
    );
    let (status, h, body) = f.request(
        "GET",
        "/data.txt",
        "Accept-Encoding: gzip\r\nRange: bytes=99-\r\n",
    )?;
    assert_eq!(status, 416);
    assert_eq!(header(&h, "content-range"), Some("bytes */13"));
    assert_eq!(header(&h, "etag"), Some(etag));
    assert_eq!(header(&h, "content-encoding"), Some("gzip"));
    assert_eq!(header(&h, "vary"), Some("Accept-Encoding"));
    assert!(body.is_empty());
    fs::write(f.root.join("data.txt.gz"), b"replacement encoded")?;
    let (_, changed, _) = f.request("GET", "/data.txt", "Accept-Encoding: gzip\r\n")?;
    assert_ne!(header(&changed, "etag"), Some(etag));
    Ok(())
}

#[test]
fn custom_errors_preserve_status_and_headers() -> TestResult {
    let mut f = Fixture::new()?;
    let page = f.dir.join("error.html");
    fs::write(&page, b"error page")?;
    fs::write(f.root.join("data"), b"abc")?;
    std::os::unix::fs::symlink("../outside", f.root.join("denied"))?;
    let pages = [403, 404, 405, 406, 416]
        .iter()
        .map(|code| format!("error-page {code} {:?};", page.to_string_lossy()))
        .collect::<String>();
    f.start(&pages)?;
    for (method, path, request, expected) in [
        ("GET", "/denied", "", 403),
        (
            "GET",
            "/missing",
            "If-None-Match: *\r\nRange: bytes=0-1\r\n",
            404,
        ),
        ("POST", "/data", "", 405),
        ("GET", "/data", "Range: bytes=99-\r\n", 416),
    ] {
        let (status, head, body) = f.request(method, path, request)?;
        assert_eq!(status, expected);
        assert_eq!(body, b"error page");
        assert_eq!(header(&head, "cache-control"), Some("no-store"));
        assert_eq!(header(&head, "etag"), None);
        if expected == 405 {
            assert_eq!(header(&head, "allow"), Some("GET, HEAD"));
        }
        if expected == 416 {
            assert_eq!(header(&head, "content-range"), Some("bytes */3"));
        }
    }
    let (status, head, body) = f.request("HEAD", "/missing", "Range: bytes=0-1\r\n")?;
    assert_eq!(status, 404);
    assert_eq!(header(&head, "content-length"), Some("10"));
    assert!(body.is_empty());
    assert_eq!(
        f.request("GET", "/data", "Accept-Encoding: identity;q=0\r\n")?
            .0,
        406
    );
    assert!(
        f.request("GET", "/data", "Accept-Encoding: identity;q=0\r\n")?
            .2
            .is_empty()
    );
    fs::remove_file(&page)?;
    assert!(f.request("GET", "/missing", "")?.2.is_empty());
    fs::write(&page, vec![0; 1024 * 1024 + 1])?;
    assert!(f.request("GET", "/missing", "")?.2.is_empty());
    Ok(())
}

#[test]
fn cached_file_revalidates_representation_range_and_replacement() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("data.txt"), b"abcdef")?;
    fs::write(f.root.join("data.txt.gz"), b"GZIP12")?;
    fs::write(f.root.join("large"), vec![b'x'; 65537])?;
    let config = f.config("precompressed #true;").replacen(
        "server {",
        "server { static-cache-bytes 131072;",
        1,
    );
    f.start_config(&config, false)?;
    let (_, headers, body) = f.request("GET", "/data.txt", "")?;
    assert_eq!(body, b"abcdef");
    let etag = header(&headers, "etag").ok_or("missing etag")?.to_owned();
    assert_eq!(f.request("GET", "/data.txt", "")?.2, b"abcdef");
    let (status, _, body) = f.request("GET", "/data.txt", "Range: bytes=1-3\r\n")?;
    assert_eq!((status, body.as_slice()), (206, b"bcd".as_slice()));
    assert_eq!(
        f.request("GET", "/data.txt", &format!("If-None-Match: {etag}\r\n"))?
            .0,
        304
    );
    let (status, headers, body) = f.request("GET", "/data.txt", "Accept-Encoding: gzip\r\n")?;
    assert_eq!((status, body.as_slice()), (200, b"GZIP12".as_slice()));
    assert_eq!(header(&headers, "content-encoding"), Some("gzip"));
    assert_eq!(f.request("GET", "/large", "")?.2.len(), 65537);
    std::thread::scope(|scope| -> TestResult {
        let jobs: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| f.request("GET", "/data.txt", "")))
            .collect();
        for job in jobs {
            assert_eq!(
                job.join().map_err(|_| "request thread panicked")??.2,
                b"abcdef"
            );
        }
        Ok(())
    })?;
    fs::write(f.root.join("replacement"), b"uvwxyz")?;
    fs::rename(f.root.join("replacement"), f.root.join("data.txt"))?;
    assert_eq!(f.request("GET", "/data.txt", "")?.2, b"uvwxyz");
    fs::remove_file(f.root.join("data.txt"))?;
    assert_eq!(f.request("GET", "/data.txt", "")?.0, 404);
    Ok(())
}
