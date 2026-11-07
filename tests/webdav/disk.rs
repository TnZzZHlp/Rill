use crate::support::{Directory, Proxy, TestResult, connect, head, vacant};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[test]
#[ignore = "requires root and an isolated private mount namespace with a 1MiB tmpfs"]
fn isolated_tmpfs_enospc_returns_507_preserves_target_and_cleans_temporary() -> TestResult {
    if std::env::var_os("RILL_DAV_ENOSPC_CHILD").is_none() {
        if !nix::unistd::geteuid().is_root() {
            return Err("explicit ENOSPC fixture requires root".into());
        }
        let directory = Directory::new()?;
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))?;
        let mountpoint = directory.0.join("mount");
        fs::create_dir(&mountpoint)?;
        let namespace = fs::read_link("/proc/self/ns/mnt")?;
        let output=Command::new("/usr/bin/unshare").args(["--mount","--fork"]).arg(std::env::current_exe()?).args(["--exact","webdav::disk::isolated_tmpfs_enospc_returns_507_preserves_target_and_cleans_temporary","--ignored","--nocapture","--test-threads=1"]).env("RILL_DAV_ENOSPC_CHILD","1").env("RILL_DAV_ENOSPC_PARENT",namespace).env("RILL_DAV_ENOSPC_MOUNT",&mountpoint).output()?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        if !output.status.success() {
            return Err(format!(
                "isolated ENOSPC fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        return Ok(());
    }
    let parent = std::env::var_os("RILL_DAV_ENOSPC_PARENT").ok_or("missing parent namespace")?;
    let current = fs::read_link("/proc/self/ns/mnt")?;
    if current.as_os_str() == parent {
        return Err("refusing mount in parent namespace".into());
    }
    let mountpoint = std::path::PathBuf::from(
        std::env::var_os("RILL_DAV_ENOSPC_MOUNT").ok_or("missing mountpoint")?,
    );
    if !Command::new("/usr/bin/mount")
        .args(["--make-rprivate", "/"])
        .status()?
        .success()
    {
        return Err("private propagation setup failed".into());
    }
    if fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .any(|line| {
            line.split_whitespace()
                .skip(6)
                .take_while(|field| *field != "-")
                .any(|field| {
                    field.starts_with("shared:")
                        || field.starts_with("master:")
                        || field.starts_with("propagate_from:")
                })
        })
    {
        return Err("mount propagation is not private".into());
    }
    if !Command::new("/usr/bin/mount")
        .args([
            "-t",
            "tmpfs",
            "-o",
            "size=1m,nr_inodes=128,mode=0700",
            "rill-dav-enospc",
        ])
        .arg(&mountpoint)
        .status()?
        .success()
    {
        return Err("isolated tmpfs mount failed".into());
    }
    fs::write(mountpoint.join("file"), b"old target")?;
    let directory = Directory::new()?;
    let token = directory.0.join("token");
    fs::write(&token, "disk-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";webdav root="{}" storage-id="documents";}};}};}}"#,
        token.display(),
        mountpoint.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let stats = nix::sys::statvfs::statvfs(&mountpoint)?;
    let available = stats.blocks_available() * stats.fragment_size();
    let mut stream = connect(address)?;
    stream.write_all(b"PUT /file HTTP/1.1\r\nHost: app.test\r\nX-Token: disk-fixture\r\nExpect: 100-continue\r\nContent-Length: 2097152\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut stream)?.starts_with("HTTP/1.1 100 "));
    let mut writer = stream.try_clone()?;
    let stop = Arc::new(AtomicBool::new(false));
    let writing = stop.clone();
    let upload = thread::spawn(move || {
        let block = [b'x'; 65536];
        for _ in 0..32 {
            if writing.load(Ordering::Acquire) {
                break;
            }
            if writer.write_all(&block).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let result = super::support::response(&mut stream, "PUT");
    stop.store(true, Ordering::Release);
    upload.join().map_err(|_| "upload thread failed")?;
    assert_eq!(result?.0, 507);
    assert_eq!(
        super::support::request(address, "GET", "/file", "X-Token: disk-fixture\r\n", b"")?.2,
        b"old target"
    );
    assert_eq!(fs::read(mountpoint.join("file"))?, b"old target");
    assert!(fs::read_dir(&mountpoint)?.all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".rill-dav-put-")
    }));
    let stats = nix::sys::statvfs::statvfs(&mountpoint)?;
    let restored = stats.blocks_available() * stats.fragment_size();
    assert!(restored + 4096 >= available);
    println!(
        "isolated tmpfs size=1048576: status=507 old_target_preserved=true temp_cleanup=true available_before={available} available_after={restored}"
    );
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    drop(proxy);
    if fs::read_link("/proc/self/ns/mnt")?.as_os_str() == parent
        || fs::read_to_string("/proc/self/mountinfo")?
            .lines()
            .any(|line| {
                line.split_whitespace()
                    .skip(6)
                    .take_while(|field| *field != "-")
                    .any(|field| {
                        field.starts_with("shared:")
                            || field.starts_with("master:")
                            || field.starts_with("propagate_from:")
                    })
            })
    {
        return Err("refusing bind outside isolated private namespace".into());
    }
    let private = mountpoint.join("private");
    let public = mountpoint.join("public");
    let public_file = mountpoint.join("public-file");
    fs::create_dir(&private)?;
    fs::create_dir(&public)?;
    fs::write(private.join("file"), b"bind-protected")?;
    fs::write(&public_file, b"placeholder")?;
    for (source, target) in [(&private, &public), (&private.join("file"), &public_file)] {
        if !Command::new("/usr/bin/mount")
            .arg("--bind")
            .arg(source)
            .arg(target)
            .status()?
            .success()
        {
            return Err("isolated bind mount failed".into());
        }
    }
    let original = fs::metadata(private.join("file"))?;
    for alias in [public.join("file"), public_file.clone()] {
        let metadata = fs::metadata(alias)?;
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(metadata.nlink(), 1);
    }
    let original_directory = fs::metadata(&private)?;
    let alias_directory = fs::metadata(&public)?;
    assert_eq!(
        (original_directory.dev(), original_directory.ino()),
        (alias_directory.dev(), alias_directory.ino())
    );
    let mut proxy = Proxy::start(address, &config)?;
    for alias in ["/public/file", "/public-file"] {
        for (method, body) in [("GET", &b""[..]), ("PUT", &b"not committed"[..])] {
            let result =
                super::support::request(address, method, alias, "X-Token: disk-fixture\r\n", body)?;
            assert_eq!(result.0, 409, "{method} {alias}");
        }
    }
    assert_eq!(fs::read(private.join("file"))?, b"bind-protected");
    println!(
        "isolated bind aliases: directory_same_identity=true file_same_identity=true file_nlink=1 DAV_GET_PUT=409 original_preserved=true"
    );
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    drop(proxy);
    for target in [&public_file, &public] {
        if !Command::new("/usr/bin/umount")
            .arg(target)
            .status()?
            .success()
        {
            return Err("isolated bind unmount failed".into());
        }
    }
    if !Command::new("/usr/bin/umount")
        .arg(&mountpoint)
        .status()?
        .success()
    {
        return Err("isolated tmpfs unmount failed".into());
    }
    Ok(())
}
