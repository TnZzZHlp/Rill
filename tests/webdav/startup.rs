use crate::support::{Directory, Proxy, TestResult, vacant};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::Command,
};

#[test]
fn webdav_failed_listener_or_control_startup_rolls_back_new_metadata_but_never_existing_properties()
-> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let control = directory.0.join("control");
    fs::create_dir(&control)?;
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700))?;
    let token = directory.0.join("token");
    fs::write(&token, "startup-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let address = occupied.local_addr()?;
    let config = format!(
        r#"server {{http "{address}";control-socket "{}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";webdav root="{}" storage-id="documents";}};}};}}"#,
        control.join("rill.sock").display(),
        token.display(),
        root.display()
    );
    let entry = directory.0.join("entry.kdl");
    fs::write(&entry, &config)?;
    let failed = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["run", "--config"])
        .arg(&entry)
        .output()?;
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8(failed.stderr)?.contains("listen_failed"));
    assert!(!root.join(".rill-dav").exists());
    assert!(!control.join("rill.sock").exists());
    drop(occupied);
    let mut proxy = Proxy::start(address, &config)?;
    assert!(root.join(".rill-dav/.owner").exists());
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    drop(proxy);
    let before = fs::metadata(root.join(".rill-dav/.owner"))?;
    let contents = fs::read(root.join(".rill-dav/.owner"))?;
    let next = vacant()?;
    let unsafe_control = directory.0.join("unsafe-control");
    fs::create_dir(&unsafe_control)?;
    fs::set_permissions(&unsafe_control, fs::Permissions::from_mode(0o755))?;
    let bad = config
        .replace(&address.to_string(), &next.to_string())
        .replace(
            &control.join("rill.sock").to_string_lossy().to_string(),
            &unsafe_control.join("rill.sock").to_string_lossy(),
        );
    fs::write(&entry, &bad)?;
    let failed = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["run", "--config"])
        .arg(&entry)
        .output()?;
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8(failed.stderr)?.contains("control_listen_failed"));
    let after = fs::metadata(root.join(".rill-dav/.owner"))?;
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    assert_eq!(fs::read(root.join(".rill-dav/.owner"))?, contents);
    assert!(!unsafe_control.join("rill.sock").exists());
    fs::set_permissions(&unsafe_control, fs::Permissions::from_mode(0o700))?;
    let mut proxy = Proxy::start(next, &bad)?;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}
