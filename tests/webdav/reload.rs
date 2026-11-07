use crate::{
    reload_support::Fixture,
    support::{TestResult, head, response},
};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, thread, time::Duration};

#[test]
fn webdav_registry_survives_removal_failed_activation_and_reload_during_put_and_move() -> TestResult
{
    let mut fixture = Fixture::new()?;
    let root = fixture.dir.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("docs"))?;
    fs::write(root.join("docs/file"), b"old")?;
    let token = fixture.dir.join("token");
    fs::write(&token, "reload-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let config = format!(
        r#"server {{ http "{}"; control-socket "{}"; }}
site "example.test" {{ chain {{ rule {{ static-token header="X-Token" file="{}"; header "response" {{set "X-Policy" "old";}}; webdav root="{}" storage-id="documents"; }}; }}; }}"#,
        fixture.address,
        fixture.socket.display(),
        token.display(),
        root.display()
    );
    fs::write(&fixture.entry, &config)?;
    fixture.start()?;
    let lock=br#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
    let mut stream = fixture.connect()?;
    write!(
        stream,
        "LOCK /docs HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        lock.len()
    )?;
    stream.write_all(lock)?;
    let (status, headers, _) = response(&mut stream)?;
    assert_eq!(status, 200);
    let lock_token = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("lock-token"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .ok_or("missing lock token")?;
    let property=br#"<d:propertyupdate xmlns:d="DAV:" xmlns:p="urn:persist"><d:set><d:prop><p:value>retained</p:value></d:prop></d:set></d:propertyupdate>"#;
    let mut stream = fixture.connect()?;
    write!(
        stream,
        "PROPPATCH /docs/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nIf: ({lock_token})\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        property.len()
    )?;
    stream.write_all(property)?;
    assert_eq!(response(&mut stream)?.0, 207);
    let held = fixture.dir.join("held-root");
    fs::rename(&root, &held)?;
    fs::create_dir(&root)?;
    let replaced = fixture.cli(&[])?;
    assert_eq!(replaced.status.code(), Some(1));
    assert!(String::from_utf8(replaced.stderr)?.contains("RESTART_REQUIRED"));
    let mut stream = fixture.connect()?;
    stream.write_all(b"GET /docs/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.2, b"old");
    fs::remove_dir(&root)?;
    fs::rename(&held, &root)?;
    let alternative = fixture.dir.join("alternative");
    fs::create_dir(&alternative)?;
    fs::write(
        &fixture.entry,
        config.replace(
            &root.to_string_lossy().to_string(),
            &alternative.to_string_lossy(),
        ),
    )?;
    let failed = fixture.cli(&[])?;
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8(failed.stderr)?.contains("RESTART_REQUIRED"));
    fs::write(
        &fixture.entry,
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{chain {{rule {{respond 404;}};}};}}",
            fixture.address,
            fixture.socket.display()
        ),
    )?;
    assert!(fixture.cli(&[])?.status.success());
    fs::write(
        &fixture.entry,
        config.replace(
            &root.to_string_lossy().to_string(),
            &alternative.to_string_lossy(),
        ),
    )?;
    assert_eq!(fixture.cli(&[])?.status.code(), Some(1));
    fs::write(&fixture.entry, &config)?;
    assert!(fixture.cli(&[])?.status.success());
    let new_root = fixture.dir.join("candidate");
    fs::create_dir(&new_root)?;
    let candidate = format!(
        "{}\nsite \"extra.test\" {{chain {{rule {{static-token header=\"X-Token\" file=\"{}\"; webdav root=\"{}\" storage-id=\"candidate\";}};}};}}",
        config.replace(
            &format!("http \"{}\"", fixture.address),
            "http \"127.0.0.1:1\""
        ),
        token.display(),
        new_root.display()
    );
    fs::write(&fixture.entry, candidate)?;
    assert_eq!(fixture.cli(&[])?.status.code(), Some(1));
    assert!(!new_root.join(".rill-dav").exists());
    assert!(root.join(".rill-dav/.owner").exists());
    let mut stream = fixture.connect()?;
    stream.write_all(b"PUT /docs/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 423);
    fs::write(&fixture.entry, &config)?;
    let mut upload = fixture.connect()?;
    write!(
        upload,
        "PUT /docs/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nIf: ({lock_token})\r\nExpect: 100-continue\r\nContent-Length: 8\r\nConnection: close\r\n\r\n"
    )?;
    assert!(head(&mut upload)?.starts_with("HTTP/1.1 100 "));
    upload.write_all(b"new")?;
    fs::write(&fixture.entry, config.replace("\"old\"", "\"new\""))?;
    assert!(fixture.cli(&[])?.status.success());
    let mut moving = fixture.connect()?;
    write!(
        moving,
        "MOVE /docs HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nIf: ({lock_token})\r\nDestination: /renamed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    thread::sleep(Duration::from_millis(100));
    assert!(root.join("docs").exists());
    assert!(!root.join("renamed").exists());
    upload.write_all(b"data!")?;
    let (status, headers, _) = response(&mut upload)?;
    assert_eq!(status, 204);
    assert!(headers.to_ascii_lowercase().contains("x-policy: old"));
    let (status, headers, _) = response(&mut moving)?;
    assert_eq!(status, 201);
    assert!(headers.to_ascii_lowercase().contains("x-policy: new"));
    assert_eq!(fs::read(root.join("renamed/file"))?, b"newdata!");
    let records = fs::read_dir(root.join(".rill-dav"))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    let record = records
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(record.iter().any(|bytes| {
        serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|value| {
            value["path"] == "renamed/file"
                && value["properties"].to_string().contains("urn:persist")
        })
    }));
    let mut stream = fixture.connect()?;
    write!(
        stream,
        "LOCK /renamed/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nDepth: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        lock.len()
    )?;
    stream.write_all(lock)?;
    assert_eq!(response(&mut stream)?.0, 200);
    fixture.signal()?;
    fixture.start()?;
    let mut stream = fixture.connect()?;
    stream.write_all(b"PUT /renamed/file HTTP/1.1\r\nHost: example.test\r\nX-Token: reload-fixture\r\nContent-Length: 7\r\nConnection: close\r\n\r\nrestart")?;
    assert_eq!(response(&mut stream)?.0, 204);
    assert_eq!(fs::read(root.join("renamed/file"))?, b"restart");
    let log = String::from_utf8(fixture.log_bytes()?)?;
    assert!(!log.contains(&lock_token));
    assert!(!log.contains("reload-fixture"));
    assert!(!log.contains("urn:persist"));
    Ok(())
}
