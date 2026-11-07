use crate::support::{Directory, Proxy, TestResult, connect, head, response, vacant};
use std::{
    fs,
    io::Write,
    net::Shutdown,
    os::unix::fs::PermissionsExt,
    thread,
    time::{Duration, Instant},
};

#[test]
fn webdav_cancelled_put_keeps_target_and_private_temporary_aliases_cannot_hit_static_cache()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("docs"))?;
    fs::write(root.join("docs/file"), b"original target")?;
    let token = directory.0.join("token");
    fs::write(&token, "cancellation-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}"; static-cache-bytes 65536;}}
site "app.test" {{chain {{
 rule {{match {{path-glob "/dav/*";}}; static-token header="X-Token" file="{}"; strip-prefix "/dav"; webdav root="{}" storage-id="documents";}};
 rule {{serve-files root="{}" {{browse #true;}};}};
}};}}"#,
        token.display(),
        root.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut upload = connect(address)?;
    upload.write_all(b"PUT /dav/docs/file HTTP/1.1\r\nHost: app.test\r\nX-Token: cancellation-fixture\r\nExpect: 100-continue\r\nContent-Length: 1048576\r\n\r\n")?;
    assert!(head(&mut upload)?.starts_with("HTTP/1.1 100 "));
    upload.write_all(b"private in-flight body")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let temporary = loop {
        if let Some(path) = fs::read_dir(root.join("docs"))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b".rill-dav-put-"))
            })
        {
            break path;
        }
        if Instant::now() >= deadline {
            return Err("PUT temporary not created".into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let name = temporary.file_name().ok_or("missing temporary name")?;
    std::os::unix::fs::symlink(name, root.join("docs/symlink-alias"))?;
    fs::hard_link(&temporary, root.join("docs/hardlink-alias"))?;
    for path in [
        format!("/docs/{}", name.to_string_lossy()),
        "/docs/symlink-alias".into(),
        "/docs/hardlink-alias".into(),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, 403);
    }
    fs::remove_file(root.join("docs/symlink-alias"))?;
    fs::remove_file(root.join("docs/hardlink-alias"))?;
    upload.shutdown(Shutdown::Both)?;
    drop(upload);
    let mut stream = connect(address)?;
    stream.write_all(b"GET /dav/docs/file HTTP/1.1\r\nHost: app.test\r\nX-Token: cancellation-fixture\r\nConnection: close\r\n\r\n")?;
    let (status, _, body) = response(&mut stream)?;
    assert_eq!(status, 200);
    assert_eq!(body, b"original target");
    assert!(!temporary.exists());
    assert_eq!(fs::read(root.join("docs/file"))?, b"original target");
    let mut stream = connect(address)?;
    stream.write_all(b"PUT /dav/docs/file HTTP/1.1\r\nHost: app.test\r\nX-Token: cancellation-fixture\r\nContent-Length: 268435457\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 413);
    assert!(fs::read_dir(root.join("docs"))?.all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".rill-dav-put-")
    }));
    Ok(())
}
