use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::thread;

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
        let configuration = format!(
            "server {{ http \"{address}\"; upstream-allow \"127.0.0.0/8\"; upstream-allow \"::1/128\"; }}\nsite \"app.test\" {{ proxy \"https://{host}:{port}\"; }}\n"
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
fn upstream_trust_is_frozen_until_reload() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let other = Certificate::issue(&[], None, true, (-60, 3600))?;
    let leaf = Certificate::issue(&["localhost"], Some(&ca), false, (-60, 3600))?;
    let acceptor = acceptor(&leaf)?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let port = backend.local_addr()?.port();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = acceptor.accept(accept(&backend)?)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
        Ok(())
    });
    let address = vacant()?;
    let mut proxy = proxy(address, &format!("https://localhost:{port}"), &directory)?;
    fs::write(directory.0.join("ca.pem"), other.cert.to_pem()?)?;
    let response = send(address, "app.test", "/")?;
    assert_eq!((response.0, response.2.as_slice()), (200, b"ok".as_slice()));
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn tls_h2_refuses_missing_alpn_before_business_bytes() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let leaf = Certificate::issue(&["localhost"], Some(&ca), false, (-60, 3600))?;
    let acceptor = acceptor(&leaf)?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let port = backend.local_addr()?.port();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = acceptor.accept(accept(&backend)?)?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Ok(())
            }
            result => {
                Err(format!("backend received business bytes or did not close: {result:?}").into())
            }
        }
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"https://localhost:{port}\" protocol=\"tls-h2\"; }}; }}; }}"
    );
    let empty = directory.0.join("empty");
    fs::create_dir(&empty)?;
    let ca_path = directory.0.join("ca.pem");
    let mut proxy = Proxy::start_env(
        address,
        &configuration,
        &[
            ("SSL_CERT_FILE", ca_path.as_os_str()),
            ("SSL_CERT_DIR", empty.as_os_str()),
        ],
    )?;
    assert_eq!(send(address, "app.test", "/")?.0, 502);
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn upstream_hash_directory_symlink_is_frozen() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "directory-ca", &[])?;
    let other = Certificate::issue(&[], None, true, (-60, 3600))?;
    other.save(&directory.0, "ca", &[])?;
    let hash_dir = directory.0.join("hash-dir");
    fs::create_dir(&hash_dir)?;
    let hash = format!("{:08x}.0", ca.cert.subject_name_hash());
    let source = fs::canonicalize(directory.0.join("directory-ca.pem"))?;
    std::os::unix::fs::symlink(&source, hash_dir.join(hash))?;
    let leaf = Certificate::issue(&["localhost"], Some(&ca), false, (-60, 3600))?;
    let acceptor = acceptor(&leaf)?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let port = backend.local_addr()?.port();
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = acceptor.accept(accept(&backend)?)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
        Ok(())
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"https://localhost:{port}\"; }}"
    );
    let bundle = directory.0.join("ca.pem");
    let mut proxy = Proxy::start_env(
        address,
        &configuration,
        &[
            ("SSL_CERT_FILE", bundle.as_os_str()),
            ("SSL_CERT_DIR", hash_dir.as_os_str()),
        ],
    )?;
    fs::remove_file(source)?;
    let response = send(address, "app.test", "/")?;
    assert_eq!((response.0, response.2.as_slice()), (200, b"ok".as_slice()));
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn invalid_hash_directory_material_is_rejected_during_check() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let hash_dir = directory.0.join("hash-dir");
    fs::create_dir(&hash_dir)?;
    let ca_path = directory.0.join("ca.pem");
    let config = directory.0.join("config.kdl");
    fs::write(
        &config,
        "server { http \"127.0.0.1:8080\"; }\nsite \"app.test\" { proxy \"https://127.0.0.1:4443\"; }",
    )?;
    for name in ["12345678.0", "12345678.r0", "12345678.2"] {
        let path = hash_dir.join(name);
        fs::write(&path, b"not a certificate or CRL")?;
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config"])
            .arg(&config)
            .env("SSL_CERT_FILE", &ca_path)
            .env("SSL_CERT_DIR", &hash_dir)
            .output()?;
        assert!(!output.status.success(), "invalid {name} accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("RESOURCE_IO"));
        fs::remove_file(path)?;
    }
    let mut mixed = ca.cert.to_pem()?;
    mixed.extend_from_slice(b"-----BEGIN X509 CRL-----\nMAMCAQA=\n-----END X509 CRL-----\n");
    fs::write(hash_dir.join("12345678.0"), mixed)?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&config)
        .env("SSL_CERT_FILE", &ca_path)
        .env("SSL_CERT_DIR", &hash_dir)
        .output()?;
    assert!(
        !output.status.success(),
        "certificate plus invalid CRL accepted"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("RESOURCE_IO"));
    Ok(())
}

#[test]
fn upstream_trust_rejects_untrusted_temp_directory() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let unsafe_temp = directory.0.join("unsafe-tmp");
    fs::create_dir(&unsafe_temp)?;
    fs::set_permissions(&unsafe_temp, fs::Permissions::from_mode(0o777))?;
    let empty = directory.0.join("empty");
    fs::create_dir(&empty)?;
    let address = vacant()?;
    let config = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"https://127.0.0.1:443\"; }}"
    );
    let ca_path = directory.0.join("ca.pem");
    let error = Proxy::start_env(
        address,
        &config,
        &[
            ("SSL_CERT_FILE", ca_path.as_os_str()),
            ("SSL_CERT_DIR", empty.as_os_str()),
            ("TMPDIR", unsafe_temp.as_os_str()),
        ],
    );
    assert!(error.is_err(), "unsafe temporary parent accepted");
    Ok(())
}

#[test]
fn tls_h2_bad_ca_and_name_fail_before_business_bytes() -> TestResult {
    for case in ["bad-ca", "bad-name"] {
        let directory = Directory::new()?;
        let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
        ca.save(&directory.0, "ca", &[])?;
        let other = Certificate::issue(&[], None, true, (-60, 3600))?;
        let ip = if case == "bad-name" {
            "127.0.0.2"
        } else {
            "127.0.0.1"
        };
        let leaf = Certificate::issue_ip(
            ip.parse()?,
            if case == "bad-ca" { &other } else { &ca },
            (-60, 3600),
        )?;
        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
        builder.set_private_key(&leaf.key)?;
        builder.set_certificate(&leaf.cert)?;
        builder.set_alpn_select_callback(|_, offered| {
            openssl::ssl::select_next_proto(b"\x02h2", offered)
                .ok_or(openssl::ssl::AlpnError::NOACK)
        });
        let acceptor = builder.build();
        let backend = TcpListener::bind("127.0.0.1:0")?;
        let port = backend.local_addr()?.port();
        let worker = thread::spawn(move || -> TestResult {
            match acceptor.accept(accept(&backend)?) {
                Err(HandshakeError::Failure(error)) => {
                    if error.error().io_error().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        )
                    }) {
                        return Err("handshake timed out".into());
                    }
                }
                Ok(mut stream) => {
                    let mut preface = [0; 24];
                    stream.read_exact(&mut preface)?;
                    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                    stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])?;
                    loop {
                        let mut header = [0; 9];
                        match stream.read_exact(&mut header) {
                            Ok(()) => (),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::UnexpectedEof
                                        | io::ErrorKind::ConnectionReset
                                        | io::ErrorKind::ConnectionAborted
                                        | io::ErrorKind::WouldBlock
                                        | io::ErrorKind::TimedOut
                                ) =>
                            {
                                break;
                            }
                            Err(error) => return Err(error.into()),
                        }
                        if header[3] == 1 || header[3] == 0 {
                            return Err("backend received request business frame".into());
                        }
                        let length = (usize::from(header[0]) << 16)
                            | (usize::from(header[1]) << 8)
                            | usize::from(header[2]);
                        let mut payload = vec![0; length];
                        stream.read_exact(&mut payload)?;
                    }
                }
                Err(error) => return Err(format!("handshake: {error}").into()),
            }
            Ok(())
        });
        let address = vacant()?;
        let config = format!(
            "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"https://127.0.0.1:{port}\" protocol=\"tls-h2\"; }}; }}; }}"
        );
        let empty = directory.0.join("empty");
        fs::create_dir(&empty)?;
        let ca_path = directory.0.join("ca.pem");
        let mut process = Proxy::start_env(
            address,
            &config,
            &[
                ("SSL_CERT_FILE", ca_path.as_os_str()),
                ("SSL_CERT_DIR", empty.as_os_str()),
            ],
        )?;
        assert_eq!(send(address, "app.test", "/")?.0, 502, "{case}");
        worker.join().map_err(|_| "backend panicked")??;
        process.signal()?;
        let (status, logs) = process.finish()?;
        assert_eq!(status, 0);
        assert!(logs.contains("\"kind\":\"InvalidCert\""), "{case}: {logs}");
    }
    Ok(())
}

#[test]
fn restrictive_umask_does_not_leave_private_trust_directory() -> TestResult {
    let directory = Directory::new()?;
    let ca = Certificate::issue(&[], None, true, (-60, 3600))?;
    ca.save(&directory.0, "ca", &[])?;
    let empty = directory.0.join("empty");
    fs::create_dir(&empty)?;
    let tmp = directory.0.join("tmp");
    fs::create_dir(&tmp)?;
    let config = directory.0.join("config.kdl");
    fs::write(
        &config,
        format!(
            "server {{ http \"{}\"; }}\nsite \"app.test\" {{ proxy \"https://127.0.0.1:443\"; }}",
            vacant()?
        ),
    )?;
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg("umask 0777; exec \"$1\" run --config \"$2\"")
        .arg("sh")
        .arg(env!("CARGO_BIN_EXE_rill"))
        .arg(&config)
        .env("TMPDIR", &tmp)
        .env("SSL_CERT_FILE", directory.0.join("ca.pem"))
        .env("SSL_CERT_DIR", &empty)
        .output()?;
    assert!(!result.status.success());
    assert_eq!(
        fs::read_dir(&tmp)?.count(),
        0,
        "private trust directory leaked"
    );
    Ok(())
}
