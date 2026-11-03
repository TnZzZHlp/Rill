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
