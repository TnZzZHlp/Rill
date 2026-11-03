use std::io::Write;
use std::net::TcpListener;
use std::thread;

use super::support::*;

#[test]
fn hosts_and_paths_are_deterministic_and_unknown_hosts_never_reach_a_backend() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let wildcard_backend = TcpListener::bind("127.0.0.1:0")?;
    let wildcard_upstream = wildcard_backend.local_addr()?;
    let address = vacant()?;
    let template = |reverse: bool| {
        let routes = if reverse {
            vec!["/api/exact", "/api/private/*", "/api/*", "/*"]
        } else {
            vec!["/*", "/api/*", "/api/private/*", "/api/exact"]
        };
        let routes = routes
            .iter()
            .map(|path| format!("route \"{path}\" {{ proxy \"http://{upstream}\"; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let exact = format!(
            "site \"api.example.test\" \"alias.example.test\" \"third.example.test\" {{\n{routes}\n}}"
        );
        let wildcard =
            format!("site \"*.example.test\" {{ proxy \"http://{wildcard_upstream}\"; }}");
        format!(
            "server {{ http \"{address}\"; }}\n{}\n{}\n",
            if reverse { &exact } else { &wildcard },
            if reverse { &wildcard } else { &exact }
        )
    };
    let worker = thread::spawn(move || -> TestResult {
        for _ in 0..14 {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?;
            reply(
                &mut stream,
                request
                    .lines()
                    .next()
                    .ok_or("missing request line")?
                    .as_bytes(),
                true,
            )?;
        }
        Ok(())
    });
    let wildcard_worker = thread::spawn(move || -> TestResult {
        for _ in 0..2 {
            let mut stream = accept(&wildcard_backend)?;
            let request = head(&mut stream)?;
            let request = request.lines().next().ok_or("missing request line")?;
            reply(&mut stream, format!("wildcard {request}").as_bytes(), true)?;
        }
        Ok(())
    });
    for reverse in [false, true] {
        let mut proxy = Proxy::start(address, &template(reverse))?;
        for (host, path) in [
            ("API.EXAMPLE.TEST.", "/api/exact"),
            ("alias.example.test", "/api/exact"),
            ("third.example.test:8080", "/api/exact"),
            ("alias.example.test", "/api/private/a"),
            ("third.example.test:8080", "/api/a?original=yes"),
            ("wild.example.test", "/"),
            ("api.example.test", "/api"),
            ("api.example.test", "/apix"),
        ] {
            let (status, _, body) = send(proxy.address, host, path)?;
            assert_eq!(status, 200);
            let expected = if host == "wild.example.test" {
                format!("wildcard GET {path} HTTP/1.1")
            } else {
                format!("GET {path} HTTP/1.1")
            };
            assert_eq!(body, expected.as_bytes());
        }
        for host in [
            "example.test",
            "a.b.example.test",
            "unconfigured.test",
            "127.0.0.1",
        ] {
            assert_eq!(send(proxy.address, host, "/")?.0, 404);
        }
        for raw in [
            "GET / HTTP/1.1\r\nConnection: close\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: api.example.test\r\nHost: alias.example.test\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: api.example.test:99999\r\n\r\n",
            "GET /bad%zz HTTP/1.1\r\nHost: api.example.test\r\n\r\n",
        ] {
            let mut stream = connect(proxy.address)?;
            stream.write_all(raw.as_bytes())?;
            assert_eq!(response(&mut stream)?.0, 400);
        }
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    worker.join().map_err(|_| "backend panicked")??;
    wildcard_worker
        .join()
        .map_err(|_| "wildcard backend panicked")??;
    Ok(())
}

#[test]
fn exact_and_longest_prefix_routes_choose_different_backends() -> TestResult {
    let exact = TcpListener::bind("127.0.0.1:0")?;
    let prefix = TcpListener::bind("127.0.0.1:0")?;
    let fallback = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let config = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{\nroute \"/*\" {{ proxy \"http://{}\"; }}\nroute \"/api/*\" {{ proxy \"http://{}\"; }}\nroute \"/api/exact\" {{ proxy \"http://{}\"; }}\n}}\n",
        fallback.local_addr()?,
        prefix.local_addr()?,
        exact.local_addr()?
    );
    let worker = |listener: TcpListener, body: &'static str, count: usize| {
        thread::spawn(move || -> TestResult {
            for _ in 0..count {
                let mut stream = accept(&listener)?;
                head(&mut stream)?;
                reply(&mut stream, body.as_bytes(), true)?;
            }
            Ok(())
        })
    };
    let e = worker(exact, "exact", 1);
    let p = worker(prefix, "prefix", 2);
    let f = worker(fallback, "fallback", 2);
    let mut proxy = Proxy::start(address, &config)?;
    for (path, expected) in [
        ("/api/exact", "exact"),
        ("/api/child", "prefix"),
        ("/api/", "prefix"),
        ("/api", "fallback"),
        ("/apix", "fallback"),
    ] {
        assert_eq!(send(address, "app.test", path)?.2, expected.as_bytes());
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    for worker in [e, p, f] {
        worker.join().map_err(|_| "backend panicked")??;
    }
    Ok(())
}

#[test]
fn exact_domains_override_single_label_wildcards() -> TestResult {
    let exact = TcpListener::bind("127.0.0.1:0")?;
    let wildcard = TcpListener::bind("127.0.0.1:0")?;
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"*.example.test\" {{ proxy \"http://{}\"; }}\nsite \"api.example.test\" {{ proxy \"http://{}\"; }}\n",
        wildcard.local_addr()?,
        exact.local_addr()?
    );
    let worker = thread::spawn(move || -> TestResult {
        for (listener, body) in [(exact, "exact"), (wildcard, "wildcard")] {
            let mut stream = accept(&listener)?;
            head(&mut stream)?;
            reply(&mut stream, body.as_bytes(), true)?;
        }
        Ok(())
    });
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(send(address, "api.example.test", "/")?.2, b"exact");
    assert_eq!(send(address, "one.example.test", "/")?.2, b"wildcard");
    assert_eq!(send(address, "two.one.example.test", "/")?.0, 404);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn snippet_routes_serve_both_hosts_without_sharing_other_routes() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let worker = thread::spawn(move || -> TestResult {
        for _ in 0..2 {
            let mut stream = accept(&backend)?;
            let _ = head(&mut stream)?;
            reply(&mut stream, b"shared", true)?;
        }
        Ok(())
    });
    let config = format!(
        "server {{ http \"{address}\"; }}\nsnippet \"shared\" {{ route \"/shared\" {{ proxy \"http://{upstream}\"; }} }}\nsite \"a.test\" {{ use-snippet \"shared\"; route \"/only-a\" {{ proxy \"http://{upstream}\"; }} }}\nsite \"b.test\" {{ use-snippet \"shared\"; }}\n"
    );
    let mut proxy = Proxy::start(address, &config)?;
    for host in ["a.test", "b.test"] {
        let (status, _, body) = send(proxy.address, host, "/shared")?;
        assert_eq!(status, 200);
        assert_eq!(body, b"shared");
    }
    assert_eq!(send(proxy.address, "b.test", "/only-a")?.0, 404);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panic")??;
    Ok(())
}
