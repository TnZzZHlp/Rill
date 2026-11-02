use super::support::{Fixture, TestResult};
use std::fs;
use std::net::TcpListener;
use std::process::Command;

#[test]
fn check_prepares_root_without_binding_or_modifying_it() -> TestResult {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    f.address = listener.local_addr()?;
    fs::write(f.root.join("fixture"), b"unchanged")?;
    let config = f.dir.join("config.kdl");
    fs::write(&config, f.config(""))?;
    let output = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&config)
        .output()?;
    assert!(output.status.success());
    assert_eq!(fs::read(f.root.join("fixture"))?, b"unchanged");
    assert_eq!(fs::read_dir(&f.root)?.count(), 1);
    Ok(())
}

#[test]
fn check_rejects_missing_and_non_directory_roots() -> TestResult {
    let f = Fixture::new()?;
    let config = f.dir.join("config.kdl");
    for path in [f.dir.join("missing"), f.dir.join("file")] {
        if path.ends_with("file") {
            fs::write(&path, b"file")?;
        }
        let text = f
            .config("")
            .replace(&f.root.display().to_string(), &path.display().to_string());
        fs::write(&config, text)?;
        let output = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config"])
            .arg(&config)
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("RESOURCE_IO"));
    }
    Ok(())
}

#[test]
fn check_rejects_unreadable_root() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let f = Fixture::new()?;
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o755))?;
    let config = f.dir.join("config.kdl");
    fs::write(&config, f.config(""))?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644))?;
    let original = fs::metadata(&f.root)?.permissions();
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o000))?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_rill"));
    command.args(["check", "--config"]).arg(&config);
    if super::support::is_root()? {
        command.gid(0).uid(65534);
    }
    let output = command.output();
    fs::set_permissions(&f.root, original)?;
    let output = output?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("RESOURCE_IO"));
    Ok(())
}

#[test]
fn static_and_proxy_routes_preserve_full_path() -> TestResult {
    use std::io::{Read, Write};
    use std::thread;
    use std::time::{Duration, Instant};
    let backend = TcpListener::bind("127.0.0.1:0")?;
    backend.set_nonblocking(true)?;
    let upstream = backend.local_addr()?;
    let mut f = Fixture::new()?;
    fs::create_dir(f.root.join("assets"))?;
    fs::write(f.root.join("assets/file"), b"static")?;
    fs::write(f.root.join("file"), b"wrong")?;
    let config = format!(
        "server {{ http \"{}\"; }}\nsite \"example.test\" {{\nroute \"/assets/*\" {{ root \"{}\"; }}\nroute \"/api/*\" {{ proxy \"http://{upstream}\"; }}\n}}\n",
        f.address,
        f.root.display()
    );
    f.start_config(&config, false)?;
    let worker = thread::spawn(move || -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match backend.accept() {
                Ok((stream, _)) => break stream,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            if request.len() > 131072 {
                return Err("backend header too large".into());
            }
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"GET /api/item?x=1 HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nproxy")?;
        Ok(())
    });
    let result = (|| -> TestResult {
        let (status, _, body) = f.request("GET", "/assets/file", "")?;
        assert_eq!(status, 200);
        assert_eq!(body, b"static");
        let (status, _, body) = f.request("GET", "/api/item?x=1", "")?;
        assert_eq!(status, 200);
        assert_eq!(body, b"proxy");
        Ok(())
    })();
    worker.join().map_err(|_| "backend worker panicked")??;
    result
}

#[test]
fn short_budget_proxy_rejection_sends_only_one_response() -> TestResult {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let mut fixture = Fixture::new()?;
    let config = format!(
        "server {{ http \"{}\"; }}\nsite \"example.test\" {{ proxy \"http://127.0.0.1:3000\"; request-timeout \"1ms\"; }}\n",
        fixture.address
    );
    fixture.start_config(&config, false)?;
    let mut stream = TcpStream::connect(fixture.address)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    assert!(
        response.starts_with("HTTP/1.1 501 ") || response.starts_with("HTTP/1.1 504 "),
        "{response}"
    );
    assert_eq!(response.matches("HTTP/1.1 ").count(), 1);
    Ok(())
}

fn memory_kib(pid: u32) -> TestResult<(u64, u64)> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = |name: &str| -> TestResult<u64> {
        Ok(status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or("missing memory statistic")?
            .split_whitespace()
            .next()
            .ok_or("missing memory value")?
            .parse()?)
    };
    Ok((value("VmRSS:")?, value("VmHWM:")?))
}

#[test]
fn large_file_streams_bounded_and_drains_on_sigterm() -> TestResult {
    use std::io::{Read, Write};
    use std::thread;
    use std::time::Duration;
    const SIZE: usize = 64 * 1024 * 1024;
    let mut f = Fixture::new()?;
    {
        let block = [0x61; 64 * 1024];
        let mut file = fs::File::create(f.root.join("large"))?;
        for _ in 0..SIZE / block.len() {
            file.write_all(&block)?;
        }
    }
    f.start("request-timeout \"50ms\"; idle-timeout \"5s\";")?;
    let before = memory_kib(f.pid()?)?;
    let (mut stream, head) = f.stream("/large")?;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_eq!(
        super::support::header(&head, "content-length"),
        Some("67108864")
    );
    let mut buf = [0; 64 * 1024];
    stream.read_exact(&mut buf)?;
    assert!(buf.iter().all(|&byte| byte == 0x61));
    thread::sleep(Duration::from_millis(150));
    let paused = memory_kib(f.pid()?)?;
    eprintln!("VmRSS/VmHWM KiB before={before:?} paused={paused:?}");
    assert!(paused.0.saturating_sub(before.0) < 32 * 1024);
    assert!(paused.1.saturating_sub(before.1) < 32 * 1024);
    f.signal()?;
    let mut total = buf.len();
    while total < SIZE {
        let n = stream.read(&mut buf)?;
        assert_ne!(n, 0, "premature EOF at {total}");
        assert!(buf[..n].iter().all(|&byte| byte == 0x61));
        total += n;
        assert!(total <= SIZE);
    }
    assert_eq!(f.finish()?.code(), Some(0));
    Ok(())
}

#[test]
fn truncated_stream_never_appends_an_error_response() -> TestResult {
    use std::io::{ErrorKind, Read};
    use std::thread;
    use std::time::Duration;
    const SIZE: u64 = 64 * 1024 * 1024;
    let mut f = Fixture::new()?;
    let path = f.root.join("large");
    let file = fs::File::create(&path)?;
    file.set_len(SIZE)?;
    fs::write(f.root.join("small"), b"ok")?;
    f.start("")?;
    let (mut stream, head) = f.stream("/large")?;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_eq!(
        super::support::header(&head, "content-length"),
        Some("67108864")
    );
    let mut buf = [0; 64 * 1024];
    stream.read_exact(&mut buf)?;
    assert!(buf.iter().all(|&byte| byte == 0));
    thread::sleep(Duration::from_millis(150));
    file.set_len(0)?;
    let mut total = buf.len() as u64;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                assert!(
                    buf[..n].iter().all(|&byte| byte == 0),
                    "unexpected bytes in truncated body"
                );
                total += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        }
    }
    assert!(total < SIZE, "complete body received before truncation");
    let (status, _, body) = f.request("GET", "/small", "")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"ok");
    Ok(())
}
