use super::support::{Fixture, Result};
use rill::config::prepare::{ConfigurationOnly, build, build_tracked};
use rill::runtime::watch::resources::WatchResources;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static RESOURCE_LOCK: Mutex<()> = Mutex::new(());

fn count_inotify(pid: u32) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
        match fs::read_link(entry?.path()) {
            Ok(target) if target == Path::new("anon_inode:inotify") => count += 1,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(count)
}

fn reject_watch_change(f: &Fixture, expected_fd: usize) -> Result {
    let stop = AtomicBool::new(false);
    let pid = f.pid()?;
    thread::scope(|scope| -> Result {
        let sampler = scope.spawn(|| -> Result {
            let deadline = Instant::now() + Duration::from_secs(35);
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                assert_eq!(count_inotify(pid)?, expected_fd);
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        });
        let result = f.reload();
        stop.store(true, Ordering::Relaxed);
        sampler.join().map_err(|_| "FD sampler panicked")??;
        assert_eq!(result?.code.as_deref(), Some("RESTART_REQUIRED"));
        assert_eq!(count_inotify(pid)?, expected_fd);
        Ok(())
    })
}

fn until(mut condition: impl FnMut() -> Result<bool>) -> Result {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if condition()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("watch observation timed out".into());
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn logs(f: &Fixture, text: &str) -> Result<usize> {
    Ok(String::from_utf8_lossy(&f.log_bytes()?)
        .matches(text)
        .count())
}

fn wait_log(f: &Fixture, text: &str, previous: usize) -> Result {
    until(|| Ok(logs(f, text)? > previous))
}

fn response(f: &Fixture) -> Result<(u16, String, Vec<u8>)> {
    Fixture::request(&mut f.connect()?, "/file")
}

fn wait_body(f: &Fixture, body: &[u8]) -> Result {
    until(|| Ok(response(f).is_ok_and(|(status, _, actual)| status == 200 && actual == body)))
}

fn atomic_write(path: &Path, value: &str) -> Result {
    let temporary = path.with_extension("replacement");
    fs::write(&temporary, value)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn server(f: &Fixture) -> String {
    format!(
        "server {{ http \"{}\"; control-socket \"{}\"; watch #true; }}\n",
        f.address,
        f.socket.display()
    )
}

fn site(root: &Path, cache: &str) -> String {
    format!(
        "site \"example.test\" {{ root \"{}\"; cache-control \"{cache}\"; }}\n",
        root.display()
    )
}

fn roots(f: &Fixture) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let a = f.dir.join("a");
    let b = f.dir.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    fs::write(a.join("file"), b"alpha")?;
    fs::write(b.join("file"), b"beta")?;
    Ok((a, b))
}

#[test]
fn watch_startup_mode_and_check_do_not_allocate_inotify() -> Result {
    let _lock = RESOURCE_LOCK.lock().map_err(|_| "resource lock poisoned")?;
    let mut f = Fixture::new()?;
    let (a, _) = roots(&f)?;
    let plain = f.config(&a, "max-age=10");
    let watched = plain.replacen("server {", "server { watch #true;", 1);
    fs::write(&f.entry, &watched)?;
    let before = count_inotify(std::process::id())?;
    let checked = build(&f.entry, &ConfigurationOnly)?;
    assert_eq!(count_inotify(std::process::id())?, before);
    drop(checked);
    let output = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&f.entry)
        .output()?;
    assert!(output.status.success());
    assert!(!f.socket.exists());
    assert!(TcpStream::connect_timeout(&f.address, Duration::from_millis(100)).is_err());
    fs::write(&f.entry, &plain)?;
    f.start()?;
    assert_eq!(count_inotify(f.pid()?)?, 0);
    fs::write(&f.entry, &watched)?;
    reject_watch_change(&f, 0)?;
    f.signal()?;

    let mut g = Fixture::new()?;
    let (root, _) = roots(&g)?;
    let plain = g.config(&root, "max-age=10");
    let watched = plain.replacen("server {", "server { watch #true;", 1);
    fs::write(&g.entry, &watched)?;
    g.start()?;
    until(|| Ok(count_inotify(g.pid()?)? == 1))?;
    fs::write(&g.entry, plain)?;
    reject_watch_change(&g, 1)?;
    g.signal()?;
    Ok(())
}

#[test]
fn watch_symlink_atomic_import_and_glob_graph() -> Result {
    let mut f = Fixture::new()?;
    let (a, b) = roots(&f)?;
    let one = f.dir.join("one.kdl");
    let two = f.dir.join("two.kdl");
    let imported = f.dir.join("site.kdl");
    fs::write(&imported, site(&a, "max-age=10"))?;
    fs::write(&one, format!("{}import \"site.kdl\"\n", server(&f)))?;
    fs::write(&two, format!("{}import \"site.kdl\"\n", server(&f)))?;
    symlink(&one, &f.entry)?;
    f.start()?;
    wait_body(&f, b"alpha")?;
    atomic_write(&imported, &site(&b, "max-age=20"))?;
    wait_body(&f, b"beta")?;
    let link = f.dir.join("next-link");
    symlink(&two, &link)?;
    let old = logs(&f, "watch_reload_succeeded")?;
    fs::rename(&link, &f.entry)?;
    wait_log(&f, "watch_reload_succeeded", old)?;
    let dir = f.dir.join("groups");
    fs::create_dir(&dir)?;
    fs::create_dir(dir.join("first"))?;
    fs::write(dir.join("first/site.kdl"), site(&a, "max-age=10"))?;
    atomic_write(&two, &format!("{}import \"groups/*/*.kdl\"\n", server(&f)))?;
    wait_body(&f, b"alpha")?;
    let failed = logs(&f, "watch_reload_failed")?;
    fs::remove_file(dir.join("first/site.kdl"))?;
    wait_log(&f, "watch_reload_failed", failed)?;
    fs::create_dir(dir.join("second"))?;
    fs::write(dir.join("second/site.kdl"), site(&b, "max-age=20"))?;
    wait_body(&f, b"beta")?;
    let successes = logs(&f, "watch_reload_succeeded")?;
    atomic_write(&imported, &site(&a, "max-age=10"))?;
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        assert_eq!(logs(&f, "watch_reload_succeeded")?, successes);
        assert_eq!(response(&f)?.2, b"beta");
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[test]
fn watch_imported_profile_changes_real_timeout() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let profile = f.dir.join("profile.kdl");
    fs::write(&profile, "profile \"slow\" { request-timeout \"30ms\"; }\n")?;
    fs::write(
        &f.entry,
        format!(
            "{}import \"profile.kdl\"\nsite \"example.test\" {{ use \"slow\"; proxy \"http://{}\"; }}\n",
            server(&f),
            listener.local_addr()?
        ),
    )?;
    f.start()?;
    let worker = thread::spawn(move || -> Result {
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                if bytes.len() > 131072 {
                    return Err("headers too large".into());
                }
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                bytes.push(byte[0]);
            }
            thread::sleep(Duration::from_millis(80));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
        Ok(())
    });
    let result = (|| -> Result {
        assert_eq!(response(&f)?.0, 504);
        let previous = logs(&f, "watch_reload_succeeded")?;
        atomic_write(
            &profile,
            "profile \"slow\" { request-timeout \"500ms\"; }\n",
        )?;
        wait_log(&f, "watch_reload_succeeded", previous)?;
        assert_eq!(response(&f)?.2, b"ok");
        Ok(())
    })();
    let joined = worker.join().map_err(|_| "backend panicked")?;
    result?;
    joined
}

#[test]
fn watch_failed_imports_recover_without_entry_edits() -> Result {
    let mut f = Fixture::new()?;
    let (a, b) = roots(&f)?;
    let imported = f.dir.join("site.kdl");
    fs::write(&imported, site(&a, "max-age=10"))?;
    fs::write(&f.entry, format!("{}import \"site.kdl\"\n", server(&f)))?;
    f.start()?;
    for (bad, repair) in [
        ("site \"broken\" {".to_string(), 0),
        ("import \"missing/site.kdl\"\n".to_string(), 1),
        ("import \"parent/site.kdl\"\n".to_string(), 2),
        ("import \"dangling/site.kdl\"\n".to_string(), 3),
    ] {
        if repair == 2 {
            fs::write(f.dir.join("parent"), "not a directory")?;
        }
        if repair == 3 {
            symlink("target", f.dir.join("dangling"))?;
        }
        let failed = logs(&f, "watch_reload_failed")?;
        atomic_write(&imported, &bad)?;
        wait_log(&f, "watch_reload_failed", failed)?;
        assert_eq!(response(&f)?.2, b"alpha");
        match repair {
            0 => atomic_write(&imported, &site(&b, "max-age=20"))?,
            1 => {
                fs::create_dir(f.dir.join("missing"))?;
                fs::write(f.dir.join("missing/site.kdl"), site(&b, "max-age=20"))?;
            }
            2 => {
                fs::remove_file(f.dir.join("parent"))?;
                fs::create_dir(f.dir.join("parent"))?;
                fs::write(f.dir.join("parent/site.kdl"), site(&b, "max-age=20"))?;
            }
            _ => {
                fs::create_dir(f.dir.join("target"))?;
                fs::write(f.dir.join("target/site.kdl"), site(&b, "max-age=20"))?;
            }
        }
        wait_body(&f, b"beta")?;
        atomic_write(&imported, &site(&a, "max-age=10"))?;
        wait_body(&f, b"alpha")?;
    }
    Ok(())
}

#[test]
fn watch_non_utf8_glob_recovers_and_ignores_unmatched_file() -> Result {
    let mut f = Fixture::new()?;
    let (a, _) = roots(&f)?;
    let dir = f.dir.join("sites");
    fs::create_dir(&dir)?;
    fs::write(dir.join("site.kdl"), site(&a, "max-age=10"))?;
    fs::write(&f.entry, format!("{}import \"sites/*.kdl\"\n", server(&f)))?;
    f.start()?;
    let bad = dir.join(OsString::from_vec(vec![0xff, b'.', b'k', b'd', b'l']));
    let failures = logs(&f, "watch_reload_failed")?;
    fs::write(&bad, "bad")?;
    wait_log(&f, "watch_reload_failed", failures)?;
    assert_eq!(response(&f)?.2, b"alpha");
    let successes = logs(&f, "watch_reload_succeeded")?;
    fs::remove_file(&bad)?;
    wait_log(&f, "watch_reload_succeeded", successes)?;
    assert_eq!(response(&f)?.2, b"alpha");
    thread::sleep(Duration::from_millis(600));
    let successes = logs(&f, "watch_reload_succeeded")?;
    let failures = logs(&f, "watch_reload_failed")?;
    fs::write(dir.join("unrelated.txt"), "static")?;
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        assert_eq!(logs(&f, "watch_reload_succeeded")?, successes);
        assert_eq!(logs(&f, "watch_reload_failed")?, failures);
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[test]
fn watch_burst_concurrent_cli_converges() -> Result {
    let mut f = Fixture::new()?;
    let (a, b) = roots(&f)?;
    let imported = f.dir.join("site.kdl");
    fs::write(&imported, site(&a, "max-age=10"))?;
    fs::write(&f.entry, format!("{}import \"site.kdl\"\n", server(&f)))?;
    f.start()?;
    let socket = f.socket.clone();
    let worker = thread::spawn(move || -> Result {
        for _ in 0..4 {
            let output = Command::new(env!("CARGO_BIN_EXE_rill"))
                .args(["reload", "--socket"])
                .arg(&socket)
                .args(["--timeout", "10s"])
                .output()?;
            assert!(
                output.status.success() || String::from_utf8_lossy(&output.stderr).contains("BUSY")
            );
        }
        Ok(())
    });
    for i in 0..20 {
        atomic_write(
            &imported,
            &site(
                if i % 2 == 0 { &a } else { &b },
                if i % 2 == 0 {
                    "max-age=10"
                } else {
                    "max-age=20"
                },
            ),
        )?;
        let (status, headers, body) = response(&f)?;
        let headers = headers.to_ascii_lowercase();
        assert!(
            status == 200
                && ((body == b"alpha" && headers.contains("max-age=10"))
                    || (body == b"beta" && headers.contains("max-age=20")))
        );
        thread::sleep(Duration::from_millis(30));
    }
    worker.join().map_err(|_| "reload worker panicked")??;
    let final_root = f.dir.join("final");
    fs::create_dir(&final_root)?;
    fs::write(final_root.join("file"), b"gamma")?;
    let previous = logs(&f, "watch_reload_succeeded")?;
    atomic_write(&imported, &site(&final_root, "max-age=30"))?;
    wait_log(&f, "watch_reload_succeeded", previous)?;
    wait_body(&f, b"gamma")?;
    thread::sleep(Duration::from_secs(3));
    let successes = logs(&f, "watch_reload_succeeded")?;
    let failures = logs(&f, "watch_reload_failed")?;
    assert!(successes + failures <= 20);
    thread::sleep(Duration::from_millis(2500));
    assert_eq!(logs(&f, "watch_reload_succeeded")?, successes);
    assert_eq!(logs(&f, "watch_reload_failed")?, failures);
    assert_eq!(response(&f)?.2, b"gamma");
    Ok(())
}

#[test]
fn watch_resources_detect_registration_gap_and_failure_baseline() -> Result {
    let _lock = RESOURCE_LOCK.lock().map_err(|_| "resource lock poisoned")?;
    let f = Fixture::new()?;
    let (a, b) = roots(&f)?;
    let imported = f.dir.join("site.kdl");
    fs::write(&imported, site(&a, "max-age=10"))?;
    fs::write(&f.entry, format!("{}import \"site.kdl\"\n", server(&f)))?;
    let active = build_tracked(&f.entry, &ConfigurationOnly).map_err(|e| e.error)?;
    atomic_write(&imported, &site(&b, "max-age=20"))?;
    let error =
        WatchResources::prepare(&active.dependencies).expect_err("changed plan must be rejected");
    assert_eq!(error.code, "RESOURCE_CHANGED");
    let active = build_tracked(&f.entry, &ConfigurationOnly).map_err(|e| e.error)?;
    atomic_write(&imported, "site \"broken\" {")?;
    let failure =
        build_tracked(&f.entry, &ConfigurationOnly).expect_err("invalid syntax must fail");
    let mut stable =
        WatchResources::prepare_with_failure(&active.dependencies, &failure.dependencies)?;
    assert!(!stable.drain().dirty);
    drop(stable);
    atomic_write(&imported, &site(&a, "max-age=10"))?;
    let mut changed =
        WatchResources::prepare_with_failure(&active.dependencies, &failure.dependencies)?;
    assert!(changed.drain().dirty);
    Ok(())
}

#[test]
fn watch_resources_reject_consumed_symlink_target_change() -> Result {
    use rill::config::dependencies::DependencyPlan;
    use rill::config::source::Origin;

    let _lock = RESOURCE_LOCK.lock().map_err(|_| "resource lock poisoned")?;
    let f = Fixture::new()?;
    let a = f.dir.join("first");
    let b = f.dir.join("second");
    let link = f.dir.join("dependency");
    let next = f.dir.join("next");
    fs::write(&a, "first contents")?;
    fs::write(&b, "second contents")?;
    symlink(&a, &link)?;
    let origin = Origin::file(&link);
    let mut plan = DependencyPlan::default();
    plan.track_path(&link, &origin)?;
    symlink(&b, &next)?;
    fs::rename(&next, &link)?;
    let consumed = fs::metadata(&link)?;
    plan.record_consumed(&link, &consumed, &origin)?;
    assert!(plan.consumed.contains_key(&link));
    symlink(&a, &next)?;
    fs::rename(&next, &link)?;
    let mut current = DependencyPlan::default();
    current.track_path(&link, &origin)?;
    plan.paths = current.paths;
    assert!(!plan.unchanged());
    let error = WatchResources::prepare(&plan).expect_err("consumed symlink target changed");
    assert_eq!(error.code, "RESOURCE_CHANGED");
    Ok(())
}

#[test]
fn watch_resources_reports_real_inotify_queue_overflow() -> Result {
    use rill::config::dependencies::DependencyPlan;
    use rill::config::source::Origin;

    let _lock = RESOURCE_LOCK.lock().map_err(|_| "resource lock poisoned")?;
    let maximum: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")?
        .trim()
        .parse()
        .map_err(|_| "unsupported inotify queue limit")?;
    if maximum > 65536 {
        return Err("unsupported inotify queue limit above 65536".into());
    }
    let f = Fixture::new()?;
    let directory = f.dir.join("glob");
    fs::create_dir(&directory)?;
    let origin = Origin::file(&directory);
    let mut plan = DependencyPlan::default();
    plan.track_glob(&directory, "*.kdl", &origin)?;
    let before = count_inotify(std::process::id())?;
    let mut resources = WatchResources::prepare(&plan)?;
    assert_eq!(count_inotify(std::process::id())?, before + 1);
    for index in 0..=maximum {
        let path = directory.join(format!("{index}.kdl"));
        fs::write(&path, b"")?;
        fs::remove_file(path)?;
    }
    let mut overflow = false;
    for _ in 0..256 {
        let outcome = resources.drain();
        if outcome.code == Some("WATCH_OVERFLOW") {
            assert!(outcome.dirty);
            assert!(outcome.needs_rebuild);
            overflow = true;
            break;
        }
        if !outcome.dirty {
            break;
        }
    }
    assert!(overflow, "inotify did not report queue overflow");
    drop(resources);
    assert_eq!(count_inotify(std::process::id())?, before);
    Ok(())
}

#[test]
fn watch_permission_denied_retries_are_bounded_and_chmod_recovers() -> Result {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    let mut f = Fixture::new()?;
    let (a, b) = roots(&f)?;
    let blocked = f.dir.join("blocked");
    fs::create_dir(&blocked)?;
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700))?;
    let target = blocked.join("site.kdl");
    fs::write(&target, site(&b, "max-age=20"))?;
    fs::write(
        &f.entry,
        format!("{}{}", server(&f), site(&a, "max-age=10")),
    )?;
    f.start()?;
    wait_body(&f, b"alpha")?;
    until(|| Ok(count_inotify(f.pid()?)? == 1))?;
    let restore = RestorePermissions(blocked.clone());
    let result = (|| -> Result {
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0))?;
        match fs::read(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Ok(_) => return Err("permission test requires an unprivileged UID".into()),
            Err(error) => return Err(error.into()),
        }
        let offset = f.log_bytes()?.len();
        let failures = logs(&f, "watch_reload_failed")?;
        atomic_write(
            &f.entry,
            &format!("{}import \"blocked/site.kdl\"\n", server(&f)),
        )?;
        wait_log(&f, "watch_reload_failed", failures)?;
        let output = f.log_bytes()?;
        if !String::from_utf8_lossy(&output[offset..])
            .lines()
            .any(|line| line.contains("watch_reload_failed") && line.contains("RESOURCE_IO"))
        {
            return Err("missing RESOURCE_IO watch failure".into());
        }
        let (status, _, body) = response(&f)?;
        if status != 200 || body != b"alpha" {
            return Err("permission failure changed the active route".into());
        }
        let initial = logs(&f, "watch_reload_failed")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if logs(&f, "watch_reload_failed")?.saturating_sub(initial) > 3 {
                return Err("permission recovery retried too frequently".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        if logs(&f, "watch_reload_failed")? <= initial {
            return Err("permission recovery did not retry while access remained denied".into());
        }
        let successes = logs(&f, "watch_reload_succeeded")?;
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700))?;
        wait_body(&f, b"beta")?;
        wait_log(&f, "watch_reload_succeeded", successes)?;
        until(|| Ok(count_inotify(f.pid()?)? == 1))?;
        Ok(())
    })();
    drop(restore);
    result
}
