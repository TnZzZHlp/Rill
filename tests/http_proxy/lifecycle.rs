use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::support::*;

#[test]
fn keepalive_reuses_downstream_and_upstream_connections() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        for _ in 0..2 {
            head(&mut stream)?;
            reply(&mut stream, b"ok", false)?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    for _ in 0..2 {
        stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\n\r\n")?;
        assert_eq!(response(&mut stream)?.2, b"ok");
    }
    drop(stream);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn ipv6_listener_and_ipv6_upstream_are_available() -> TestResult {
    let backend = TcpListener::bind("[::1]:0")?;
    let probe = TcpListener::bind("[::1]:0")?;
    let address = probe.local_addr()?;
    drop(probe);
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"ipv6.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        head(&mut stream)?;
        reply(&mut stream, b"ipv6", true)?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(send(address, "ipv6.test", "/")?.2, b"ipv6");
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn graceful_shutdown_stops_accepting_and_preserves_an_inflight_response() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
        backend.local_addr()?
    );
    let (release, released) = mpsc::channel();
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
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\n\r\n")?;
    assert!(head(&mut stream)?.starts_with("HTTP/1.1 200"));
    let mut chunk = [0; 6];
    stream.read_exact(&mut chunk)?;
    assert_eq!(&chunk, b"1\r\na\r\n");
    proxy.signal()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
        if Instant::now() >= deadline {
            return Err("listener remained open during drain".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(proxy.child.try_wait()?.is_none());
    release.send(())?;
    stream.read_exact(&mut chunk)?;
    assert_eq!(&chunk, b"1\r\nb\r\n");
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn invalid_or_unavailable_startup_configuration_never_leaves_a_running_service() -> TestResult {
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    for configuration in [
        "server { http \"127.0.0.1:8080\"; }\nsite \"app.test\" { proxy \"file:///etc/shadow\"; }\n".to_string(),
        format!("server {{ http \"{}\"; }}\nsite \"app.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n", occupied.local_addr()?),
    ] {
        fs::write(&entry, configuration)?;
        let output = Command::new(env!("CARGO_BIN_EXE_rill")).args(["run", "--config"]).arg(&entry).stdin(Stdio::null()).output()?;
        assert_eq!(output.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("\"event\":\"started\""));
    }
    Ok(())
}

#[test]
fn observability_metrics_and_access_are_bounded_to_controlled_fields() -> TestResult {
    let address = vacant()?;
    let metrics = vacant()?;
    let directory = Directory::new()?;
    fs::write(directory.0.join("index.html"), "hello")?;
    let configuration = format!(
        "server {{ http \"{address}\"; observability {{ access-log #true; metrics \"{metrics}\"; }}; }}\nsite \"app.test\" {{ root \"{}\"; }}\n",
        directory.0.display()
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(
        send(address, "app.test", "/index.html?private-token")?.0,
        200
    );
    assert_eq!(send(address, "missing.test", "/private-path")?.0, 404);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut scrape = connect(metrics)?;
        scrape.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
        let (status, _, body) = response(&mut scrape)?;
        assert_eq!(status, 200);
        let body = String::from_utf8(body)?;
        if body.contains("rill_requests_completed_total{method=\"GET\",status=\"2xx\"} 1")
            && body.contains("rill_requests_completed_total{method=\"GET\",status=\"4xx\"} 1")
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("request metrics did not settle".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut invalid = connect(metrics)?;
    invalid.write_all(b"POST /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
    assert_eq!(response(&mut invalid)?.0, 405);
    proxy.signal()?;
    let (status, logs) = proxy.finish()?;
    assert_eq!(status, 0);
    assert_eq!(
        logs.lines()
            .filter(|line| line.contains("\"event\":\"access\""))
            .count(),
        2
    );
    assert!(!logs.contains("private-token"));
    assert!(!logs.contains("private-path"));
    assert!(!logs.contains("app.test"));
    Ok(())
}

#[test]
fn tracing_strips_foreign_context_and_only_injects_local_sampled_parent() -> TestResult {
    for enabled in [false, true] {
        let backend = TcpListener::bind("127.0.0.1:0")?;
        let address = vacant()?;
        let configuration = format!(
            "server {{ http \"{address}\"; observability {{ tracing #{}; trace-sample-per-mille 1000; }}; }}\nsite \"app.test\" {{ proxy \"http://{}\"; }}\n",
            enabled,
            backend.local_addr()?
        );
        let worker = thread::spawn(move || -> TestResult<String> {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?;
            reply(&mut stream, b"ok", true)?;
            Ok(request)
        });
        let mut proxy = Proxy::start(address, &configuration)?;
        let mut stream = connect(address)?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\ntraceparent: 00-11111111111111111111111111111111-2222222222222222-01\r\ntracestate: foreign=secret\r\nbaggage: tenant=secret\r\n\r\n")?;
        assert_eq!(response(&mut stream)?.0, 200);
        let request = worker
            .join()
            .map_err(|_| "backend panicked")??
            .to_ascii_lowercase();
        assert!(!request.contains("tracestate:"));
        assert!(!request.contains("baggage:"));
        assert!(!request.contains("11111111111111111111111111111111"));
        let parent = request
            .lines()
            .find_map(|line| line.strip_prefix("traceparent: "));
        if enabled {
            let parent = parent.ok_or("sampled traceparent missing")?;
            assert_eq!(parent.len(), 55);
            assert!(parent.starts_with("00-") && parent.ends_with("-01"));
        } else {
            assert!(parent.is_none());
        }
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    Ok(())
}

#[test]
fn tracing_environment_is_rejected_even_when_disabled_without_disclosing_values() -> TestResult {
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nsite \"app.test\" { proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    for variable in [
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_COMPRESSION",
        "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["run", "--config"])
            .arg(&entry)
            .env(variable, "private-secret")
            .stdin(Stdio::null())
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("CONFIG_ENV"));
        assert!(!stderr.contains("private-secret"));
    }
    Ok(())
}
