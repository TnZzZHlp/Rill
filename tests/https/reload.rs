use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openssl::ssl::SslStream;
use pingora::connectors::http::Connector;
use pingora::http::RequestHeader;
use pingora::protocols::http::client::HttpSession;
use pingora::protocols::tls::ALPN;
use pingora::upstreams::peer::HttpPeer;

use super::support::*;

fn write_site(path: &Path, name: &str, root: &Path, cache: &str) -> TestResult {
    fs::write(
        path,
        format!(
            "site \"{name}\" {{ tls \"default\"; root \"{}\"; cache-control \"{cache}\"; }}\n",
            root.display()
        ),
    )?;
    Ok(())
}

fn keepalive_request(stream: &mut SslStream<TcpStream>) -> TestResult<(u16, String, Vec<u8>)> {
    write!(stream, "GET /file HTTP/1.1\r\nHost: app.test\r\n\r\n")?;
    let headers = head(stream)?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("missing status")?
        .parse()?;
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .ok_or("missing content-length")?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok((status, headers, body))
}

#[test]
fn tls_reload_changes_new_handshakes_and_keepalive_routes_and_rolls_back_invalid_cert() -> TestResult
{
    let directory = Directory::new()?;
    let control_dir = directory.0.join("control");
    fs::create_dir(&control_dir)?;
    fs::set_permissions(&control_dir, fs::Permissions::from_mode(0o700))?;
    let socket = control_dir.join("rill.sock");
    let site = directory.0.join("site.kdl");
    let root_a = directory.0.join("a");
    let root_b = directory.0.join("b");
    fs::create_dir(&root_a)?;
    fs::create_dir(&root_b)?;
    fs::write(root_a.join("file"), b"alpha")?;
    fs::write(root_b.join("file"), b"beta")?;

    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    let a = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    let b = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    assert_ne!(a.cert.to_der()?, b.cert.to_der()?);
    a.save(&directory.0, "default", &[])?;
    b.save(&directory.0, "replacement", &[])?;
    write_site(&site, "app.test", &root_a, "max-age=10")?;

    let address = vacant()?;
    let config = format!(
        "server {{ https \"{address}\"; control-socket \"{}\"; }}\n\
         tls \"default\" {{ cert \"{}\"; key \"{}\"; }}\n\
         import \"{}\"\n",
        socket.display(),
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display(),
        site.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut old = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        old.ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        a.cert.to_der()?
    );
    let (status, headers, body) = keepalive_request(&mut old)?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=10")
    );
    assert_eq!(body, b"alpha");

    fs::copy(
        directory.0.join("replacement.pem"),
        directory.0.join("default.pem"),
    )?;
    fs::copy(
        directory.0.join("replacement.key"),
        directory.0.join("default.key"),
    )?;
    write_site(&site, "app.test", &root_b, "max-age=20")?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "ok");
    assert_eq!(response.generation, Some(2));

    let mut new = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        new.ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        b.cert.to_der()?
    );
    assert_eq!(
        old.ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        a.cert.to_der()?
    );
    for stream in [&mut old, &mut new] {
        let (status, headers, body) = keepalive_request(stream)?;
        assert_eq!(status, 200);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("cache-control: max-age=20")
        );
        assert_eq!(body, b"beta");
    }

    a.save(&directory.0, "original", &[])?;
    fs::copy(
        directory.0.join("original.key"),
        directory.0.join("default.key"),
    )?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "error");
    assert_eq!(response.code.as_deref(), Some("TLS_INVALID"));
    assert_eq!(response.generation, None);
    let mut mismatch = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        mismatch
            .ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        b.cert.to_der()?
    );
    let (status, headers, body) = keepalive_request(&mut mismatch)?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=20")
    );
    assert_eq!(body, b"beta");
    drop(mismatch);
    fs::copy(
        directory.0.join("replacement.key"),
        directory.0.join("default.key"),
    )?;

    fs::write(directory.0.join("default.pem"), b"invalid certificate")?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "error");
    assert_eq!(response.code.as_deref(), Some("TLS_INVALID"));
    assert_eq!(response.generation, None);

    let mut after_failure = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        after_failure
            .ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        b.cert.to_der()?
    );
    let (status, headers, body) = keepalive_request(&mut after_failure)?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=20")
    );
    assert_eq!(body, b"beta");

    fs::copy(
        directory.0.join("replacement.pem"),
        directory.0.join("default.pem"),
    )?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "ok");
    assert_eq!(response.generation, Some(3));

    proxy.signal()?;
    drop((old, new, after_failure));
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_existing_connection_uses_reloaded_routes_for_new_streams() -> TestResult {
    let directory = Directory::new()?;
    let control_dir = directory.0.join("control");
    fs::create_dir(&control_dir)?;
    fs::set_permissions(&control_dir, fs::Permissions::from_mode(0o700))?;
    let socket = control_dir.join("rill.sock");
    let site = directory.0.join("site.kdl");
    let root_a = directory.0.join("a");
    let root_b = directory.0.join("b");
    fs::create_dir(&root_a)?;
    fs::create_dir(&root_b)?;
    fs::write(root_a.join("file"), b"alpha")?;
    fs::write(root_b.join("file"), b"beta")?;

    let certificate = Certificate::issue(&["app.test", "alias.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    write_site(&site, "app.test", &root_a, "max-age=10")?;
    let address = vacant()?;
    let config = format!(
        "server {{ https \"{address}\"; control-socket \"{}\"; }}\n\
         tls \"default\" {{ cert \"{}\"; key \"{}\"; }}\n\
         import \"{}\"\n",
        socket.display(),
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display(),
        site.display()
    );
    let mut proxy = Proxy::start(address, &config)?;

    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(address, true, "app.test".to_string());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    peer.options.connection_timeout = Some(Duration::from_secs(2));

    let mut connection_id = None;
    for (step, name, expected_status, expected_body, expected_cache) in [
        (0, "app.test", 200, &b"alpha"[..], Some("max-age=10")),
        (1, "app.test", 200, &b"beta"[..], Some("max-age=20")),
        (2, "app.test", 404, &b""[..], None),
        (3, "alias.test", 421, &b""[..], None),
    ] {
        if step == 1 {
            write_site(&site, "app.test", &root_b, "max-age=20")?;
            let response = rill::control::client::reload(&socket, Duration::from_secs(10)).await?;
            assert_eq!(response.status, "ok");
            assert_eq!(response.generation, Some(2));
        } else if step == 2 {
            write_site(&site, "alias.test", &root_b, "max-age=20")?;
            let response = rill::control::client::reload(&socket, Duration::from_secs(10)).await?;
            assert_eq!(response.status, "ok");
            assert_eq!(response.generation, Some(3));
        }

        let (mut session, _) = connector.get_http_session(&peer).await?;
        let HttpSession::H2(ref mut http2) = session else {
            return Err("HTTP/2 not negotiated".into());
        };
        if let Some(id) = connection_id {
            assert_eq!(http2.fd(), id);
        } else {
            connection_id = Some(http2.fd());
        }

        let mut request = RequestHeader::build("GET", b"/file", None)?;
        request.set_uri(format!("https://{name}/file").parse()?);
        request.insert_header("host", name)?;
        http2.write_request_header(Box::new(request), true)?;
        tokio::time::timeout(Duration::from_secs(5), http2.read_response_header()).await??;
        let response = http2.response_header().ok_or("missing response")?;
        assert_eq!(response.status.as_u16(), expected_status);
        if let Some(cache) = expected_cache {
            assert_eq!(
                response
                    .headers
                    .get("cache-control")
                    .ok_or("missing cache-control")?
                    .to_str()?,
                cache
            );
        }
        let mut body = Vec::new();
        while let Some(bytes) =
            tokio::time::timeout(Duration::from_secs(5), http2.read_response_body()).await??
        {
            body.extend_from_slice(&bytes);
        }
        if expected_status == 200 {
            assert_eq!(body, expected_body);
        }
        connector
            .release_http_session(session, &peer, Some(Duration::from_secs(5)))
            .await;
    }

    proxy.signal()?;
    drop(connector);
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn tls_websocket_survives_successful_and_failed_reload() -> TestResult {
    use std::net::TcpListener;
    use std::thread;

    const MASKED: &[u8] = &[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
    const CLOSE: &[u8] = &[0x88, 0x80, 1, 2, 3, 4];

    let directory = Directory::new()?;
    let control = directory.0.join("control");
    fs::create_dir(&control)?;
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700))?;
    let socket = control.join("rill.sock");
    let site = directory.0.join("site.kdl");
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    let certificate = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    let other = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    other.save(&directory.0, "other", &[])?;
    let backend_a = TcpListener::bind("127.0.0.1:0")?;
    let backend_b = TcpListener::bind("127.0.0.1:0")?;
    let a = backend_a.local_addr()?;
    let b = backend_b.local_addr()?;
    let site_config =
        |backend| format!("site \"app.test\" {{ tls \"default\"; proxy \"http://{backend}\"; }}\n");
    fs::write(&site, site_config(a))?;
    let address = vacant()?;
    let config = format!(
        "server {{ https \"{address}\"; control-socket \"{}\"; }}\n\
         tls \"default\" {{ cert \"{}\"; key \"{}\"; }}\n\
         import \"{}\"\n",
        socket.display(),
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display(),
        site.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let worker_a = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend_a)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(request.contains("sec-websocket-key: dghlihnhbxbszsbub25jzq=="));
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n")?;
        for _ in 0..3 {
            let mut frame = [0; 8];
            stream.read_exact(&mut frame)?;
            assert_eq!(&frame[..], MASKED);
            stream.write_all(b"\x81\x02hi")?;
        }
        let mut close = [0; 6];
        stream.read_exact(&mut close)?;
        assert_eq!(&close[..], CLOSE);
        stream.write_all(b"\x88\x00")?;
        Ok(())
    });
    let mut ws = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        ws.ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        certificate.cert.to_der()?
    );
    ws.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    let headers = head(&mut ws)?;
    assert!(headers.starts_with("HTTP/1.1 101 "), "{headers}");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=")
    );
    let exchange = |stream: &mut SslStream<TcpStream>| -> TestResult {
        stream.write_all(MASKED)?;
        let mut frame = [0; 4];
        stream.read_exact(&mut frame)?;
        assert_eq!(&frame, b"\x81\x02hi");
        Ok(())
    };
    exchange(&mut ws)?;
    fs::write(&site, site_config(b))?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "ok");
    assert_eq!(response.generation, Some(2));
    let worker_b = thread::spawn(move || -> TestResult {
        for _ in 0..2 {
            let mut stream = accept(&backend_b)?;
            let request = head(&mut stream)?;
            assert!(request.starts_with("GET / "), "{request}");
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew",
            )?;
        }
        Ok(())
    });
    let mut new = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        new.ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        certificate.cert.to_der()?
    );
    let (status, _, body) = tls_request(&mut new, "app.test", "/")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"new");
    exchange(&mut ws)?;
    fs::copy(
        directory.0.join("default.key"),
        directory.0.join("saved.key"),
    )?;
    fs::copy(
        directory.0.join("other.key"),
        directory.0.join("default.key"),
    )?;
    let response = runtime.block_on(rill::control::client::reload(
        &socket,
        Duration::from_secs(10),
    ))?;
    assert_eq!(response.status, "error");
    assert_eq!(response.code.as_deref(), Some("TLS_INVALID"));
    assert_eq!(response.generation, None);
    exchange(&mut ws)?;
    fs::copy(
        directory.0.join("saved.key"),
        directory.0.join("default.key"),
    )?;
    let mut after_failure = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        after_failure
            .ssl()
            .peer_certificate()
            .ok_or("missing cert")?
            .to_der()?,
        certificate.cert.to_der()?
    );
    let (status, _, body) = tls_request(&mut after_failure, "app.test", "/")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"new");
    ws.write_all(CLOSE)?;
    let mut close = [0; 2];
    ws.read_exact(&mut close)?;
    assert_eq!(&close, b"\x88\x00");
    drop((ws, new, after_failure));
    worker_a
        .join()
        .map_err(|_| "websocket backend panicked")??;
    worker_b.join().map_err(|_| "http backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn watch_tls_websocket_survives_reloads_and_certificate_failure_without_fd_leak() -> TestResult {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Instant;

    const MASKED: &[u8] = &[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
    const CLOSE: &[u8] = &[0x88, 0x80, 1, 2, 3, 4];

    struct StopSampler {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<TestResult>>,
    }
    impl Drop for StopSampler {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn count_inotify(pid: u32) -> TestResult<usize> {
        let mut count = 0;
        for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
            match fs::read_link(entry?.path()) {
                Ok(path) if path == Path::new("anon_inode:inotify") => count += 1,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(count)
    }
    fn wait(mut condition: impl FnMut() -> TestResult<bool>) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if condition()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("watch TLS observation timed out".into());
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    let directory = Directory::new()?;
    let control = directory.0.join("control");
    fs::create_dir(&control)?;
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700))?;
    let socket = control.join("rill.sock");
    let site = directory.0.join("site.kdl");
    let root = directory.0.join("static");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), b"static")?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    let first = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    let second = Certificate::issue(&["app.test"], Some(&ca), false, (-60, 3600))?;
    first.save(&directory.0, "default", &[])?;
    second.save(&directory.0, "second", &[])?;
    let backend_a = TcpListener::bind("127.0.0.1:0")?;
    let backend_b = TcpListener::bind("127.0.0.1:0")?;
    let a = backend_a.local_addr()?;
    let b = backend_b.local_addr()?;
    let proxy_site =
        |backend| format!("site \"app.test\" {{ tls \"default\"; proxy \"http://{backend}\"; }}\n");
    let static_site = format!(
        "site \"app.test\" {{ tls \"default\"; root \"{}\"; cache-control \"max-age=20\"; }}\n",
        root.display()
    );
    fs::write(&site, proxy_site(a))?;
    let address = vacant()?;
    let config = format!(
        "server {{ https \"{address}\"; control-socket \"{}\"; watch #true; }}\ntls \"default\" {{ cert \"{}\"; key \"{}\"; }}\nimport \"{}\"\n",
        socket.display(),
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display(),
        site.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let pid = proxy.child.id();
    wait(|| Ok(count_inotify(pid)? == 1))?;
    let log = Arc::new(Mutex::new(String::new()));
    let output = Arc::clone(&log);
    let stderr = proxy.child.stderr.take().ok_or("missing proxy stderr")?;
    let logger = thread::spawn(move || -> TestResult {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            output
                .lock()
                .map_err(|_| "log lock poisoned")?
                .push_str(&line);
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let sampler_stop = Arc::clone(&stop);
    let sampler_peak = Arc::clone(&peak);
    let sampler = thread::spawn(move || -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !sampler_stop.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                return Err("FD sampler timed out".into());
            }
            sampler_peak.fetch_max(count_inotify(pid)?, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    });
    let mut sampler = StopSampler {
        stop,
        thread: Some(sampler),
    };
    let backend_stop = Arc::new(AtomicBool::new(false));
    let backend_finished = Arc::clone(&backend_stop);
    backend_b.set_nonblocking(true)?;
    let worker_b = thread::spawn(move || -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !backend_finished.load(Ordering::Relaxed) {
            if Instant::now() >= deadline {
                return Err("HTTP backend timed out".into());
            }
            match backend_b.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    let request = head(&mut stream)?;
                    if !request.starts_with("GET /file ") {
                        return Err("unexpected backend request".into());
                    }
                    stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew",
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    });
    let mut backend_guard = StopSampler {
        stop: backend_stop,
        thread: Some(worker_b),
    };
    let worker_a = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend_a)?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(request.contains("sec-websocket-key: dghlihnhbxbszsbub25jzq=="));
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n")?;
        for _ in 0..9 {
            let mut frame = [0; 8];
            stream.read_exact(&mut frame)?;
            assert_eq!(&frame, MASKED);
            stream.write_all(b"\x81\x02hi")?;
        }
        let mut frame = [0; 6];
        stream.read_exact(&mut frame)?;
        assert_eq!(&frame, CLOSE);
        stream.write_all(b"\x88\x00")?;
        Ok(())
    });
    let mut ws = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        ws.ssl()
            .peer_certificate()
            .ok_or("missing certificate")?
            .to_der()?,
        first.cert.to_der()?
    );
    ws.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    assert!(head(&mut ws)?.starts_with("HTTP/1.1 101 "));
    let exchange = |stream: &mut SslStream<TcpStream>| -> TestResult {
        stream.write_all(MASKED)?;
        let mut frame = [0; 4];
        stream.read_exact(&mut frame)?;
        assert_eq!(&frame, b"\x81\x02hi");
        Ok(())
    };
    exchange(&mut ws)?;
    fs::copy(
        directory.0.join("second.pem"),
        directory.0.join("default.pem"),
    )?;
    fs::copy(
        directory.0.join("second.key"),
        directory.0.join("default.key"),
    )?;
    for round in 0..6 {
        let expected: &[u8] = if round % 2 == 0 { b"new" } else { b"static" };
        fs::write(
            &site,
            if round % 2 == 0 {
                proxy_site(b)
            } else {
                static_site.clone()
            },
        )?;
        wait(|| {
            let mut client = tls_connect(address, "app.test", &[&ca], true)?;
            let cert = client
                .ssl()
                .peer_certificate()
                .ok_or("missing certificate")?
                .to_der()?;
            if cert != second.cert.to_der()? {
                return Ok(false);
            }
            let Ok((status, headers, body)) = tls_request(&mut client, "app.test", "/file") else {
                return Ok(false);
            };
            Ok(status == 200
                && body == expected
                && (expected != b"static" || headers.to_ascii_lowercase().contains("max-age=20")))
        })?;
        exchange(&mut ws)?;
        wait(|| Ok(count_inotify(pid)? == 1))?;
    }
    assert_eq!(
        ws.ssl()
            .peer_certificate()
            .ok_or("missing old certificate")?
            .to_der()?,
        first.cert.to_der()?
    );
    first.save(&directory.0, "first", &[])?;
    let failures = log
        .lock()
        .map_err(|_| "log lock poisoned")?
        .lines()
        .filter(|line| line.contains("watch_reload_failed") && line.contains("TLS_INVALID"))
        .count();
    fs::copy(
        directory.0.join("first.key"),
        directory.0.join("default.key"),
    )?;
    wait(|| {
        Ok(log
            .lock()
            .map_err(|_| "log lock poisoned")?
            .lines()
            .filter(|line| line.contains("watch_reload_failed") && line.contains("TLS_INVALID"))
            .count()
            > failures)
    })?;
    let mut client = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        client
            .ssl()
            .peer_certificate()
            .ok_or("missing certificate")?
            .to_der()?,
        second.cert.to_der()?
    );
    assert_eq!(tls_request(&mut client, "app.test", "/file")?.2, b"static");
    exchange(&mut ws)?;
    let successes = log
        .lock()
        .map_err(|_| "log lock poisoned")?
        .matches("watch_reload_succeeded")
        .count();
    fs::copy(
        directory.0.join("second.key"),
        directory.0.join("default.key"),
    )?;
    wait(|| {
        Ok(log
            .lock()
            .map_err(|_| "log lock poisoned")?
            .matches("watch_reload_succeeded")
            .count()
            > successes)
    })?;
    let mut repaired = tls_connect(address, "app.test", &[&ca], true)?;
    assert_eq!(
        repaired
            .ssl()
            .peer_certificate()
            .ok_or("missing certificate")?
            .to_der()?,
        second.cert.to_der()?
    );
    assert_eq!(
        tls_request(&mut repaired, "app.test", "/file")?.2,
        b"static"
    );
    exchange(&mut ws)?;
    wait(|| Ok(count_inotify(pid)? == 1))?;
    sampler.stop.store(true, Ordering::Relaxed);
    sampler
        .thread
        .take()
        .ok_or("missing sampler")?
        .join()
        .map_err(|_| "sampler panicked")??;
    assert!(peak.load(Ordering::Relaxed) <= 2);
    ws.write_all(CLOSE)?;
    let mut close = [0; 2];
    ws.read_exact(&mut close)?;
    assert_eq!(&close, b"\x88\x00");
    drop((ws, client, repaired));
    worker_a
        .join()
        .map_err(|_| "websocket backend panicked")??;
    backend_guard.stop.store(true, Ordering::Relaxed);
    backend_guard
        .thread
        .take()
        .ok_or("missing HTTP backend")?
        .join()
        .map_err(|_| "HTTP backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    logger.join().map_err(|_| "log reader panicked")??;
    assert!(!socket.exists());
    Ok(())
}
