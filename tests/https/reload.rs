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
