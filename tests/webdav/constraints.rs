use crate::support::{Directory, Proxy, TestResult, connect, response, vacant};
use base64::Engine;
use std::{fs, io::Write, thread, time::Duration};

#[test]
fn webdav_basic_auth_depth_zero_bindings_expiry_xml_limits_and_atomic_proppatch() -> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let address = vacant()?;
    let hash = bcrypt::hash("fixture-password", 10)?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{basic-auth realm="dav" {{user "alice" hash="{hash}";}}; webdav root="{}" storage-id="documents";}};}};}}"#,
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    for authorization in ["", "Authorization: Basic invalid\r\n"] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "OPTIONS / HTTP/1.1\r\nHost: app.test\r\n{authorization}Connection: close\r\n\r\n"
        )?;
        let (status, headers, _) = response(&mut stream)?;
        assert_eq!(status, 401);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("www-authenticate: basic realm=\"dav\"")
        );
    }
    let credential = base64::engine::general_purpose::STANDARD.encode("alice:fixture-password");
    let lock=br#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
    let mut stream = connect(address)?;
    write!(
        stream,
        "LOCK / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\nDepth: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        lock.len()
    )?;
    stream.write_all(lock)?;
    let (status, headers, _) = response(&mut stream)?;
    assert_eq!(status, 200);
    let root_token = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("lock-token"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .ok_or("missing root token")?;
    for (method, path, headers, body, expected) in [
        ("MKCOL", "/child", String::new(), &b""[..], 423),
        (
            "MKCOL",
            "/child",
            format!("If: <http://app.test/> ({root_token})\r\n"),
            &b""[..],
            201,
        ),
        (
            "PUT",
            "/direct",
            format!("If: ({root_token})\r\n"),
            &b"not committed"[..],
            412,
        ),
        ("PUT", "/child/file", String::new(), &b"abcde"[..], 201),
        ("DELETE", "/child", String::new(), &b""[..], 423),
        (
            "UNLOCK",
            "/",
            format!("Lock-Token: {root_token}\r\n"),
            &b""[..],
            204,
        ),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        assert_eq!(response(&mut stream)?.0, expected, "{method} {path}");
    }
    let mut stream = connect(address)?;
    write!(
        stream,
        "LOCK /child HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\nDepth: infinity\r\nTimeout: Second-60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        lock.len()
    )?;
    stream.write_all(lock)?;
    let (status, headers, _) = response(&mut stream)?;
    assert_eq!(status, 200);
    let child_token = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("lock-token"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .ok_or("missing child token")?;
    let mut stream = connect(address)?;
    write!(
        stream,
        "PUT /child/file HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    assert_eq!(response(&mut stream)?.0, 423);
    let mut stream = connect(address)?;
    write!(
        stream,
        "LOCK /child HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\nIf: ({child_token})\r\nTimeout: Second-1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    assert_eq!(response(&mut stream)?.0, 200);
    thread::sleep(Duration::from_millis(1100));
    let readonly=br#"<d:propertyupdate xmlns:d="DAV:" xmlns:p="urn:atomic"><d:set><d:prop><p:never>not stored</p:never><d:getetag>forbidden</d:getetag></d:prop></d:set></d:propertyupdate>"#;
    let deep = format!("{}{}", "<a>".repeat(33), "</a>".repeat(33));
    let shared=br#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:shared/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
    for (method, path, headers, body, expected) in [
        ("PUT", "/child/file", "", &b"newvalue"[..], 204),
        (
            "PUT",
            "/child/file",
            "If-Match: \"wrong\"\r\n",
            &b"not committed"[..],
            412,
        ),
        ("GET", "/child/file", "Range: bytes=1-3\r\n", &b""[..], 206),
        ("PROPPATCH", "/child/file", "", readonly.as_slice(), 207),
        ("PROPPATCH", "/child/file", "", deep.as_bytes(), 413),
        (
            "PROPFIND",
            "/child",
            "",
            &b"<!DOCTYPE x [<!ENTITY e SYSTEM 'file:///not-readable'>]><x>&e;</x>"[..],
            400,
        ),
        ("MKCOL", "/body", "", &b"body"[..], 415),
        ("LOCK", "/child/file", "", shared.as_slice(), 501),
        (
            "COPY",
            "/child/file",
            "Destination: /child/copied\r\n",
            &b""[..],
            201,
        ),
        (
            "COPY",
            "/child/file",
            "Destination: /child/copied\r\nOverwrite: F\r\n",
            &b""[..],
            412,
        ),
        (
            "COPY",
            "/child",
            "Destination: /another\r\nDepth: 1\r\n",
            &b""[..],
            400,
        ),
        (
            "MOVE",
            "/child/file",
            "Destination: /child/moved\r\nDepth: 0\r\n",
            &b""[..],
            400,
        ),
        ("POST", "/child/file", "", &b""[..], 405),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        let (status, _, body) = response(&mut stream)?;
        assert_eq!(status, expected, "{method} {path}");
        if method == "GET" {
            assert_eq!(body, b"ewv");
        }
        if method == "PROPPATCH" && status == 207 {
            let body = String::from_utf8(body)?;
            assert!(body.contains("403 Forbidden"));
            assert!(body.contains("424 Failed Dependency"));
        }
    }
    assert_eq!(fs::read(root.join("child/file"))?, b"newvalue");
    assert!(!root.join("direct").exists());
    assert!(!root.join("body").exists());
    let records = fs::read_dir(root.join(".rill-dav"))?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| fs::read_to_string(entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(records.iter().all(|record| !record.contains("urn:atomic")));
    Ok(())
}
