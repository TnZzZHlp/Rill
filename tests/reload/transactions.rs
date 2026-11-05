use super::support::{Fixture, Result};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn reload_updates_keepalive_snapshot_and_cli() -> Result {
    let mut f = Fixture::new()?;
    let a = f.dir.join("a");
    let b = f.dir.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    fs::write(a.join("file"), "alpha")?;
    fs::write(b.join("file"), "beta")?;
    fs::write(&f.entry, f.config(&a, "max-age=10"))?;
    f.start()?;
    let mut stream = f.connect()?;
    let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=10")
    );
    assert_eq!(body, b"alpha");
    fs::write(&f.entry, f.config(&b, "max-age=20"))?;
    let response = f.reload()?;
    assert_eq!(response.generation, Some(2));
    let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=20")
    );
    assert_eq!(body, b"beta");
    let output = f.cli(&[])?;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains('3'));
    assert!(text.contains("id"));
    Ok(())
}

#[test]
fn failed_reload_preserves_state_and_generation() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), "stable")?;
    let valid = f.config(&root, "max-age=10");
    fs::write(&f.entry, &valid)?;
    f.start()?;
    for (bad, expected) in [
        ("server {".to_owned(), "CONFIG_SYNTAX"),
        (
            f.config(&f.dir.join("missing"), "max-age=10"),
            "RESOURCE_IO",
        ),
        (
            valid.replace(
                &f.address.to_string(),
                &TcpListener::bind("127.0.0.1:0")?.local_addr()?.to_string(),
            ),
            "RESTART_REQUIRED",
        ),
        (
            valid.replace(
                &f.socket.display().to_string(),
                &f.dir.join("other.sock").display().to_string(),
            ),
            "RESTART_REQUIRED",
        ),
        (
            valid.replace("root ", "use \"missing\"; root "),
            "PROFILE_REF",
        ),
    ] {
        fs::write(&f.entry, bad)?;
        let response = f.reload()?;
        assert_eq!(response.code.as_deref(), Some(expected));
        assert_eq!(f.cli(&[])?.status.code(), Some(1));
        let mut stream = f.connect()?;
        assert_eq!(Fixture::request(&mut stream, "/file")?.2, b"stable");
    }
    fs::write(&f.entry, valid)?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_eq!(f.reload()?.generation, Some(3));
    Ok(())
}

#[test]
fn reload_follows_retargeted_entry_symlink() -> Result {
    let mut f = Fixture::new()?;
    let a = f.dir.join("a");
    let b = f.dir.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    fs::write(a.join("file"), "alpha")?;
    fs::write(b.join("file"), "beta")?;
    let old = f.dir.join("old.kdl");
    let new = f.dir.join("new.kdl");
    fs::write(&old, f.config(&a, "max-age=10"))?;
    fs::write(&new, f.config(&b, "max-age=20"))?;
    symlink(&old, &f.entry)?;
    f.start()?;
    fs::remove_file(&f.entry)?;
    symlink(&new, &f.entry)?;
    assert_eq!(f.reload()?.generation, Some(2));
    let mut stream = f.connect()?;
    assert_eq!(Fixture::request(&mut stream, "/file")?.2, b"beta");
    Ok(())
}

#[test]
fn removed_route_returns_404_while_other_route_remains() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    fs::create_dir(root.join("gone"))?;
    fs::create_dir(root.join("keep"))?;
    fs::write(root.join("gone/file"), "stable")?;
    fs::write(root.join("keep/file"), "stable")?;
    let server = format!(
        "server {{ http \"{}\"; control-socket \"{}\"; }}\n",
        f.address,
        f.socket.display()
    );
    let route = |path: &str| format!("route \"{path}\" {{ root \"{}\"; }};\n", root.display());
    fs::write(
        &f.entry,
        format!(
            "{server}site \"example.test\" {{ {} {} }}\n",
            route("/gone/*"),
            route("/keep/*")
        ),
    )?;
    f.start()?;
    let mut stream = f.connect()?;
    assert_eq!(Fixture::request(&mut stream, "/gone/file")?.0, 200);
    fs::write(
        &f.entry,
        format!("{server}site \"example.test\" {{ {} }}\n", route("/keep/*")),
    )?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_eq!(Fixture::request(&mut stream, "/gone/file")?.0, 404);
    let mut stream = f.connect()?;
    assert_eq!(Fixture::request(&mut stream, "/keep/file")?.2, b"stable");
    Ok(())
}

#[test]
fn changed_imported_profile_updates_proxy_timeout() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let backend = listener.local_addr()?;
    let profile = f.dir.join("profile.kdl");
    fs::write(&profile, "profile \"slow\" { request-timeout \"30ms\"; }\n")?;
    fs::write(
        &f.entry,
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; observability {{ access-log #true; }}; }}\nimport \"profile.kdl\"\nsite \"example.test\" {{ use \"slow\"; proxy \"http://{backend}\"; }}\n",
            f.address,
            f.socket.display()
        ),
    )?;
    f.start()?;
    let worker = thread::spawn(move || -> Result {
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                if headers.len() >= 131072 {
                    return Err("backend header too large".into());
                }
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                headers.push(byte[0]);
            }
            thread::sleep(Duration::from_millis(80));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
        Ok(())
    });
    let requests = (|| -> Result {
        let mut stream = f.connect()?;
        if Fixture::request(&mut stream, "/")?.0 != 504 {
            return Err("initial request did not time out".into());
        }
        fs::write(
            &profile,
            "profile \"slow\" { request-timeout \"500ms\"; }\n",
        )?;
        if f.reload()?.generation != Some(2) {
            return Err("profile reload did not publish generation 2".into());
        }
        let mut stream = f.connect()?;
        let (status, _, body) = Fixture::request(&mut stream, "/")?;
        if status != 200 || body != b"ok" {
            return Err("updated profile request failed".into());
        }
        Ok(())
    })();
    let backend_result = worker.join().map_err(|_| "backend thread panicked")?;
    requests?;
    backend_result?;
    f.signal()
}

#[test]
fn concurrent_reload_generations_are_contiguous_and_snapshots_complete() -> Result {
    let mut f = Fixture::new()?;
    let old = f.dir.join("old");
    let new = f.dir.join("new");
    fs::create_dir(&old)?;
    fs::create_dir(&new)?;
    fs::write(old.join("file"), "old")?;
    fs::write(new.join("file"), "new")?;
    fs::write(&f.entry, f.config(&old, "max-age=10"))?;
    f.start()?;
    let contents = format!(
        "{}\n/*{}*/\n",
        f.config(&new, "max-age=20"),
        "x".repeat(900 * 1024)
    );
    fs::write(&f.entry, contents)?;
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let barrier = Arc::clone(&barrier);
        let path = f.socket.clone();
        workers.push(thread::spawn(
            move || -> Result<rill::control::protocol::Response> {
                barrier.wait();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                Ok(runtime.block_on(rill::control::client::reload(
                    &path,
                    Duration::from_secs(32),
                ))?)
            },
        ));
    }
    barrier.wait();
    let observations = (|| -> Result<Vec<(u16, String, Vec<u8>)>> {
        let mut values = Vec::new();
        for _ in 0..8 {
            let mut stream = TcpStream::connect_timeout(&f.address, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            values.push(Fixture::request(&mut stream, "/file")?);
        }
        Ok(values)
    })();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().map_err(|_| "reload thread panicked"))
        .collect();
    let mut generations = Vec::new();
    for result in results {
        let response = result??;
        if response.status == "ok" {
            generations.push(response.generation.ok_or("missing generation")?);
        } else {
            assert_eq!(response.code.as_deref(), Some("BUSY"));
        }
    }
    generations.sort_unstable();
    assert!(!generations.is_empty());
    assert_eq!(
        generations,
        (2..2 + generations.len() as u64).collect::<Vec<_>>()
    );
    for (status, headers, body) in observations? {
        assert_eq!(status, 200);
        let headers = headers.to_ascii_lowercase();
        assert!(
            (body == b"old" && headers.contains("cache-control: max-age=10"))
                || (body == b"new" && headers.contains("cache-control: max-age=20"))
        );
    }
    let mut stream = f.connect()?;
    let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"new");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=20")
    );
    Ok(())
}

#[test]
fn alternating_reloads_keep_concurrent_reader_snapshots_consistent() -> Result {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    let mut f = Fixture::new()?;
    let a = f.dir.join("a");
    let b = f.dir.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    fs::write(a.join("file"), b"alpha")?;
    fs::write(b.join("file"), b"beta")?;
    fs::write(&f.entry, f.config(&a, "max-age=10"))?;
    f.start()?;
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let address = f.address;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || -> Result {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
            let headers = headers.to_ascii_lowercase();
            if status != 200
                || !((body == b"alpha" && headers.contains("cache-control: max-age=10"))
                    || (body == b"beta" && headers.contains("cache-control: max-age=20")))
            {
                return Err("inconsistent snapshot".into());
            }
            let _ = tx.try_send(body == b"beta");
        }
        Ok(())
    });
    let result = (|| -> Result {
        assert!(!rx.recv_timeout(Duration::from_secs(10))?);
        for round in 0..6 {
            let (root, cache) = if round % 2 == 0 {
                (&b, "max-age=20")
            } else {
                (&a, "max-age=10")
            };
            fs::write(&f.entry, f.config(root, cache))?;
            let response = f.reload()?;
            assert_eq!(response.status, "ok");
            assert_eq!(response.generation, Some(round + 2));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("reader did not observe committed snapshot".into());
                }
                if rx.recv_timeout(remaining)? == (round % 2 == 0) {
                    break;
                }
            }
        }
        let mut stream = f.connect()?;
        let (status, headers, body) = Fixture::request(&mut stream, "/file")?;
        assert_eq!(status, 200);
        assert_eq!(body, b"alpha");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("cache-control: max-age=10")
        );
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    let reader_result = reader.join().map_err(|_| "reader panicked")?;
    result?;
    reader_result
}

#[test]
fn cache_root_reload_isolates_generations_and_failed_candidate() -> Result {
    let mut f = Fixture::new()?;
    let old = f.dir.join("old-root");
    let new = f.dir.join("new-root");
    fs::create_dir(&old)?;
    fs::create_dir(&new)?;
    fs::write(old.join("file"), b"alpha")?;
    fs::write(new.join("file"), b"bravo")?;
    let config = |root: &std::path::Path| {
        f.config(root, "max-age=10")
            .replacen("server {", "server { static-cache-bytes 65536;", 1)
    };
    let initial = config(&old);
    let next = config(&new);
    let invalid = config(&f.dir.join("missing"));
    fs::write(&f.entry, initial)?;
    f.start()?;
    let mut connection = f.connect()?;
    assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"alpha");
    assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"alpha");
    fs::write(&f.entry, invalid)?;
    assert_eq!(f.reload()?.code.as_deref(), Some("RESOURCE_IO"));
    assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"alpha");
    fs::write(&f.entry, next)?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"bravo");
    assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"bravo");
    Ok(())
}

#[test]
fn cache_budget_changes_require_restart() -> Result {
    for (initial, changed) in [(0, 65536), (65536, 131072)] {
        let mut f = Fixture::new()?;
        let root = f.dir.join("root");
        fs::create_dir(&root)?;
        fs::write(root.join("file"), b"stable")?;
        let config = f.config(&root, "max-age=10");
        let with_budget = |budget| {
            config.replacen(
                "server {",
                &format!("server {{ static-cache-bytes {budget};"),
                1,
            )
        };
        fs::write(&f.entry, with_budget(initial))?;
        f.start()?;
        let mut connection = f.connect()?;
        assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"stable");
        fs::write(&f.entry, with_budget(changed))?;
        assert_eq!(f.reload()?.code.as_deref(), Some("RESTART_REQUIRED"));
        let mut cli = std::process::Command::new(env!("CARGO_BIN_EXE_rill"))
            .arg("reload")
            .arg("--socket")
            .arg(&f.socket)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = cli.try_wait()? {
                assert_eq!(status.code(), Some(1));
                break;
            }
            if Instant::now() >= deadline {
                cli.kill()?;
                cli.wait()?;
                return Err("reload CLI timed out".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Fixture::request(&mut connection, "/file")?.2, b"stable");
    }
    Ok(())
}

#[test]
fn access_log_policy_reloads_and_removed_exporters_are_rejected() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    let base = f.config(&root, "max-age=10");
    let off = base.replacen(
        "server {",
        "server { observability { access-log #false; };",
        1,
    );
    fs::write(&f.entry, &off)?;
    f.start()?;
    let on = off.replace(
        "access-log #false",
        "access-log #true; access-sample-per-mille 500",
    );
    fs::write(&f.entry, &on)?;
    assert_eq!(f.reload()?.generation, Some(2));
    let changed = on.replace(
        "access-log #true",
        "otlp-endpoint \"http://127.0.0.1:4319/v1/traces\"; access-log #true",
    );
    fs::write(&f.entry, changed)?;
    assert_eq!(f.reload()?.code.as_deref(), Some("CONFIG_UNKNOWN"));
    fs::write(&f.entry, off)?;
    assert_eq!(f.reload()?.generation, Some(3));
    Ok(())
}

#[test]
fn chain_failed_reload_and_keepalive_generation() -> Result {
    let mut f = Fixture::new()?;
    let address = f.address;
    let socket = f.socket.clone();
    let config = |text: &str| {
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ chain {{ rule {{ match {{ path-regex \"^/\"; }}; respond 200 text=\"{text}\"; }}; }}; }}",
            address,
            socket.display()
        )
    };
    fs::write(&f.entry, config("old"))?;
    f.start()?;
    let mut stream = f.connect()?;
    assert_eq!(Fixture::request(&mut stream, "/")?.2, b"old");
    fs::write(&f.entry, config("old").replace("^/", "(?=bad)"))?;
    assert_eq!(f.reload()?.code.as_deref(), Some("POLICY_INVALID_REGEX"));
    assert_eq!(Fixture::request(&mut stream, "/")?.2, b"old");
    fs::write(&f.entry, config("new"))?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_eq!(Fixture::request(&mut stream, "/")?.2, b"new");
    Ok(())
}
