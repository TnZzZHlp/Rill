use crate::support::{Directory, Proxy, TestResult, vacant};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
#[ignore = "requires explicitly prepared isolated cadaver client and installed rclone"]
fn real_cadaver_and_rclone_use_loopback_only_with_temporary_credentials() -> TestResult {
    let client = std::env::var_os("RILL_DAV_CADAVER")
        .ok_or("set RILL_DAV_CADAVER to the approved extracted client")?;
    let libraries = std::env::var_os("RILL_DAV_CADAVER_LIBS")
        .ok_or("set RILL_DAV_CADAVER_LIBS to its extracted libraries")?;
    let directory = Directory::new()?;
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let home = directory.0.join("home");
    fs::create_dir(&home)?;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    let password = "isolated-client-fixture";
    let hash = bcrypt::hash(password, 10)?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "localhost" {{chain {{rule {{basic-auth realm="dav" {{user "alice" hash="{hash}";}};webdav root="{}" storage-id="documents";}};}};}}"#,
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    fs::write(
        home.join(".netrc"),
        format!("machine localhost login alice password {password}\n"),
    )?;
    fs::set_permissions(home.join(".netrc"), fs::Permissions::from_mode(0o600))?;
    let rc = home.join("empty.rc");
    fs::write(&rc, "")?;
    let input = directory.0.join("input.txt");
    let output = directory.0.join("cadaver.txt");
    let payload = b"real client interoperability\n";
    fs::write(&input, payload)?;
    let version = Command::new(&client)
        .arg("--version")
        .env("HOME", &home)
        .env("LD_LIBRARY_PATH", &libraries)
        .output()?;
    assert_eq!(version.status.code(), Some(255));
    let version = String::from_utf8(version.stdout)?;
    assert!(version.contains("cadaver 0.26"));
    println!("{}", version.trim());
    let url = format!("http://localhost:{}/", address.port());
    let script = format!(
        "ls\nmkcol collection\ncd collection\nput {} note.txt\npropset note.txt fixture retained\npropget note.txt fixture\nlock note.txt\nunlock note.txt\ncopy note.txt copied.txt\nmove copied.txt moved.txt\nget moved.txt {}\ndelete note.txt\ndelete moved.txt\ncd ..\nrmcol collection\nquit\n",
        input.display(),
        output.display()
    );
    let mut child = Command::new(&client)
        .args(["--rcfile"])
        .arg(&rc)
        .arg(&url)
        .env("HOME", &home)
        .env("LD_LIBRARY_PATH", &libraries)
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing client stdin")?
        .write_all(script.as_bytes())?;
    let result = child.wait_with_output()?;
    let raw = format!(
        "{}{}",
        String::from_utf8(result.stdout)?,
        String::from_utf8(result.stderr)?
    );
    let redact = regex::Regex::new(r"urn:uuid:[A-Za-z0-9-]+")?;
    let log = redact.replace_all(&raw, "<lock-token>");
    assert!(!log.contains(password));
    println!("{log}");
    assert!(result.status.success());
    assert!(!log.to_ascii_lowercase().contains("failed"));
    assert_eq!(fs::read(&output)?, payload);
    assert!(!root.join("collection").exists());
    assert!(log.contains("retained"));
    let version = Command::new("/usr/bin/rclone").arg("version").output()?;
    assert!(version.status.success());
    println!(
        "{}",
        String::from_utf8(version.stdout)?
            .lines()
            .next()
            .ok_or("missing rclone version")?
    );
    let destination = directory.0.join("rclone.txt");
    for arguments in [
        vec!["mkdir".into(), ":webdav:/rclone".into()],
        vec![
            "copyto".into(),
            input.to_string_lossy().into_owned(),
            ":webdav:/rclone/note.txt".into(),
        ],
        vec!["lsl".into(), ":webdav:/".into()],
        vec![
            "copyto".into(),
            ":webdav:/rclone/note.txt".into(),
            destination.to_string_lossy().into_owned(),
        ],
        vec![
            "copyto".into(),
            ":webdav:/rclone/note.txt".into(),
            ":webdav:/rclone/copied.txt".into(),
        ],
        vec![
            "moveto".into(),
            ":webdav:/rclone/copied.txt".into(),
            ":webdav:/rclone/moved.txt".into(),
        ],
        vec!["deletefile".into(), ":webdav:/rclone/note.txt".into()],
        vec!["deletefile".into(), ":webdav:/rclone/moved.txt".into()],
        vec!["rmdir".into(), ":webdav:/rclone".into()],
    ] {
        let result = Command::new("/usr/bin/rclone")
            .args([
                "--config",
                "/dev/null",
                "--retries",
                "1",
                "--low-level-retries",
                "1",
                "--timeout",
                "10s",
                "--webdav-url",
            ])
            .arg(&url)
            .args(["--webdav-vendor", "other", "--header"])
            .arg(format!(
                "Authorization: Basic {}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("alice:{password}")
                )
            ))
            .args(&arguments)
            .env("HOME", &home)
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .output()?;
        assert!(result.status.success(), "rclone {} failed", arguments[0]);
        println!("rclone {}: ok", arguments[0]);
    }
    assert_eq!(fs::read(destination)?, payload);
    assert!(!root.join("rclone").exists());
    println!("cadaver/rclone: loopback methods, properties, locks and byte comparison passed");
    Ok(())
}
