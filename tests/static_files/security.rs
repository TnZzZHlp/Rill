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

#[test]
fn precompressed_sidecars_remain_confined_and_require_original() -> TestResult {
    let mut f = Fixture::new()?;
    fs::write(f.root.join("asset"), b"original")?;
    fs::write(f.dir.join("outside"), b"outside")?;
    fs::write(f.root.join("missing.br"), b"sidecar only")?;
    symlink("../outside", f.root.join("asset.br"))?;
    f.start("precompressed #true;")?;
    assert_eq!(
        f.request("GET", "/asset", "Accept-Encoding: br\r\n")?.0,
        403
    );
    assert_eq!(
        f.request("GET", "/asset", "Accept-Encoding: identity\r\n")?
            .2,
        b"original"
    );
    assert_eq!(
        f.request("GET", "/missing", "Accept-Encoding: br\r\n")?.0,
        404
    );
    fs::remove_file(f.root.join("asset.br"))?;
    fs::create_dir(f.root.join("asset.br"))?;
    assert_eq!(
        f.request("GET", "/asset", "Accept-Encoding: br\r\n")?.0,
        403
    );
    Ok(())
}

#[test]
fn directory_listing_escapes_and_confines_entries() -> TestResult {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let mut f = Fixture::new()?;
    let dir = f.root.join("dir");
    fs::create_dir(&dir)?;
    fs::write(dir.join("a\"<>%#? &"), b"safe")?;
    fs::write(dir.join(".secret"), b"secret")?;
    fs::write(dir.join(OsString::from_vec(vec![b'x', 0xff])), b"secret")?;
    fs::write(dir.join("bad\\name"), b"secret")?;
    fs::write(f.dir.join("outside"), b"secret")?;
    symlink("../../outside", dir.join("escape"))?;
    f.start("browse #true;")?;
    let (status, head, body) = f.request("GET", "/dir", "Range: bytes=0-1\r\n")?;
    assert_eq!(status, 200);
    assert_eq!(
        super::support::header(&head, "cache-control"),
        Some("no-store")
    );
    let html = String::from_utf8(body)?;
    assert!(html.contains("a&quot;&lt;&gt;%#? &amp;"));
    assert!(html.contains("href=\"/dir/a%22%3C%3E%25%23%3F%20%26\""));
    for hidden in [".secret", "escape", "bad\\name"] {
        assert!(!html.contains(hidden));
    }
    assert_eq!(
        f.request("GET", "/dir/a%22%3C%3E%25%23%3F%20%26", "")?.2,
        b"safe"
    );
    for method in ["GET", "HEAD"] {
        let (status, head, body) = f.request(method, "/dir", "If-None-Match: *\r\n")?;
        assert_eq!(status, 304);
        assert_eq!(super::support::header(&head, "content-length"), None);
        assert_eq!(super::support::header(&head, "etag"), None);
        assert_eq!(
            super::support::header(&head, "cache-control"),
            Some("no-store")
        );
        assert!(body.is_empty());
    }
    for condition in [
        "If-None-Match: \"other\"\r\n",
        "If-None-Match: *\r\nIf-None-Match: *\r\n",
        "If-None-Match: *, \"other\"\r\n",
    ] {
        assert_eq!(f.request("GET", "/dir", condition)?.0, 200);
    }
    let (_, head, body) = f.request("HEAD", "/dir", "")?;
    assert_eq!(
        super::support::header(&head, "content-length"),
        Some(html.len().to_string().as_str())
    );
    assert!(body.is_empty());
    fs::write(dir.join("index.html"), b"index")?;
    assert_eq!(f.request("GET", "/dir", "")?.2, b"index");
    Ok(())
}

#[test]
fn directory_listing_rejects_visible_and_scanned_limits() -> TestResult {
    let mut f = Fixture::new()?;
    let visible = f.root.join("visible");
    let hidden = f.root.join("hidden");
    fs::create_dir(&visible)?;
    fs::create_dir(&hidden)?;
    for index in 0..513 {
        fs::write(visible.join(format!("file{index}")), b"")?;
    }
    for index in 0..4097 {
        fs::write(hidden.join(format!(".file{index}")), b"")?;
    }
    let component = "a".repeat(200);
    let mut deep = f.root.clone();
    let mut url = String::new();
    for _ in 0..4 {
        deep.push(&component);
        fs::create_dir(&deep)?;
        url.push('/');
        url.push_str(&component);
    }
    for index in 0..400 {
        fs::write(deep.join(format!("file{index}")), b"")?;
    }
    f.start("browse #true;")?;
    assert_eq!(f.request("GET", "/visible", "")?.0, 503);
    assert_eq!(f.request("GET", "/hidden", "")?.0, 503);
    assert_eq!(f.request("GET", &url, "")?.0, 503);
    Ok(())
}

#[test]
fn cache_does_not_bypass_new_permission_denial() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new()?;
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o755))?;
    let path = f.root.join("file");
    fs::write(&path, b"private")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    let config = f
        .config("")
        .replacen("server {", "server { static-cache-bytes 65536;", 1);
    f.start_config(&config, true)?;
    assert_eq!(f.request("GET", "/file", "")?.2, b"private");
    assert_eq!(f.request("GET", "/file", "")?.2, b"private");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
    let (status, _, body) = f.request("GET", "/file", "")?;
    assert_eq!(status, 403);
    assert!(body.is_empty());
    Ok(())
}
