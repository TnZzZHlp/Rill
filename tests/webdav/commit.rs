use super::support::request;
use crate::{
    reload_support::Fixture,
    support::{TestResult, head, response},
};
use std::{fs, io::Write, os::unix::fs::PermissionsExt};

#[test]
fn put_commit_failure_preserves_target_and_leftover_marker_is_fail_closed_until_explicit_recovery()
-> TestResult {
    let mut fixture = Fixture::new()?;
    let root = fixture.dir.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), b"old target")?;
    let token = fixture.dir.join("token");
    fs::write(&token, "commit-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let config = format!(
        r#"server {{http "{}";control-socket "{}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";webdav root="{}" storage-id="documents";}};}};}}"#,
        fixture.address,
        fixture.socket.display(),
        token.display(),
        root.display()
    );
    fs::write(&fixture.entry, &config)?;
    fixture.start()?;
    let mut upload = fixture.connect()?;
    upload.write_all(b"PUT /file HTTP/1.1\r\nHost: app.test\r\nX-Token: commit-fixture\r\nExpect: 100-continue\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut upload)?.starts_with("HTTP/1.1 100 "));
    let temporary = fs::read_dir(&root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".rill-dav-put-"))
        })
        .ok_or("temporary missing")?;
    fs::rename(&temporary, root.join("detached-private"))?;
    upload.write_all(b"new!")?;
    assert_eq!(response(&mut upload)?.0, 404);
    assert_eq!(fs::read(root.join("file"))?, b"old target");
    assert_eq!(
        request(
            fixture.address,
            "GET",
            "/detached-private",
            "X-Token: commit-fixture\r\n",
            b""
        )?
        .0,
        403
    );
    fixture.signal()?;
    let marked = fs::OpenOptions::new()
        .write(true)
        .open(root.join("detached-private"))?;
    fs::remove_file(root.join("detached-private"))?;
    let file = fs::OpenOptions::new().write(true).open(root.join("file"))?;
    let mut value = [0_u8; 32];
    let length = rustix::fs::fgetxattr(&marked, c"user.rill.webdav.private", &mut value[..])?;
    rustix::fs::fsetxattr(
        &file,
        c"user.rill.webdav.private",
        &value[..length],
        rustix::fs::XattrFlags::CREATE,
    )?;
    file.sync_all()?;
    let static_only = format!(
        "server {{http \"{}\";control-socket \"{}\";}}\nsite \"app.test\" {{root \"{}\";}}",
        fixture.address,
        fixture.socket.display(),
        root.display()
    );
    fs::write(&fixture.entry, static_only)?;
    fixture.start()?;
    assert_eq!(request(fixture.address, "GET", "/file", "", b"")?.0, 403);
    assert_eq!(fs::read(root.join("file"))?, b"old target");
    rustix::fs::fremovexattr(&file, c"user.rill.webdav.private")?;
    file.sync_all()?;
    assert_eq!(
        request(fixture.address, "GET", "/file", "", b"")?.2,
        b"old target"
    );
    fs::create_dir(root.join("execute-only"))?;
    fs::write(root.join("execute-only/child"), b"readable child")?;
    fs::set_permissions(root.join("execute-only"), fs::Permissions::from_mode(0o111))?;
    assert_eq!(
        request(fixture.address, "GET", "/execute-only/child", "", b"")?.2,
        b"readable child"
    );
    fs::set_permissions(root.join("execute-only"), fs::Permissions::from_mode(0o755))?;
    Ok(())
}
