use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use pingora::connectors::http::Connector;
use pingora::http::RequestHeader;
use pingora::protocols::http::client::HttpSession;
use pingora::protocols::tls::ALPN;
use pingora::upstreams::peer::HttpPeer;

use super::support::*;

#[test]
fn sni_selects_site_certificates_and_never_falls_back_for_unknown_names() -> TestResult {
    let directory = Directory::new()?;
    let first = Certificate::issue(&["app.test", "alias.test"], None, false, (-60, 3600))?;
    let second = Certificate::issue(&["*.example.test"], None, false, (-60, 3600))?;
    first.save(&directory.0, "default", &[])?;
    second.save(&directory.0, "second", &[])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let mut config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" \"alias.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }\nsite \"*.example.test\" { tls \"second\"; proxy \"http://$UPSTREAM\"; }",
    );
    config.push_str(&format!(
        "tls \"second\" {{ cert \"{}\"; key \"{}\"; }}\n",
        directory.0.join("second.pem").display(),
        directory.0.join("second.key").display()
    ));
    let worker = thread::spawn(move || -> TestResult {
        for _ in 0..3 {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?.to_ascii_lowercase();
            assert!(request.contains("x-forwarded-proto: https"));
            reply(&mut stream, b"secure", true)?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &config)?;
    for (name, certificate) in [
        ("app.test", &first),
        ("alias.test", &first),
        ("one.example.test", &second),
    ] {
        let mut stream = tls_connect(proxy.address, name, &[&first, &second], true)?;
        assert_eq!(
            stream.ssl().selected_alpn_protocol(),
            Some(&b"http/1.1"[..])
        );
        assert_eq!(
            stream
                .ssl()
                .peer_certificate()
                .ok_or("missing cert")?
                .to_der()?,
            certificate.cert.to_der()?
        );
        assert_eq!(tls_request(&mut stream, name, "/")?.2, b"secure");
    }
    for name in ["unknown.test", "two.one.example.test", "example.test"] {
        assert!(tls_connect(address, name, &[&first, &second], true).is_err());
    }
    assert!(tls_connect(address, "app.test", &[&first], false).is_err());
    let mut stream = tls_connect(address, "app.test", &[&first], true)?;
    assert_eq!(tls_request(&mut stream, "alias.test", "/")?.0, 421);
    proxy.signal()?;
    let (code, logs) = proxy.finish()?;
    assert_eq!(code, 0);
    assert!(!logs.contains("PRIVATE KEY"));
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_carries_real_requests_and_rejects_host_coalescing_on_the_same_connection()
-> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test", "alias.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" \"alias.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    let worker = thread::spawn(move || -> TestResult {
        for _ in 0..2 {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?;
            assert!(request.starts_with("GET /h2?encoded=%2f HTTP/1.1\r\n"));
            reply(&mut stream, b"h2-body", true)?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &config)?;
    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(address, true, "app.test".to_string());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    peer.options.connection_timeout = Some(Duration::from_secs(2));
    let mut connection_id = None;
    for (name, status) in [("app.test", 200), ("alias.test", 421), ("app.test", 200)] {
        let (mut session, _) = connector.get_http_session(&peer).await?;
        let HttpSession::H2(ref mut http2) = session else {
            return Err("HTTP/2 not negotiated".into());
        };
        if let Some(id) = connection_id {
            assert_eq!(http2.fd(), id);
        } else {
            connection_id = Some(http2.fd());
        }
        let mut request = RequestHeader::build("GET", b"/h2?encoded=%2f", None)?;
        request.set_uri(format!("https://{name}/h2?encoded=%2f").parse()?);
        http2.write_request_header(Box::new(request), true)?;
        tokio::time::timeout(Duration::from_secs(5), http2.read_response_header()).await??;
        assert_eq!(
            http2
                .response_header()
                .ok_or("missing response")?
                .status
                .as_u16(),
            status
        );
        let mut body = Vec::new();
        while let Some(bytes) = http2.read_response_body().await? {
            body.extend_from_slice(&bytes);
        }
        if status == 200 {
            assert_eq!(body, b"h2-body");
        }
        connector
            .release_http_session(session, &peer, Some(Duration::from_secs(5)))
            .await;
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn redirects_use_the_matched_domain_and_preserve_path_and_query() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let http = vacant()?;
    let https = vacant()?;
    let mut config = configuration(
        &directory.0,
        Some(http),
        https,
        "127.0.0.1:3000".parse()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    config = config.replacen("server {", "server { redirect-https #true;", 1);
    let mut proxy = Proxy::start(http, &config)?;
    let (status, headers, body) = send(http, "APP.TEST.:1234", "/a/%2f?original=%2F")?;
    assert_eq!(status, 308);
    assert!(headers.to_ascii_lowercase().contains(&format!(
        "location: https://app.test:{}/a/%2f?original=%2f",
        https.port()
    )));
    assert!(body.is_empty());
    assert_eq!(send(http, "evil.test", "/")?.0, 404);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn https_response_survives_graceful_shutdown_and_delivers_the_full_chain() -> TestResult {
    let directory = Directory::new()?;
    let root = Certificate::issue(&[], None, true, (-60, 3600))?;
    let intermediate = Certificate::issue(&[], Some(&root), true, (-60, 3600))?;
    let certificate = Certificate::issue(&["app.test"], Some(&intermediate), false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[&intermediate, &root])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    let (release, released) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n")?;
        stream.flush()?;
        released.recv_timeout(Duration::from_secs(5))?;
        stream.write_all(b"1\r\nb\r\n0\r\n\r\n")?;
        stream.flush()?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &config)?;
    let mut stream = tls_connect(address, "app.test", &[&root], true)?;
    assert!(stream.ssl().peer_cert_chain().ok_or("missing chain")?.len() >= 2);
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\n\r\n")?;
    assert!(head(&mut stream)?.starts_with("HTTP/1.1 200"));
    let mut chunk = [0; 6];
    stream.read_exact(&mut chunk)?;
    assert_eq!(&chunk, b"1\r\na\r\n");
    proxy.signal()?;
    release.send(())?;
    stream.read_exact(&mut chunk)?;
    assert_eq!(&chunk, b"1\r\nb\r\n");
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn ipv4_and_ipv6_https_listeners_can_share_a_port() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let ipv6 = TcpListener::bind("[::1]:0")?;
    let address6 = ipv6.local_addr()?;
    let ipv4 = TcpListener::bind(("127.0.0.1", address6.port()))?;
    let address4 = ipv4.local_addr()?;
    drop((ipv4, ipv6));
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let config = configuration(
        &directory.0,
        None,
        address4,
        backend.local_addr()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    )
    .replacen(
        &format!("https \"{address4}\";"),
        &format!("https \"{address4}\"; https \"{address6}\";"),
        1,
    );
    let worker = thread::spawn(move || -> TestResult {
        for _ in 0..2 {
            let mut stream = accept(&backend)?;
            head(&mut stream)?;
            reply(&mut stream, b"dual-stack", true)?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address4, &config)?;
    for address in [address4, address6] {
        let mut stream = tls_connect(address, "app.test", &[&certificate], true)?;
        assert_eq!(tls_request(&mut stream, "app.test", "/")?.2, b"dual-stack");
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_serves_static_files_and_keeps_connection_after_misdirected_request() -> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("file.txt"), b"abcdef")?;

    let certificate = Certificate::issue(&["app.test", "alias.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let http = vacant()?;
    let https = vacant()?;
    let config = configuration(
        &directory.0,
        Some(http),
        https,
        "127.0.0.1:3000".parse()?,
        &format!(
            "site \"app.test\" \"alias.test\" {{ tls \"default\"; root \"{}\"; }}",
            root.display()
        ),
    );
    let mut proxy = Proxy::start(https, &config)?;

    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(https, true, "app.test".to_string());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    peer.options.connection_timeout = Some(Duration::from_secs(2));

    let mut connection_id = None;
    for (name, range, status, expected) in [
        ("app.test", false, 200, &b"abcdef"[..]),
        ("app.test", true, 206, &b"bcd"[..]),
        ("alias.test", false, 421, &b""[..]),
        ("app.test", false, 200, &b"abcdef"[..]),
    ] {
        let (mut session, _) = connector.get_http_session(&peer).await?;
        let HttpSession::H2(ref mut http2) = session else {
            return Err("HTTP/2 not negotiated".into());
        };
        if let Some(id) = connection_id {
            assert_eq!(http2.fd(), id);
        } else {
            connection_id = Some(http2.fd());
        }

        let mut request = RequestHeader::build("GET", b"/file.txt", None)?;
        request.set_uri(format!("https://{name}/file.txt").parse()?);
        request.insert_header("host", name)?;
        if range {
            request.insert_header("range", "bytes=1-3")?;
        }
        http2.write_request_header(Box::new(request), true)?;
        tokio::time::timeout(Duration::from_secs(5), http2.read_response_header()).await??;
        assert_eq!(
            http2
                .response_header()
                .ok_or("missing response")?
                .status
                .as_u16(),
            status
        );
        let mut body = Vec::new();
        while let Some(bytes) = http2.read_response_body().await? {
            body.extend_from_slice(&bytes);
        }
        if status != 421 {
            assert_eq!(body, expected);
        }
        connector
            .release_http_session(session, &peer, Some(Duration::from_secs(5)))
            .await;
    }

    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http2_request_deadlines_reset_only_the_expired_stream() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; request-timeout \"150ms\"; read-timeout \"5s\"; }",
    );
    let (received, request_received) = tokio::sync::oneshot::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut slow = accept(&backend)?;
        assert!(head(&mut slow)?.starts_with("GET /slow "));
        received.send(()).map_err(|_| "request receiver closed")?;
        for _ in 0..2 {
            let mut stream = accept(&backend)?;
            assert!(head(&mut stream)?.starts_with("GET /fast "));
            reply(&mut stream, b"sibling", true)?;
        }
        drop(slow);
        Ok(())
    });
    let mut proxy = Proxy::start(address, &config)?;
    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(address, true, "app.test".to_string());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    peer.options.max_h2_streams = 10;
    let (mut slow, _) = connector.get_http_session(&peer).await?;
    let HttpSession::H2(ref mut slow_stream) = slow else {
        return Err("HTTP/2 not negotiated".into());
    };
    let connection_id = slow_stream.fd();
    let mut request = RequestHeader::build("GET", b"/slow", None)?;
    request.set_uri("https://app.test/slow".parse()?);
    slow_stream.write_request_header(Box::new(request), true)?;
    tokio::time::timeout(Duration::from_secs(3), request_received).await??;
    for index in 0..2 {
        let (mut fast, _) = connector.get_http_session(&peer).await?;
        let HttpSession::H2(ref mut fast_stream) = fast else {
            return Err("HTTP/2 not negotiated".into());
        };
        assert_eq!(fast_stream.fd(), connection_id);
        let mut request = RequestHeader::build("GET", b"/fast", None)?;
        request.set_uri("https://app.test/fast".parse()?);
        fast_stream.write_request_header(Box::new(request), true)?;
        tokio::time::timeout(Duration::from_secs(3), fast_stream.read_response_header()).await??;
        assert_eq!(
            fast_stream
                .response_header()
                .ok_or("missing response")?
                .status
                .as_u16(),
            200
        );
        while fast_stream.read_response_body().await?.is_some() {}
        connector
            .release_http_session(fast, &peer, Some(Duration::from_secs(5)))
            .await;
        if index == 0 {
            assert!(
                tokio::time::timeout(Duration::from_secs(3), slow_stream.read_response_header())
                    .await?
                    .is_err()
            );
        }
    }
    drop(slow);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn http2_missing_authority_with_host_never_reaches_upstream() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    backend.set_nonblocking(true)?;
    let address = vacant()?;
    let config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    let mut proxy = Proxy::start(address, &config)?;
    let mut stream = tls_connect_with_alpn(address, "app.test", &[&certificate], true, b"\x02h2")?;
    assert_eq!(stream.ssl().selected_alpn_protocol(), Some(&b"h2"[..]));
    stream.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x0e\x01\x05\x00\x00\x00\x01\x82\x87\x84\x0f\x17\x08app.test")?;
    loop {
        let mut header = [0; 9];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
        let length =
            (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload)?;
        if matches!(header[3], 1 | 3 | 7) {
            break;
        }
    }
    assert!(
        matches!(backend.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    drop(stream);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn http2_incomplete_preface_closes_after_deadline() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let config = configuration(
        &directory.0,
        None,
        address,
        backend.local_addr()?,
        "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    let mut proxy = Proxy::start(address, &config)?;
    let mut stream = tls_connect_with_alpn(address, "app.test", &[&certificate], true, b"\x02h2")?;
    assert_eq!(stream.ssl().selected_alpn_protocol(), Some(&b"h2"[..]));
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(12)))?;
    let started = std::time::Instant::now();
    stream.write_all(b"PRI * HTTP/2.0\r\n")?;
    let mut bytes = [0; 1024];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert!(started.elapsed() >= Duration::from_secs(9));
    drop(stream);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_message_split_across_h2_data_frames_round_trips() -> TestResult {
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream = backend.local_addr()?;
    let worker = tokio::spawn(async move {
        let (stream, _) = backend.accept().await?;
        let mut connection = h2::server::handshake(stream).await?;
        let (request, mut response) = connection.accept().await.ok_or("missing request")??;
        let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
        let mut body = request.into_body();
        let mut received = Vec::new();
        let mut chunks = 0;
        while let Some(chunk) = body.data().await {
            chunks += 1;
            let chunk = chunk?;
            received.extend_from_slice(&chunk);
            body.flow_control().release_capacity(chunk.len())?;
        }
        assert_eq!(received, [0, 0, 0, 0, 3, 8, 150, 1]);
        assert!(chunks >= 2, "backend did not observe separate DATA frames");
        let header = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())?;
        let mut send = response.send_response(header, false)?;
        send.send_data(bytes::Bytes::from_static(&[0, 0]), false)?;
        send.send_data(bytes::Bytes::from_static(&[0, 0, 3, 8, 150, 1]), false)?;
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", "0".parse()?);
        send.send_trailers(trailers)?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        driver.abort();
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let address = vacant()?;
    let config = format!(
        "server {{ https \"{address}\"; }}\ntls \"default\" {{ cert \"{}\"; key \"{}\"; }}\nsite \"app.test\" {{ tls \"default\"; chain {{ rule {{ proxy \"http://{upstream}\" protocol=\"h2c\"; }}; }}; }}",
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(address, true, "app.test".into());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    let (session, _) = connector.get_http_session(&peer).await?;
    let HttpSession::H2(mut session) = session else {
        return Err("HTTP/2 not negotiated".into());
    };
    let mut request = RequestHeader::build("POST", b"/probe", None)?;
    request.set_uri("https://app.test/probe".parse()?);
    request.insert_header("content-type", "application/grpc")?;
    session.write_request_header(Box::new(request), false)?;
    session
        .write_request_body(bytes::Bytes::from_static(&[0, 0]), false)
        .await?;
    tokio::time::sleep(Duration::from_millis(20)).await;
    session
        .write_request_body(bytes::Bytes::from_static(&[0, 0, 3, 8, 150, 1]), true)
        .await?;
    session.read_response_header().await?;
    let mut response = Vec::new();
    while let Some(chunk) = session.read_response_body().await? {
        response.extend_from_slice(&chunk);
    }
    assert_eq!(response, [0, 0, 0, 0, 3, 8, 150, 1]);
    worker.await??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}
