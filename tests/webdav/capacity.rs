use crate::support::{Directory, Proxy, TestResult, connect, head, response, vacant};
use std::{fs, io::Write, net::Shutdown, os::unix::fs::PermissionsExt};

#[test]
fn propfind_namespace_expansion_fails_before_headers_and_recovers_without_a_name_limit()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), b"preserved")?;
    let token = directory.0.join("token");
    fs::write(&token, "namespace-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        "server {{http \"{address}\";}}\nsite \"app.test\" {{chain {{rule {{static-token header=\"X-Token\" file=\"{}\";webdav root=\"{}\" storage-id=\"documents\";}};}};}}",
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let names = (0..64).map(|n| format!("<p:n{n}/>")).collect::<String>();
    let large = format!(
        "<d:propfind xmlns:d=\"DAV:\" xmlns:p=\"{}\"><d:prop>{names}</d:prop></d:propfind>",
        "&amp;".repeat(160 * 1024)
    );
    assert!(large.len() < 1024 * 1024);
    for _ in 0..3 {
        let result = super::support::request(
            address,
            "PROPFIND",
            "/file",
            "X-Token: namespace-fixture\r\nDepth: 0\r\n",
            large.as_bytes(),
        )?;
        assert_eq!(
            result.0, 503,
            "expanded missing properties must fail before 207"
        );
    }
    let names = (0..65).map(|n| format!("<p:n{n}/>")).collect::<String>();
    let normal = format!(
        "<d:propfind xmlns:d=\"DAV:\" xmlns:p=\"{}\"><d:prop>{names}</d:prop></d:propfind>",
        "z".repeat(8192)
    );
    let result = super::support::request(
        address,
        "PROPFIND",
        "/file",
        "X-Token: namespace-fixture\r\nDepth: 0\r\n",
        normal.as_bytes(),
    )?;
    assert_eq!(result.0, 207);
    let body = String::from_utf8(result.2)?;
    assert_eq!(body.matches("xmlns:p=").count(), 65);
    assert!(body.contains("HTTP/1.1 404 Not Found"));
    assert!(body.contains("<p:n64 "));
    assert_eq!(fs::read(root.join("file"))?, b"preserved");
    Ok(())
}

#[test]
fn webdav_real_upload_workers_hold_all_sixteen_permits_until_cancel_cleanup_and_properties_are_bounded()
-> TestResult {
    let directory = Directory::new()?;
    let token = directory.0.join("token");
    fs::write(&token, "capacity-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let mut branches = String::new();
    for index in 0..17 {
        let root = directory.0.join(format!("root-{index}"));
        fs::create_dir(&root)?;
        branches.push_str(&format!("when {{match {{path-glob \"/s{index}/*\";}};strip-prefix \"/s{index}\";webdav root=\"{}\" storage-id=\"storage-{index}\";}};",root.display()));
    }
    let config = format!(
        "server {{http \"{address}\";}}\nsite \"app.test\" {{chain {{rule {{static-token header=\"X-Token\" file=\"{}\";branch {{{branches}}};}};}};}}",
        token.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut uploads = Vec::new();
    for index in 0..16 {
        let mut stream = connect(address)?;
        write!(
            stream,
            "PUT /s{index}/file HTTP/1.1\r\nHost: app.test\r\nX-Token: capacity-fixture\r\nExpect: 100-continue\r\nContent-Length: 10\r\nConnection: close\r\n\r\n"
        )?;
        assert!(head(&mut stream)?.starts_with("HTTP/1.1 100 "));
        stream.write_all(b"part")?;
        uploads.push(stream);
    }
    let mut stream = connect(address)?;
    stream.write_all(b"PUT /s16/file HTTP/1.1\r\nHost: app.test\r\nX-Token: capacity-fixture\r\nExpect: 100-continue\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    for stream in uploads {
        stream.shutdown(Shutdown::Both)?;
    }
    for index in 0..16 {
        let mut stream = connect(address)?;
        write!(
            stream,
            "OPTIONS /s{index}/ HTTP/1.1\r\nHost: app.test\r\nX-Token: capacity-fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, 200);
        assert!(
            fs::read_dir(directory.0.join(format!("root-{index}")))?.all(|entry| !entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".rill-dav-put-"))
        );
    }
    let mut stream = connect(address)?;
    stream.write_all(b"PUT /s16/file HTTP/1.1\r\nHost: app.test\r\nX-Token: capacity-fixture\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfull")?;
    assert_eq!(response(&mut stream)?.0, 201);
    let mut names = String::new();
    for index in 0..64 {
        names.push_str(&format!("<p:n{index}>v</p:n{index}>"));
    }
    let initial = format!(
        "<d:propertyupdate xmlns:d=\"DAV:\" xmlns:p=\"urn:quota\"><d:set><d:prop>{names}</d:prop></d:set></d:propertyupdate>"
    );
    let extra=b"<d:propertyupdate xmlns:d=\"DAV:\" xmlns:p=\"urn:quota\"><d:set><d:prop><p:overflow>v</p:overflow></d:prop></d:set></d:propertyupdate>";
    let big = format!(
        "<d:propertyupdate xmlns:d=\"DAV:\" xmlns:p=\"urn:quota\"><d:set><d:prop><p:n0>{}</p:n0></d:prop></d:set></d:propertyupdate>",
        "z".repeat(65536)
    );
    for (body, expected) in [
        (initial.as_bytes(), 207),
        (extra.as_slice(), 413),
        (big.as_bytes(), 413),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "PROPPATCH /s16/file HTTP/1.1\r\nHost: app.test\r\nX-Token: capacity-fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        assert_eq!(response(&mut stream)?.0, expected);
    }
    let record = fs::read_dir(directory.0.join("root-16/.rill-dav"))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .ok_or("missing property record")?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(record)?)?;
    let properties = value["properties"].as_array().ok_or("missing properties")?;
    assert_eq!(properties.len(), 64);
    assert!(properties.iter().all(
        |value| !value.to_string().contains("overflow") && !value.to_string().contains("zzzz")
    ));
    Ok(())
}
