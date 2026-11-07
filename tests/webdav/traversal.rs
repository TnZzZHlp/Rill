use crate::support::{Directory, Proxy, TestResult, connect, response, vacant};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
};

#[test]
fn webdav_propfind_enforces_resource_and_depth_boundaries_without_exposing_internal_names()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let token = directory.0.join("token");
    fs::write(&token, "traversal-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{match {{all {{method "PROPFIND";path-glob "/dav/*";}};}};static-token header="X-Token" file="{}";strip-prefix "/dav";webdav root="{}" storage-id="documents";}};}};}}"#,
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    for index in 0..4094 {
        fs::write(root.join(format!("item-{index}")), b"x")?;
    }
    let mut stream = connect(address)?;
    stream.write_all(b"PROPFIND /dav/ HTTP/1.1\r\nHost: app.test\r\nX-Token: traversal-fixture\r\nDepth: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    let (status, _, _) = response(&mut stream)?;
    assert_eq!(status, 207);
    let mut raw = Vec::new();
    stream.take(4 * 1024 * 1024 + 1).read_to_end(&mut raw)?;
    assert!(raw.len() <= 4 * 1024 * 1024);
    let raw = String::from_utf8(raw)?;
    assert!(raw.contains("/dav/item-4093</d:href>"));
    assert!(raw.contains("</d:multistatus>"));
    assert!(!raw.contains(".rill-dav"));
    fs::write(root.join("overflow"), b"x")?;
    let mut stream = connect(address)?;
    stream.write_all(b"PROPFIND /dav/ HTTP/1.1\r\nHost: app.test\r\nX-Token: traversal-fixture\r\nDepth: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    for index in 0..4094 {
        fs::remove_file(root.join(format!("item-{index}")))?;
    }
    fs::remove_file(root.join("overflow"))?;
    let mut deepest = root.clone();
    for _ in 0..16 {
        deepest = deepest.join("d");
        fs::create_dir(&deepest)?;
    }
    let mut stream = connect(address)?;
    stream.write_all(b"PROPFIND /dav/ HTTP/1.1\r\nHost: app.test\r\nX-Token: traversal-fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 207);
    let mut sink = Vec::new();
    stream.take(4 * 1024 * 1024 + 1).read_to_end(&mut sink)?;
    assert!(sink.len() <= 4 * 1024 * 1024);
    fs::write(deepest.join("too-deep"), b"x")?;
    let mut stream = connect(address)?;
    stream.write_all(b"PROPFIND /dav/ HTTP/1.1\r\nHost: app.test\r\nX-Token: traversal-fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    Ok(())
}

#[test]
fn copy_overwrite_source_and_old_target_share_one_enumeration_budget_before_any_delete()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("source"))?;
    fs::create_dir(root.join("target"))?;
    for index in 0..2100 {
        fs::write(root.join(format!("source/file-{index}")), b"source")?;
        fs::write(root.join(format!("target/file-{index}")), b"old target")?;
    }
    let token = directory.0.join("token");
    fs::write(&token, "shared-ledger")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";webdav root="{}" storage-id="documents";}};}};}}"#,
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    assert_eq!(
        super::support::request(
            address,
            "COPY",
            "/source",
            "X-Token: shared-ledger\r\nDestination: /target\r\n",
            b""
        )?
        .0,
        503
    );
    for index in 0..2100 {
        assert_eq!(
            fs::read(root.join(format!("source/file-{index}")))?,
            b"source"
        );
        assert_eq!(
            fs::read(root.join(format!("target/file-{index}")))?,
            b"old target"
        );
    }
    Ok(())
}

#[test]
fn webdav_complex_destination_proofs_share_the_request_step_budget_before_any_copy_mutation()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("source"))?;
    for index in 0..300 {
        fs::write(root.join(format!("source/file-{index}")), b"original")?;
    }
    let token = directory.0.join("token");
    fs::write(&token, "proof-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";branch {{when {{match {{method "COPY";}};webdav root="{}" storage-id="documents";}};otherwise {{webdav root="{}" storage-id="documents";}};}};}};}};}}"#,
        token.display(),
        root.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"COPY /source HTTP/1.1\r\nHost: app.test\r\nX-Token: proof-fixture\r\nDestination: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    assert!(!root.join("target").exists());
    for index in 0..300 {
        assert_eq!(
            fs::read(root.join(format!("source/file-{index}")))?,
            b"original"
        );
    }
    Ok(())
}
