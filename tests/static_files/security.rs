use super::support::{Fixture, TestResult};
use std::fs;
use std::os::unix::fs::symlink;

#[test]
fn decoding_and_confined_symlinks() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("space name"), b"space")?;
    fs::write(f.root.join("你好"), b"utf8")?;
    fs::write(f.root.join("safe"), b"safe")?;
    fs::write(f.dir.join("outside"), b"outside-marker")?;
    fs::create_dir(f.root.join("directory"))?;
    symlink("safe", f.root.join("relative"))?;
    symlink(f.root.join("safe"), f.root.join("absolute"))?;
    symlink("../outside", f.root.join("escape"))?;
    symlink("/proc/self/fd/0", f.root.join("magic"))?;
    f.start("")?;
    for (path, body) in [
        ("/space%20name", "space"),
        ("/%E4%BD%A0%E5%A5%BD", "utf8"),
        ("/relative", "safe"),
    ] {
        let (status, _, actual) = f.request("GET", path, "")?;
        assert_eq!(status, 200);
        assert_eq!(actual, body.as_bytes());
    }
    for path in ["/%zz", "/a%2fb", "/%5c", "/%00", "/%2e%2e", "/./a"] {
        assert_eq!(f.request("GET", path, "")?.0, 400, "{path}");
    }
    for path in ["/absolute", "/escape", "/magic"] {
        assert_eq!(f.request("GET", path, "")?.0, 403, "{path}");
    }
    for path in ["/%252e%252e/outside", "/directory/"] {
        assert_eq!(f.request("GET", path, "")?.0, 404, "{path}");
    }
    Ok(())
}

#[test]
fn root_descriptor_survives_path_replacement() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("file"), b"old")?;
    f.start("")?;
    fs::rename(&f.root, f.dir.join("old"))?;
    fs::create_dir(&f.root)?;
    fs::write(f.root.join("file"), b"new")?;
    assert_eq!(f.request("GET", "/file", "")?.2, b"old");
    Ok(())
}

#[test]
fn changing_symlink_never_escapes_root() -> TestResult {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    let mut f = Fixture::new()?;
    fs::write(f.root.join("safe"), b"safe")?;
    fs::write(f.dir.join("outside"), b"outside-marker")?;
    symlink("safe", f.root.join("link"))?;
    f.start("")?;
    let stop = Arc::new(AtomicBool::new(false));
    let running = stop.clone();
    let root = f.root.clone();
    let worker = thread::spawn(move || -> TestResult {
        while !running.load(Ordering::Relaxed) {
            for target in ["safe", "../outside"] {
                let temporary = root.join("next");
                symlink(target, &temporary)?;
                fs::rename(temporary, root.join("link"))?;
            }
        }
        Ok(())
    });
    let result = (|| -> TestResult {
        for _ in 0..100 {
            let (status, _, body) = f.request("GET", "/link", "")?;
            assert!(status == 403 || status == 200);
            if status == 200 {
                assert_eq!(body, b"safe");
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    worker.join().map_err(|_| "symlink worker panicked")??;
    result
}

#[test]
fn fifo_and_socket_are_not_served() -> TestResult {
    use std::os::unix::net::UnixListener;
    let mut f = Fixture::new()?;
    nix::unistd::mkfifo(
        &f.root.join("fifo"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )?;
    let _socket = UnixListener::bind(f.root.join("socket"))?;
    f.start("")?;
    for path in ["/fifo", "/socket"] {
        assert_eq!(f.request("GET", path, "")?.0, 403);
    }
    Ok(())
}

#[test]
fn spa_does_not_fallback_for_denied_file() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new()?;
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755))?;
    fs::write(f.root.join("index.html"), b"index")?;
    fs::set_permissions(f.root.join("index.html"), fs::Permissions::from_mode(0o644))?;
    fs::write(f.root.join("denied"), b"private")?;
    fs::set_permissions(f.root.join("denied"), fs::Permissions::from_mode(0o000))?;
    let config = f.config("spa #true;");
    f.start_config(&config, true)?;
    let (status, _, body) = f.request("GET", "/denied", "Accept: text/html\r\n")?;
    assert_eq!(status, 403);
    assert!(body.is_empty());
    Ok(())
}

#[test]
fn changing_directory_symlink_never_escapes_root() -> TestResult {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    let mut f = Fixture::new()?;
    fs::create_dir(f.root.join("safe-dir"))?;
    fs::write(f.root.join("safe-dir/file"), b"safe")?;
    fs::create_dir(f.dir.join("outside-dir"))?;
    fs::write(f.dir.join("outside-dir/file"), b"outside-marker")?;
    symlink("safe-dir", f.root.join("linkdir"))?;
    f.start("")?;
    let stop = Arc::new(AtomicBool::new(false));
    let running = stop.clone();
    let root = f.root.clone();
    let worker = thread::spawn(move || -> TestResult {
        while !running.load(Ordering::Relaxed) {
            for target in ["safe-dir", "../outside-dir"] {
                symlink(target, root.join("nextdir"))?;
                fs::rename(root.join("nextdir"), root.join("linkdir"))?;
            }
        }
        Ok(())
    });
    let result = (|| -> TestResult {
        for _ in 0..100 {
            let (status, _, body) = f.request("GET", "/linkdir/file", "")?;
            assert!(matches!(status, 200 | 403 | 404));
            if status == 200 {
                assert_eq!(body, b"safe");
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    worker.join().map_err(|_| "symlink worker panicked")??;
    result
}

#[test]
fn directory_index_symlink_remains_confined_to_original_root() -> TestResult {
    let mut f = Fixture::new()?;
    fs::create_dir(f.root.join("page"))?;
    fs::write(f.root.join("safe.html"), b"safe")?;
    fs::write(f.dir.join("outside.html"), b"outside-marker")?;
    symlink("../safe.html", f.root.join("page/index.html"))?;
    f.start("")?;
    assert_eq!(f.request("GET", "/page/", "")?.2, b"safe");
    symlink("../../outside.html", f.root.join("page/next"))?;
    fs::rename(f.root.join("page/next"), f.root.join("page/index.html"))?;
    assert_eq!(f.request("GET", "/page/", "")?.0, 403);
    fs::rename(&f.root, f.dir.join("old-root"))?;
    fs::create_dir(&f.root)?;
    fs::create_dir(f.root.join("page"))?;
    fs::write(f.root.join("page/index.html"), b"replacement")?;
    assert_eq!(f.request("GET", "/page/", "")?.0, 403);
    Ok(())
}
