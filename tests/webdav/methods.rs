use crate::support::{Directory, Proxy, TestResult, connect, head, response, vacant};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
};

#[test]
fn webdav_real_methods_atomic_properties_exclusive_lock_and_head() -> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let token = directory.0.join("token");
    fs::write(&token, "method-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ static-token header="X-Token" file="{}"; webdav root="{}" storage-id="documents"; }}; }}; }}"#,
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let patch=br#"<d:propertyupdate xmlns:d="DAV:" xmlns:p="urn:example"><d:set><d:prop><p:note p:kind="a&amp;b">text&lt;<p:child/>tail</p:note></d:prop></d:set></d:propertyupdate>"#;
    for (method, path, headers, body, expected) in [
        ("OPTIONS", "/", "", &b""[..], 200),
        ("PUT", "/file.txt", "", &b"original"[..], 201),
        ("GET", "/file.txt", "", &b""[..], 200),
        ("MKCOL", "/collection", "", &b""[..], 201),
        ("PUT", "/%252e", "", &b"encoded"[..], 201),
        ("PROPPATCH", "/%252e", "", patch.as_slice(), 207),
        (
            "COPY",
            "/%252e",
            "Destination: /encoded-copy\r\n",
            &b""[..],
            201,
        ),
        (
            "PROPPATCH",
            "/file.txt",
            "Content-Type: application/xml\r\n",
            patch.as_slice(),
            207,
        ),
        (
            "COPY",
            "/file.txt",
            "Destination: http://app.test/collection/copy.txt\r\n",
            &b""[..],
            201,
        ),
        (
            "MOVE",
            "/collection/copy.txt",
            "Destination: /collection/moved.txt\r\n",
            &b""[..],
            201,
        ),
        (
            "COPY",
            "/collection",
            "Destination: /duplicate\r\n",
            &b""[..],
            201,
        ),
        (
            "COPY",
            "/collection",
            "Destination: /duplicate\r\n",
            &b""[..],
            204,
        ),
        (
            "MOVE",
            "/duplicate",
            "Destination: /renamed-tree\r\n",
            &b""[..],
            201,
        ),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nX-Token: method-fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        let (status, head, received) = response(&mut stream)?;
        assert_eq!(status, expected, "{method} {head}");
        if method == "GET" {
            assert_eq!(received, b"original");
        }
        if method == "OPTIONS" {
            assert!(head.to_ascii_lowercase().contains("dav: 1, 2"));
        }
    }
    let mut stream = connect(address)?;
    stream.write_all(b"HEAD /file.txt HTTP/1.1\r\nHost: app.test\r\nX-Token: method-fixture\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 200 "));
    assert!(headers.to_ascii_lowercase().contains("content-length: 8"));
    let mut extra = Vec::new();
    stream.read_to_end(&mut extra)?;
    assert!(extra.is_empty());
    let lock=br#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype><d:owner>test client</d:owner></d:lockinfo>"#;
    let mut stream = connect(address)?;
    write!(
        stream,
        "LOCK /file.txt HTTP/1.1\r\nHost: app.test\r\nX-Token: method-fixture\r\nDepth: 0\r\nTimeout: Second-60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        lock.len()
    )?;
    stream.write_all(lock)?;
    let (status, headers, body) = response(&mut stream)?;
    assert_eq!(status, 200);
    assert!(String::from_utf8(body)?.contains("<d:exclusive/>"));
    let lock_token = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("lock-token"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .ok_or("missing lock token")?;
    for (method, path, headers, body, expected) in [
        ("PUT", "/file.txt", String::new(), &b"blocked"[..], 423),
        ("PUT", "/%66ile.txt", String::new(), &b"blocked"[..], 423),
        (
            "LOCK",
            "/file.txt",
            format!("If: ({lock_token})\r\nTimeout: Second-10\r\n"),
            &b""[..],
            200,
        ),
        (
            "PUT",
            "/file.txt",
            format!("If: ({lock_token})\r\n"),
            &b"replacement"[..],
            204,
        ),
        (
            "UNLOCK",
            "/file.txt",
            "Lock-Token: <urn:uuid:wrong>\r\n".into(),
            &b""[..],
            409,
        ),
        (
            "UNLOCK",
            "/file.txt",
            format!("Lock-Token: {lock_token}\r\n"),
            &b""[..],
            204,
        ),
        ("DELETE", "/collection", String::new(), &b""[..], 204),
        ("DELETE", "/file.txt", String::new(), &b""[..], 204),
        (
            "PUT",
            "/file.txt",
            "If-None-Match: *\r\n".into(),
            &b"new"[..],
            201,
        ),
        (
            "PUT",
            "/file.txt",
            "If-None-Match: *\r\n".into(),
            &b"not committed"[..],
            412,
        ),
        ("DELETE", "/", String::new(), &b""[..], 403),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nX-Token: method-fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        let (status, headers, _) = response(&mut stream)?;
        assert_eq!(status, expected, "{method} {headers}");
    }
    assert_eq!(fs::read(root.join("file.txt"))?, b"new");
    assert!(fs::read_dir(&root)?.all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".rill-dav-put-")
    }));
    Ok(())
}

#[test]
fn webdav_canonical_source_proof_prevents_encoded_path_from_selecting_a_weaker_auth_action()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("private"))?;
    fs::write(root.join("private/file"), b"protected")?;
    fs::write(root.join("ordinary"), b"public-to-weak-policy")?;
    let token = directory.0.join("token");
    fs::write(&token, "source-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{
 rule {{match {{path-glob "/dav/private/*";}};static-token header="X-Strong" file="{}";strip-prefix "/dav";webdav root="{}" storage-id="documents";}};
 rule {{static-token header="X-Weak" file="{}";strip-prefix "/dav";webdav root="{}" storage-id="documents";}};
}};}}"#,
        token.display(),
        root.display(),
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    for (method, path, header, body, expected) in [
        (
            "GET",
            "/dav/private/file",
            "X-Weak: source-fixture\r\n",
            &b""[..],
            401,
        ),
        (
            "GET",
            "/dav/%70rivate/file",
            "X-Weak: source-fixture\r\n",
            &b""[..],
            403,
        ),
        (
            "GET",
            "/dav//private/file",
            "X-Weak: source-fixture\r\n",
            &b""[..],
            403,
        ),
        (
            "PUT",
            "/dav/%70rivate/file",
            "X-Weak: source-fixture\r\n",
            &b"not committed"[..],
            403,
        ),
        (
            "GET",
            "/dav/private/file",
            "X-Strong: source-fixture\r\n",
            &b""[..],
            200,
        ),
        (
            "GET",
            "/dav/ordinary",
            "X-Weak: source-fixture\r\n",
            &b""[..],
            200,
        ),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        assert_eq!(response(&mut stream)?.0, expected, "{method} {path}");
    }
    for tag in ["private", "%70rivate"] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "OPTIONS /dav/ordinary HTTP/1.1\r\nHost: app.test\r\nX-Weak: source-fixture\r\nIf: <http://app.test/dav/{tag}/file> (Not [\"probe\"])\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, 403, "tagged If {tag}");
    }
    assert_eq!(fs::read(root.join("private/file"))?, b"protected");
    Ok(())
}

#[test]
fn webdav_denies_destination_auth_bypass_symlinks_hardlinks_and_private_static_aliases()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let token = directory.0.join("token");
    fs::write(&token, "security-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    fs::write(root.join("file"), b"private source")?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; static-cache-bytes 65536; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-glob "/dav/*"; }}; static-token header="X-Token" file="{}"; strip-prefix "/dav"; webdav root="{}" storage-id="documents"; }};
 rule {{ match {{ path-glob "/other/*"; }}; static-token header="X-Other" file="{}"; strip-prefix "/other"; webdav root="{}" storage-id="documents"; }};
 rule {{ serve-files root="{}" {{ browse #true; }}; }};
}}; }}"#,
        token.display(),
        root.display(),
        token.display(),
        root.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    std::os::unix::fs::symlink(".rill-dav/.owner", root.join("marker-alias"))?;
    fs::hard_link(root.join(".rill-dav/.owner"), root.join("marker-hardlink"))?;
    std::os::unix::fs::symlink("file", root.join("symlink"))?;
    fs::hard_link(root.join("file"), root.join("hardlink"))?;
    for (method, path, headers, expected) in [
        ("GET", "/dav/file", "", 403),
        ("GET", "/dav/symlink", "", 403),
        ("GET", "/marker-alias", "", 403),
        ("GET", "/marker-hardlink", "", 403),
        ("GET", "/.rill-dav/.owner", "", 403),
        ("GET", "/%2erill-dav/.owner", "", 403),
        ("COPY", "/dav/hardlink", "Destination: /other/copy\r\n", 403),
        ("PUT", "/dav/.rill-dav-put-user", "", 403),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nX-Token: security-fixture\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, expected, "{method} {path}");
    }
    fs::remove_file(root.join("hardlink"))?;
    for (method, path, headers, expected) in [
        ("COPY", "/dav/file", "Destination: /other/copy\r\n", 403),
        ("COPY", "/dav/file", "Destination: /public-copy\r\n", 403),
        (
            "COPY",
            "/dav/file",
            "Destination: http://elsewhere.test/dav/copy\r\n",
            403,
        ),
        ("COPY", "/dav/file", "Destination: /dav/copy\r\n", 201),
        ("MOVE", "/dav/copy", "Destination: /dav/moved\r\n", 201),
        (
            "MOVE",
            "/dav/moved",
            "Destination: /dav/moved/child\r\n",
            403,
        ),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nX-Token: security-fixture\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, expected, "{method} {path}");
    }
    let property_file = fs::read_dir(root.join(".rill-dav"))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .ok_or("missing property file")?;
    fs::hard_link(property_file, root.join("property-alias"))?;
    let mut stream = connect(address)?;
    stream.write_all(
        b"GET /property-alias HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n",
    )?;
    assert_eq!(response(&mut stream)?.0, 403);
    fs::remove_file(root.join("property-alias"))?;
    fs::remove_file(root.join("marker-hardlink"))?;
    assert_eq!(fs::read(root.join("moved"))?, b"private source");
    assert!(!root.join("copy").exists());
    assert!(!root.join("public-copy").exists());
    Ok(())
}
