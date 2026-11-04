use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use openssl::ssl::{HandshakeError, NameType, SslAcceptor, SslMethod};

use super::support::*;

fn acceptor(certificate: &Certificate) -> TestResult<SslAcceptor> {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
    builder.set_private_key(&certificate.key)?;
    builder.set_certificate(&certificate.cert)?;
    Ok(builder.build())
}

fn proxy(
    address: std::net::SocketAddr,
    upstream: &str,
    directory: &Directory,
) -> TestResult<Proxy> {
    let configuration = format!(
        "server {{ http \"{address}\"; upstream-allow \"127.0.0.0/8\"; upstream-allow \"::1/128\"; }}\nsite \"app.test\" {{ proxy \"{upstream}\"; }}\n"
    );
    let ca = directory.0.join("ca.pem");
    let empty = directory.0.join("empty");
    fs::create_dir(&empty)?;
    Proxy::start_env(
        address,
        &configuration,
        &[
            ("SSL_CERT_FILE", ca.as_os_str()),
            ("SSL_CERT_DIR", empty.as_os_str()),
        ],
    )
}

#[test]
fn dns_upstream_verifies_name_and_preserves_host() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let leaf = Certificate::issue(&["localhost"], Some(&ca), false, (-60, 3600))?;
    let acceptor = acceptor(&leaf)?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let port = backend.local_addr()?.port();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = acceptor.accept(accept(&backend)?)?;
        assert_eq!(
            stream.ssl().servername(NameType::HOST_NAME),
            Some("localhost")
        );
        let request = head(&mut stream)?;
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("host: app.test")),
            "{request}"
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
        Ok(())
    });
    let address = vacant()?;
    let _proxy = proxy(address, &format!("https://localhost:{port}"), &directory)?;
    let response = send(address, "app.test", "/")?;
    assert_eq!((response.0, response.2.as_slice()), (200, b"ok".as_slice()));
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn ip_upstream_verifies_san_without_sni_on_reuse() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let leaf = Certificate::issue_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), &ca, (-60, 3600))?;
    let acceptor = acceptor(&leaf)?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = format!("https://{}", backend.local_addr()?);
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = acceptor.accept(accept(&backend)?)?;
        assert_eq!(stream.ssl().servername(NameType::HOST_NAME), None);
        for _ in 0..2 {
            head(&mut stream)?;
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
            )?;
            stream.flush()?;
        }
        Ok(())
    });
    let address = vacant()?;
    let _proxy = proxy(address, &upstream, &directory)?;
    let mut client = connect(address)?;
    for _ in 0..2 {
        client.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\n\r\n")?;
        let response = response(&mut client)?;
        assert_eq!((response.0, response.2.as_slice()), (200, b"ok".as_slice()));
    }
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn invalid_upstream_certificates_never_receive_http() -> TestResult {
    for case in ["wrong_ip", "dns_ip", "wrong_dns", "expired", "unknown_ca"] {
        let directory = Directory::new()?;
        let ca = Certificate::issue(&[], None, true, (-3600, 3600))?;
        ca.save(&directory.0, "ca", &[])?;
        let other = Certificate::issue(&[], None, true, (-3600, 3600))?;
        let leaf = match case {
            "wrong_ip" => Certificate::issue_ip("127.0.0.2".parse()?, &ca, (-60, 3600))?,
            "dns_ip" => Certificate::issue(&["127.0.0.1"], Some(&ca), false, (-60, 3600))?,
            "wrong_dns" => Certificate::issue(&["wrong.test"], Some(&ca), false, (-60, 3600))?,
            "expired" => Certificate::issue_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), &ca, (-3600, -60))?,
            _ => Certificate::issue_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), &other, (-60, 3600))?,
        };
        let acceptor = acceptor(&leaf)?;
        let backend = TcpListener::bind("127.0.0.1:0")?;
        let port = backend.local_addr()?.port();
        let worker = thread::spawn(move || -> TestResult {
            match acceptor.accept(accept(&backend)?) {
                Ok(mut stream) => {
                    let mut byte = [0];
                    match stream.read(&mut byte) {
                        Ok(0) => (),
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::ConnectionAborted
                                    | io::ErrorKind::UnexpectedEof
                            ) => {}
                        result => {
                            return Err(format!(
                                "backend received HTTP or did not close: {result:?}"
                            )
                            .into());
                        }
                    }
                }
                Err(HandshakeError::Failure(error)) => {
                    if let Some(error) = error.error().io_error()
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        )
                    {
                        return Err("TLS handshake timed out".into());
                    }
                }
                Err(error) => return Err(format!("TLS handshake: {error}").into()),
            }
            Ok(())
        });
        let address = vacant()?;
        let host = if case == "wrong_dns" {
            "localhost"
        } else {
            "127.0.0.1"
        };
        let metrics = vacant()?;
        let configuration = format!(
            "server {{ http \"{address}\"; upstream-allow \"127.0.0.0/8\"; upstream-allow \"::1/128\"; observability {{ metrics \"{metrics}\"; }}; }}\nsite \"app.test\" {{ proxy \"https://{host}:{port}\"; }}\n"
        );
        let ca_path = directory.0.join("ca.pem");
        let empty = directory.0.join("empty");
        fs::create_dir(&empty)?;
        let mut proxy = Proxy::start_env(
            address,
            &configuration,
            &[
                ("SSL_CERT_FILE", ca_path.as_os_str()),
                ("SSL_CERT_DIR", empty.as_os_str()),
            ],
        )?;
        assert_eq!(send(address, "app.test", "/")?.0, 502, "{case}");
        worker.join().map_err(|_| "backend panicked")??;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut scrape = connect(metrics)?;
            scrape.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
            let (status, _, body) = response(&mut scrape)?;
            assert_eq!(status, 200);
            let body = String::from_utf8(body)?;
            if body.contains("rill_upstream_attempts_total{result=\"tls_fail\"} 1") {
                if case == "unknown_ca" {
                    assert!(body.contains("rill_upstream_attempts_total{result=\"fail\"} 0"));
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("TLS failure metric missing: {case}").into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        proxy.signal()?;
        let (status, logs) = proxy.finish()?;
        assert_eq!(status, 0);
        assert!(
            logs.lines()
                .any(|line| line.contains("\"event\":\"stopped\""))
        );
    }
    Ok(())
}

#[test]
fn metrics_bind_failure_emits_startup_error() -> TestResult {
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let metrics = occupied.local_addr()?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; observability {{ metrics \"{metrics}\"; }}; }}\nsite \"app.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"
    );
    match Proxy::start(address, &configuration) {
        Ok(_) => Err("metrics bind conflict accepted".into()),
        Err(error) => {
            assert!(
                error.to_string().contains("metrics_listen_failed"),
                "{error}"
            );
            Ok(())
        }
    }
}
