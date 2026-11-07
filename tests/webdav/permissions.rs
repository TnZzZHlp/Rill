use crate::{
    static_support::Fixture,
    support::{TestResult, connect, response},
};
use std::{fs, io::Write, os::unix::fs::PermissionsExt};

#[test]
fn webdav_file_and_parent_permissions_preserve_original_contents_and_properties() -> TestResult {
    let mut fixture = Fixture::new()?;
    let token = fixture.dir.join("token");
    fs::write(&token, "permission-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    fs::write(fixture.root.join("file"), b"readable but not writable")?;
    fs::set_permissions(fixture.root.join("file"), fs::Permissions::from_mode(0o444))?;
    fs::create_dir(fixture.root.join("readonly"))?;
    fs::write(fixture.root.join("readonly/child"), b"preserved child")?;
    fs::set_permissions(
        fixture.root.join("readonly"),
        fs::Permissions::from_mode(0o555),
    )?;
    if nix::unistd::geteuid().is_root() {
        let uid = Some(nix::unistd::Uid::from_raw(65534));
        let gid = Some(nix::unistd::Gid::from_raw(0));
        for path in [&fixture.root, &token] {
            nix::unistd::chown(path, uid, gid)?;
        }
    }
    let config = format!(
        r#"server {{http "{}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";webdav root="{}" storage-id="documents";}};}};}}"#,
        fixture.address,
        token.display(),
        fixture.root.display()
    );
    fixture.start_config(&config, true)?;
    let patch=br#"<d:propertyupdate xmlns:d="DAV:" xmlns:p="urn:permissions"><d:set><d:prop><p:denied>never</p:denied></d:prop></d:set></d:propertyupdate>"#;
    let lock=br#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
    for (method, path, headers, body, expected) in [
        ("GET", "/file", "", &b""[..], 200),
        ("PUT", "/file", "", &b"not committed"[..], 403),
        ("PROPPATCH", "/file", "", patch.as_slice(), 403),
        ("LOCK", "/file", "", lock.as_slice(), 403),
        ("PUT", "/readonly/new", "", &b"not committed"[..], 403),
        ("MKCOL", "/readonly/new", "", &b""[..], 403),
        ("DELETE", "/readonly/child", "", &b""[..], 403),
        (
            "MOVE",
            "/file",
            "Destination: /readonly/moved\r\n",
            &b""[..],
            403,
        ),
        (
            "COPY",
            "/file",
            "Destination: /readonly/copy\r\n",
            &b""[..],
            403,
        ),
    ] {
        let mut stream = connect(fixture.address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nX-Token: permission-fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(body)?;
        assert_eq!(response(&mut stream)?.0, expected, "{method} {path}");
    }
    assert_eq!(
        fs::read(fixture.root.join("file"))?,
        b"readable but not writable"
    );
    assert_eq!(
        fs::read(fixture.root.join("readonly/child"))?,
        b"preserved child"
    );
    assert!(!fixture.root.join("readonly/new").exists());
    fixture.signal()?;
    assert!(fixture.finish()?.success());
    fs::set_permissions(
        fixture.root.join("readonly"),
        fs::Permissions::from_mode(0o755),
    )?;
    Ok(())
}
