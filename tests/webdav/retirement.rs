use super::support::request;
use crate::{
    reload_support::Fixture,
    support::{TestResult, head},
};
use std::{fs, io::Write, net::Shutdown, os::unix::fs::PermissionsExt};

#[test]
fn private_hardlinks_remain_denied_after_cancel_property_replacement_and_static_only_restart()
-> TestResult {
    let mut fixture = Fixture::new()?;
    let root = fixture.dir.join("root");
    fs::create_dir(&root)?;
    let token = fixture.dir.join("token");
    fs::write(&token, "retirement-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let config = format!(
        r#"server {{http "{}";control-socket "{}";}}
site "app.test" {{chain {{rule {{match {{path-glob "/dav/*";}};static-token header="X-Token" file="{}";strip-prefix "/dav";webdav root="{}" storage-id="documents";}};rule {{serve-files root="{}" {{browse #true;}};}};}};}}"#,
        fixture.address,
        fixture.socket.display(),
        token.display(),
        root.display(),
        root.display()
    );
    fs::write(&fixture.entry, &config)?;
    fixture.start()?;
    let auth = "X-Token: retirement-fixture\r\n";
    assert_eq!(
        request(fixture.address, "PUT", "/dav/file", auth, b"original")?.0,
        201
    );
    let initial=br#"<d:propertyupdate xmlns:d="DAV:" xmlns:p="urn:retired"><d:set><d:prop><p:value>old</p:value></d:prop></d:set></d:propertyupdate>"#;
    assert_eq!(
        request(fixture.address, "PROPPATCH", "/dav/file", auth, initial)?.0,
        207
    );
    let record = fs::read_dir(root.join(".rill-dav"))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .ok_or("property record missing")?;
    fs::hard_link(&record, root.join("property-alias"))?;
    let replacement = std::str::from_utf8(initial)?.replace(">old<", ">new<");
    assert_eq!(
        request(
            fixture.address,
            "PROPPATCH",
            "/dav/file",
            auth,
            replacement.as_bytes()
        )?
        .0,
        207
    );
    let (status,_,body)=request(fixture.address,"PROPFIND","/dav/file",&format!("{auth}Depth: 0\r\n"),br#"<d:propfind xmlns:d="DAV:" xmlns:p="urn:retired"><d:prop><p:value/></d:prop></d:propfind>"#)?;
    assert_eq!(status, 207);
    let properties = String::from_utf8(body)?;
    assert!(properties.contains(">new<"));
    assert!(!properties.contains(">old<"));
    assert_eq!(
        request(fixture.address, "GET", "/property-alias", "", b"")?.0,
        403
    );
    assert_eq!(
        request(fixture.address, "GET", "/dav/property-alias", auth, b"")?.0,
        403
    );
    let mut upload = fixture.connect()?;
    upload.write_all(b"PUT /dav/file HTTP/1.1\r\nHost: app.test\r\nX-Token: retirement-fixture\r\nExpect: 100-continue\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut upload)?.starts_with("HTTP/1.1 100 "));
    upload.write_all(b"partial private body")?;
    let temporary = fs::read_dir(&root)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".rill-dav-put-"))
        })
        .ok_or("upload temporary missing")?;
    fs::hard_link(&temporary, root.join("cancel-alias"))?;
    upload.shutdown(Shutdown::Both)?;
    drop(upload);
    assert_eq!(
        request(fixture.address, "OPTIONS", "/dav/", auth, b"")?.0,
        200
    );
    assert!(!temporary.exists());
    assert_eq!(fs::read(root.join("file"))?, b"original");
    for path in ["/cancel-alias", "/property-alias"] {
        assert_eq!(request(fixture.address, "GET", path, "", b"")?.0, 403);
    }
    std::os::unix::fs::symlink(".rill-dav", root.join("directory-alias"))?;
    fixture.signal()?;
    let static_only = format!(
        "server {{http \"{}\";control-socket \"{}\";}}\nsite \"app.test\" {{root \"{}\";browse #true;}}",
        fixture.address,
        fixture.socket.display(),
        root.display()
    );
    fs::write(&fixture.entry, static_only)?;
    fixture.start()?;
    for path in [
        "/cancel-alias",
        "/property-alias",
        "/directory-alias/",
        "/directory-alias/.owner",
    ] {
        assert_eq!(
            request(fixture.address, "GET", path, "", b"")?.0,
            403,
            "private alias was readable after restart"
        );
    }
    assert_eq!(
        request(fixture.address, "GET", "/file", "", b"")?.2,
        b"original"
    );
    fs::remove_file(root.join("cancel-alias"))?;
    fs::remove_file(root.join("property-alias"))?;
    assert_eq!(request(fixture.address, "GET", "/", "", b"")?.0, 200);
    fixture.signal()?;
    let owner = fs::OpenOptions::new()
        .write(true)
        .open(root.join(".rill-dav/.owner"))?;
    rustix::fs::fremovexattr(&owner, c"user.rill.webdav.private")?;
    owner.sync_all()?;
    fs::write(&fixture.entry, &config)?;
    let failed = std::process::Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["run", "--config"])
        .arg(&fixture.entry)
        .output()?;
    assert_eq!(failed.status.code(), Some(1));
    assert!(root.join(".rill-dav/.owner").exists());
    Ok(())
}
