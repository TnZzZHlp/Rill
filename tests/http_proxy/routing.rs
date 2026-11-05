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

#[test]
fn chain_rules_responses_and_existing_actions() -> TestResult {
    let directory = Directory::new()?;
    std::fs::write(directory.0.join("home.txt"), b"static")?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-glob "/branch/*"; }}; branch {{ when {{ match {{ path-regex "^/branch/yes"; }}; continue; }}; }}; }}
 rule {{ match {{ path-glob "/branch/*"; }}; respond 200 text="continued"; }}
 rule {{ match {{ all {{ path-glob "/ok/*"; not {{ client-cidr "192.0.2.0/24"; }}; }}; }}; respond 200 text="hello"; }}
 rule {{ match {{ path-glob "/proxy"; }}; proxy "http://{upstream}" host="backend.test" protocol="http1"; }}
 rule {{ match {{ path-glob "/files/*"; }}; serve-files root="{}" {{ index "home.txt"; }}; }}
 rule {{ match {{ path-glob "/empty"; }}; respond 204; }}
 rule {{ match {{ path-glob "/cached"; }}; respond 304; }}
 rule {{ respond 404 text="missing"; }}
}} }}"#,
        directory.0.display()
    );
    std::fs::create_dir(directory.0.join("files"))?;
    std::fs::write(directory.0.join("files/home.txt"), b"static")?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(request.contains("host: backend.test"));
        reply(&mut stream, b"upstream", true)?;
        Ok(())
    });
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/branch/no")?.0, 404);
    assert_eq!(send(address, "app.test", "/branch/yes")?.2, b"continued");
    assert_eq!(send(address, "app.test", "/ok/a/b")?.2, b"hello");
    assert_eq!(send(address, "app.test", "/proxy")?.2, b"upstream");
    assert_eq!(send(address, "app.test", "/files/")?.2, b"static");
    for (path, status) in [("/empty", 204), ("/cached", 304)] {
        let (actual, headers, _) = send(address, "app.test", path)?;
        assert_eq!(actual, status);
        assert!(!headers.to_ascii_lowercase().contains("content-length:"));
    }
    let mut stream = connect(address)?;
    stream.write_all(b"HEAD /ok/a HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut stream)?.to_ascii_lowercase();
    assert!(headers.contains("200 ok"));
    assert!(headers.contains("content-length: 5"));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn chain_headers_and_trusted_client_address() -> TestResult {
    for trusted in [false, true] {
        let address = vacant()?;
        let network = if trusted {
            "trusted-proxy \"127.0.0.0/8\";"
        } else {
            ""
        };
        let config = format!(
            r#"server {{ http "{address}"; {network} }}
site "app.test" {{ chain {{
rule {{ match {{ all {{ method "GET"; header-equal "x-test" "second"; client-cidr "192.0.2.0/24"; }}; }}; respond 200 text="trusted"; }}
rule {{ respond 401 text="denied"; }}
}} }}"#
        );
        let mut proxy = Proxy::start(address, &config)?;
        let mut stream = connect(address)?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nX-Test: first\r\nX-Test: second\r\nX-Forwarded-For: 192.0.2.3\r\nConnection: close\r\n\r\n")?;
        let (status, _, body) = response(&mut stream)?;
        assert_eq!(
            (status, body.as_slice()),
            if trusted {
                (200, b"trusted".as_slice())
            } else {
                (401, b"denied".as_slice())
            }
        );
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    Ok(())
}

#[test]
fn chain_reenter_loop_exhausts_budget() -> TestResult {
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ branch {{ when {{ match {{ path-glob "/loop"; }}; reenter; }}; otherwise {{ respond 200 text="ok"; }}; }}; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/")?.0, 200);
    assert_eq!(send(address, "app.test", "/loop")?.0, 503);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn chain_header_policy_across_terminal_responses() -> TestResult {
    let directory = Directory::new()?;
    std::fs::create_dir(directory.0.join("static"))?;
    std::fs::write(directory.0.join("static/value.html"), b"file")?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let worker = thread::spawn(move || -> TestResult {
        for index in 0..3 {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?.to_ascii_lowercase();
            assert!(request.contains("x-test: first\r\n"));
            assert!(request.contains("x-test: second\r\n"));
            assert!(request.contains("host: override.test\r\n"));
            assert!(request.contains(if index == 0 {
                "x-real-ip: 127.0.0.1"
            } else {
                "x-real-ip: 192.0.2.7"
            }));
            let connection = if index == 2 {
                "x-block, close"
            } else {
                "close"
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/html; charset=utf-8\r\nLocation: /old\r\nVary: Accept\r\nVary: accept-encoding\r\nConnection: {connection}\r\n\r\nok"
            )?;
        }
        Ok(())
    });
    for trusted in [false, true] {
        let address = vacant()?;
        let network = if trusted {
            "trusted-proxy \"127.0.0.0/8\";"
        } else {
            ""
        };
        let config = format!(
            r#"server {{ http "{address}"; {network} }}
site "app.test" {{ chain {{
 rule {{ match {{ path-glob "/missing-request-upstream"; }}; header "request" {{ set "X-Upstream" "{{upstream-authority}}"; }}; respond 200; }}
 rule {{ match {{ path-glob "/missing-response-upstream"; }}; header "response" {{ set "X-Upstream" "{{upstream-authority}}"; }}; respond 200; }}
 rule {{ match {{ path-regex "^/(proxy|static|local)/"; }};
   header "request" real-client-ip="X-Real-IP" {{ set "X-Test" "first"; add "X-Test" "second"; }};
   header "response" {{ set "X-Frozen" "{{capture.1}}"; set "Strict-Transport-Security" "max-age=60"; }};
   header "response" {{ set "Content-Type" "application/json"; }};
   header "response" when-content-type="application/json" {{ set "X-Wrong-Mime" "bad"; }};
   header "response" when-content-type="text/html" when-status="202..299" {{ set "X-Wrong-Status" "bad"; }};
   header "response" when-content-type="text/html" when-status="200..201" {{ set "Access-Control-Allow-Origin" "*"; replace "Location" "old" "new"; vary "Accept-Encoding"; }};
   continue;
 }}
 rule {{ match {{ path-regex "^/[^/]+/(.*)"; }}; continue; }}
 rule {{ match {{ path-glob "/proxy/deny"; }}; header "response" {{ set "X-Block" "bad"; }}; continue; }}
 rule {{ match {{ path-glob "/proxy/*"; }}; header "response" {{ set "X-Upstream" "{{upstream-authority}}"; }}; continue; }}
 rule {{ match {{ all {{ path-glob "/proxy/*"; header-equal "x-test" "second"; }}; }}; proxy "http://{upstream}" host="override.test"; }}
 rule {{ match {{ path-glob "/static/*"; }}; serve-files root="{}"; }}
 rule {{ respond 201 text="local" content-type="text/html; charset=utf-8"; }}
}} }}"#,
            directory.0.display()
        );
        let mut proxy = Proxy::start(address, &config)?;
        let mut stream = connect(address)?;
        stream.write_all(b"GET /proxy/value HTTP/1.1\r\nHost: app.test\r\nX-Forwarded-For: 192.0.2.7\r\nX-Real-IP: 203.0.113.3\r\nConnection: close\r\n\r\n")?;
        let (status, headers, _) = response(&mut stream)?;
        let headers = headers.to_ascii_lowercase();
        assert_eq!(status, 200);
        assert!(headers.contains(&format!("x-upstream: {upstream}")));
        assert!(!headers.contains("x-wrong-mime"));
        assert!(!headers.contains("x-wrong-status"));
        for expected in [
            "x-frozen: proxy",
            "strict-transport-security: max-age=60",
            "access-control-allow-origin: *",
            "location: /new",
            "vary: accept, accept-encoding",
        ] {
            assert!(headers.contains(expected), "{headers}");
        }
        let (status, headers, body) = send(address, "app.test", "/static/value.html")?;
        assert_eq!((status, body.as_slice()), (200, b"file".as_slice()));
        assert!(headers.to_ascii_lowercase().contains("x-frozen: static"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("access-control-allow-origin: *")
        );
        let (status, headers, _) = send(address, "app.test", "/local/value")?;
        assert_eq!(status, 201);
        assert!(headers.to_ascii_lowercase().contains("x-frozen: local"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("access-control-allow-origin: *")
        );
        if trusted {
            let mut stream = connect(address)?;
            stream.write_all(b"GET /proxy/deny HTTP/1.1\r\nHost: app.test\r\nX-Forwarded-For: 192.0.2.7\r\nConnection: close\r\n\r\n")?;
            assert_eq!(response(&mut stream)?.0, 503);
        }
        let mut stream = connect(address)?;
        stream.write_all(
            b"GET /local/value HTTP/1.1\r\nHost: app.test\r\nConnection: X-Test, close\r\n\r\n",
        )?;
        assert_eq!(response(&mut stream)?.0, 503);
        for path in ["/missing-request-upstream", "/missing-response-upstream"] {
            let (status, headers, _) = send(address, "app.test", path)?;
            assert_eq!(status, 503);
            assert!(!headers.to_ascii_lowercase().contains("x-upstream:"));
        }
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn chain_header_captures_survive_reenter_and_filter_limit() -> TestResult {
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ all {{ path-regex "^/(first)/"; header-equal "x-stage" "start"; }}; }};
   header "response" {{ set "X-Frozen" "{{capture.1}}"; }};
   header "request" {{ set "X-Stage" "next"; }};
   reenter;
 }}
 rule {{ match {{ all {{ path-regex "^/first/(.*)"; header-equal "x-stage" "next"; }}; }}; respond 200 text="ok"; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET /first/second HTTP/1.1\r\nHost: app.test\r\nX-Stage: start\r\nConnection: close\r\n\r\n")?;
    let (status, headers, _) = response(&mut stream)?;
    assert_eq!(status, 200);
    assert!(headers.to_ascii_lowercase().contains("x-frozen: first"));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    let address = vacant()?;
    let rules = "rule { header \"response\" { add \"X-Test\" \"v\"; }; continue; };".repeat(257);
    let config = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ {rules} rule {{ respond 200; }}; }}; }}"
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/")?.0, 503);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-regex "^/(.+)$"; }}; header "response" {{ set "X-Frozen" "{{capture.1}}"; }}; continue; }}
 rule {{ match {{ path-regex "^/(.+)$"; }}; respond 200; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(
        send(address, "app.test", &format!("/{}", "a".repeat(20_000)))?.0,
        503
    );
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn chain_paths_rewrite_strip_and_constrained_candidates() -> TestResult {
    let directory = Directory::new()?;
    std::fs::write(directory.0.join("file.txt"), b"file")?;
    std::fs::write(directory.0.join("index.html"), b"index")?;
    std::fs::create_dir(directory.0.join("directory"))?;
    std::fs::create_dir(directory.0.join("%2e%2e"))?;
    std::fs::write(directory.0.join("%2e%2e/file.txt"), b"once")?;
    nix::unistd::mkfifo(
        &directory.0.join("pipe"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )?;
    std::os::unix::fs::symlink("/etc/passwd", directory.0.join("escape"))?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let worker = thread::spawn(move || -> TestResult {
        for expected in [
            "GET /dest/value?x=%2f&x=2 HTTP/1.1",
            "GET /file.txt?q=raw%2f HTTP/1.1",
        ] {
            let mut stream = accept(&backend)?;
            let request = head(&mut stream)?;
            assert_eq!(request.lines().next(), Some(expected));
            reply(&mut stream, b"ok", true)?;
        }
        Ok(())
    });
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-glob "/assets*"; }}; strip-prefix "/assets"; serve-files root="{}"; }}
 rule {{ match {{ path-regex "^/change/(.*)$"; }}; rewrite "/dest/{{capture.1}}"; reenter; }}
 rule {{ match {{ path-glob "/dest/*"; }}; proxy "http://{upstream}"; }}
 rule {{ match {{ path-glob "/choose"; }}; try-files root="{}" "/missing" "/file.txt"; continue; }}
 rule {{ match {{ path-glob "/absent"; }}; try-files root="{}" "/missing" "/missing2"; continue; }}
 rule {{ match {{ path-glob "/dir"; }}; try-files root="{}" "/directory" "/file.txt"; continue; }}
 rule {{ match {{ path-glob "/escape"; }}; try-files root="{}" "/escape" "/file.txt"; continue; }}
 rule {{ match {{ path-glob "/notdir"; }}; try-files root="{}" "/file.txt/child" "/file.txt"; continue; }}
 rule {{ match {{ path-glob "/pipe"; }}; try-files root="{}" "/pipe" "/file.txt"; continue; }}
 rule {{ match {{ path-glob "/file.txt"; }}; proxy "http://{upstream}"; }}
 rule {{ respond 200 text="unexpected"; }}
}} }}"#,
        directory.0.display(),
        directory.0.display(),
        directory.0.display(),
        directory.0.display(),
        directory.0.display(),
        directory.0.display(),
        directory.0.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/assets/file.txt")?.2, b"file");
    assert_eq!(send(address, "app.test", "/assets")?.2, b"index");
    assert_eq!(send(address, "app.test", "/assetsx/file.txt")?.0, 404);
    assert_eq!(send(address, "app.test", "/change/value?x=%2f&x=2")?.0, 200);
    assert_eq!(send(address, "app.test", "/choose?q=raw%2f")?.0, 200);
    assert_eq!(send(address, "app.test", "/absent")?.0, 404);
    assert_eq!(send(address, "app.test", "/dir")?.0, 403);
    assert_eq!(send(address, "app.test", "/escape")?.0, 403);
    assert_eq!(send(address, "app.test", "/notdir")?.0, 403);
    assert_eq!(send(address, "app.test", "/pipe")?.0, 403);
    assert_eq!(send(address, "app.test", "/change/%2fetc")?.0, 400);
    assert_eq!(send(address, "app.test", "/assets/%2e%2e/file.txt")?.0, 400);
    assert_eq!(
        send(address, "app.test", "/assets/%252e%252e/file.txt")?.2,
        b"once"
    );
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn chain_error_handlers_only_catch_local_failures() -> TestResult {
    let directory = Directory::new()?;
    std::fs::write(directory.0.join("404.html"), b"missing page")?;
    std::fs::write(directory.0.join("405.html"), b"method page")?;
    std::fs::write(directory.0.join("file"), b"file")?;
    std::os::unix::fs::symlink("/etc/passwd", directory.0.join("outside"))?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let worker = thread::spawn(move || -> TestResult {
        for status in ["404 Not Found", "200 OK", "404 Not Found"] {
            let mut stream = accept(&backend)?;
            let _ = head(&mut stream)?;
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
            )?;
        }
        Ok(())
    });
    let address = vacant()?;
    let root = directory.0.display();
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-regex "^/(absent)"; }}; header "response" {{ set "X-Frozen" "{{capture.1}}"; }}; continue; }}
 rule {{ match {{ path-glob "/strip*"; }}; strip-prefix "/strip/segment"; continue; }}
 rule {{ match {{ path-glob "/miss"; }}; try-files root="{root}" "/missing"; continue; }}
 rule {{ match {{ path-glob "/errorproxy"; }}; strip-prefix "/different"; continue; }}
 rule {{ match {{ path-glob "/errortop404"; }}; strip-prefix "/different"; continue; }}
 rule {{ match {{ path-glob "/unsafe/*"; }}; rewrite "/file"; continue; }}
 rule {{ match {{ path-glob "/upstream"; }}; proxy "http://{upstream}"; }}
 rule {{ serve-files root="{root}"; }}
 handle-errors {{
  rule {{ match {{ all {{ error-status 404; any {{ path-glob "/errorproxy"; path-glob "/errortop404"; }}; }}; }}; proxy "http://{upstream}"; }}
  rule {{ match {{ all {{ error-status 404; path-glob "/fail"; }}; }}; rewrite "/no-error-page"; serve-files root="{root}"; }}
  rule {{ match {{ all {{ error-status 404; path-glob "/via/*"; }}; }}; rewrite "/404.html"; reenter; }}
  rule {{ match {{ error-status 404; }}; rewrite "/404.html"; serve-files root="{root}"; }}
  rule {{ match {{ error-status 405; }}; rewrite "/405.html"; serve-files root="{root}"; }}
 }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    let (status, headers, body) = send(address, "app.test", "/absent")?;
    assert_eq!((status, body.as_slice()), (404, b"missing page".as_slice()));
    assert!(headers.to_ascii_lowercase().contains("x-frozen: absent"));
    assert_eq!(send(address, "app.test", "/stripx")?.2, b"missing page");
    assert_eq!(send(address, "app.test", "/miss")?.2, b"missing page");
    assert_eq!(send(address, "app.test", "/via/test")?.2, b"missing page");
    assert_eq!(send(address, "app.test", "/outside")?.0, 403);
    assert_eq!(send(address, "app.test", "/file/child")?.0, 403);
    assert_eq!(send(address, "app.test", "/unsafe/%2fetc")?.0, 400);
    assert_eq!(send(address, "app.test", "/fail")?.0, 404);
    let mut stream = connect(address)?;
    stream.write_all(b"GET /absent HTTP/1.1\r\nHost: app.test\r\nIf-None-Match: *\r\nRange: bytes=0-1\r\nConnection: close\r\n\r\n")?;
    let (status, _, body) = response(&mut stream)?;
    assert_eq!((status, body.as_slice()), (404, b"missing page".as_slice()));
    let mut stream = connect(address)?;
    stream.write_all(
        b"POST /file HTTP/1.1\r\nHost: app.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )?;
    let (status, headers, body) = response(&mut stream)?;
    assert_eq!((status, body.as_slice()), (405, b"method page".as_slice()));
    assert!(headers.to_ascii_lowercase().contains("allow: get, head"));
    let mut stream = connect(address)?;
    stream.write_all(b"HEAD /absent HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut stream)?.contains("404"));
    let (status, _, body) = send(address, "app.test", "/upstream")?;
    assert_eq!((status, body.as_slice()), (404, b"body".as_slice()));
    assert_eq!(send(address, "app.test", "/errorproxy")?.0, 503);
    let (status, _, body) = send(address, "app.test", "/errortop404")?;
    assert_eq!((status, body.as_slice()), (404, b"body".as_slice()));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn error_chain_shares_steps_and_filter_capacity() -> TestResult {
    let address = vacant()?;
    let rules = "rule { continue; };".repeat(510);
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ {rules}
 rule {{ strip-prefix "/no"; continue; }}
 handle-errors {{ rule {{ match {{ error-status 404; }}; respond 404 text="handled"; }}; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/")?.0, 503);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);

    let address = vacant()?;
    let rules = "rule { header \"response\" { add \"X-Test\" \"v\"; }; continue; };".repeat(256);
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ {rules}
 rule {{ strip-prefix "/no"; continue; }}
 handle-errors {{ rule {{ header "response" {{ set "X-Test" "other"; }}; respond 404; }}; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/")?.0, 503);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);

    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ match {{ path-glob "/present"; }}; respond 200; }}
 handle-errors {{ rule {{ match {{ error-status 404; }}; respond 404 text="local"; }}; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    let (status, _, body) = send(address, "app.test", "/absent")?;
    assert_eq!((status, body.as_slice()), (404, b"local".as_slice()));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn chain_error_method_rejects_invalid_paths_and_preserves_unmatched_405() -> TestResult {
    let directory = Directory::new()?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ header "response" {{ set "X-Filter" "applied"; }}; serve-files root="{}"; }}
 handle-errors {{ rule {{ match {{ error-status 404; }}; respond 404 text="error handler"; }}; }}
}} }}"#,
        directory.0.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    for (path, expected) in [("/%2e%2e/file", 400), ("/%2ffile", 400), ("/file", 405)] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: app.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        let (status, headers, body) = response(&mut stream)?;
        assert_eq!(status, expected, "{path}");
        assert!(!String::from_utf8_lossy(&body).contains("error handler"));
        if expected == 405 {
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("allow: get, head"), "{headers}");
            assert!(headers.contains("x-filter: applied"), "{headers}");
            assert!(body.is_empty());
        }
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ serve-files root="{}"; }}
 handle-errors {{ rule {{ match {{ error-status 405; }}; respond 405 text="caught"; }}; }}
}} }}"#,
        directory.0.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    for path in ["/%2e%2e/file", "/%2ffile"] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: app.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        let (status, _, body) = response(&mut stream)?;
        assert_eq!(status, 400);
        assert_ne!(body, b"caught");
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn chain_branch_captures_inherit_and_override() -> TestResult {
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{
 rule {{ match {{ path-regex "^/(outer)/"; }};
   header "response" {{ set "X-Frozen" "{{capture.1}}"; }};
   branch {{
     when {{ match {{ path-regex "^/outer/(start)$"; }}; header "response" {{ set "X-Override" "{{capture.1}}"; }}; rewrite "/outer/changed"; continue; }}
     when {{ match {{ method "GET"; }}; rewrite "/{{capture.1}}/changed"; header "response" {{ set "X-Branch" "{{capture.1}}"; }}; continue; }}
     otherwise {{ rewrite "/{{capture.1}}/other"; header "response" {{ set "X-Branch" "{{capture.1}}"; }}; continue; }}
   }};
 }}
 rule {{ match {{ path-regex "^/outer/(changed|other)$"; }}; header "response" {{ set "X-Next" "{{capture.1}}"; }}; respond 200 text="ok"; }}
}} }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    for (method, path, next) in [
        ("GET", "/outer/value", "changed"),
        ("POST", "/outer/value", "other"),
        ("GET", "/outer/start", "changed"),
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: app.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        let (status, headers, _) = response(&mut stream)?;
        let headers = headers.to_ascii_lowercase();
        assert_eq!(status, 200, "{headers}");
        assert!(headers.contains("x-frozen: outer"), "{headers}");
        if path.ends_with("start") {
            assert!(headers.contains("x-override: start"), "{headers}");
        } else {
            assert!(headers.contains("x-branch: outer"), "{headers}");
        }
        assert!(headers.contains(&format!("x-next: {next}")), "{headers}");
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn authentication_missing_fails_closed() -> TestResult {
    let address = vacant()?;
    let hash = bcrypt::hash("dummy", 10)?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; respond 200; }}; handle-errors {{ rule {{ respond 404; }}; }}; }}; }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    assert_eq!(send(address, "app.test", "/")?.0, 401);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn authentication_basic_and_token_enforce_and_clear_credentials() -> TestResult {
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new()?;
    let token = directory.0.join("token");
    std::fs::write(&token, "dummy-token\n")?;
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600))?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?.to_ascii_lowercase();
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("x-api-token:"));
        assert_eq!(request.matches("x-before: once").count(), 1);
        reply(&mut stream, b"ok", true)?;
        Ok(())
    });
    let hash = bcrypt::hash("dummy-password", 10)?;
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ header "request" {{ add "X-Before" "once"; }}; branch {{ otherwise {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; static-token header="X-Api-Token" file="{}"; proxy "http://{upstream}"; }}; }}; }}; handle-errors {{ rule {{ respond 404; }}; }}; }}; }}"#,
        token.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    for extra in [
        "",
        "Authorization: Basic !!!\r\n",
        "Authorization: Basic YWxpY2U6YmFk\r\n",
        "Authorization: Basic YWxpY2U6YmFk\r\nAuthorization: Basic YWxpY2U6YmFk\r\n",
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: app.test\r\n{extra}Connection: close\r\n\r\n"
        )?;
        let (status, headers, _) = response(&mut stream)?;
        assert_eq!(status, 401);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("www-authenticate: basic realm=\"private\"")
        );
    }
    let credentials = base64::engine::general_purpose::STANDARD.encode("alice:dummy-password");
    let oversized_token = format!("X-Api-Token: {}\r\n", "a".repeat(4097));
    for token_header in [
        "",
        "X-Api-Token: bad\r\n",
        "X-Api-Token: dummy-tokem\r\n",
        "X-Api-Token: \r\n",
        oversized_token.as_str(),
        "X-Api-Token: dummy-token\r\nX-Api-Token: dummy-token\r\n",
        "X-Api-Token: dummy-token\r\n",
    ] {
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credentials}\r\nProxy-Authorization: dummy\r\n{token_header}Connection: close\r\n\r\n"
        )?;
        let (status, _, _) = response(&mut stream)?;
        assert_eq!(
            status,
            if token_header == "X-Api-Token: dummy-token\r\n" {
                200
            } else {
                401
            }
        );
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "backend panic")??;
    Ok(())
}

#[test]
fn authentication_argon2_and_unknown_user() -> TestResult {
    use argon2::{
        Algorithm, Argon2, Params, Version,
        password_hash::{PasswordHasher, SaltString},
    };
    use base64::Engine;
    let salt = SaltString::encode_b64(b"dummy-salt")?;
    let hash = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(8192, 1, 1, Some(32))?,
    )
    .hash_password(b"dummy-password", &salt)?
    .to_string();
    let long_password = "a".repeat(80);
    let long_hash = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(8192, 1, 1, Some(32))?,
    )
    .hash_password(long_password.as_bytes(), &salt)?
    .to_string();
    let address = vacant()?;
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; user "bob" hash="{long_hash}"; }}; respond 200 text="ok"; }}; }}; }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    for (user, password, expected) in [
        ("alice", "dummy-password", 200),
        ("alice", "bad", 401),
        ("unknown", "dummy-password", 401),
        ("bob", long_password.as_str(), 200),
    ] {
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        let mut stream = connect(address)?;
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {credential}\r\nConnection: close\r\n\r\n"
        )?;
        assert_eq!(response(&mut stream)?.0, expected);
    }
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn authentication_password_capacity_is_bounded() -> TestResult {
    let address = vacant()?;
    let hash = "$2b$14$EGdrhbKUv8Oc9vGiXX0HQOxSg445d458Muh7DAHskb6QbtCvdxcie";
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; respond 200; }}; }}; }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    let mut workers = Vec::new();
    for _ in 0..4 {
        workers.push(thread::spawn(move || -> TestResult<u16> {
            let mut stream = connect(address)?;
            stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic YWxpY2U6YmFk\r\nConnection: close\r\n\r\n")?;
            Ok(response(&mut stream)?.0)
        }));
    }
    thread::sleep(std::time::Duration::from_millis(400));
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic YWxpY2U6YmFk\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    for worker in workers {
        assert_eq!(worker.join().map_err(|_| "worker panic")??, 503);
    }
    let mut stream = connect(address)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic YWxpY2U6YmFk\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 503);
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn authentication_reentry_reuses_only_same_action_and_bcrypt_never_truncates() -> TestResult {
    use base64::Engine;
    let password = "a".repeat(72);
    let hash = bcrypt::hash(&password, 10)?;
    for same_action in [true, false] {
        let address = vacant()?;
        let rules = if same_action {
            format!(
                r#"rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; branch {{ when {{ match {{ path-glob "/start"; }}; rewrite "/end"; reenter; }}; otherwise {{ respond 200; }}; }}; }};"#
            )
        } else {
            format!(
                r#"rule {{ match {{ path-glob "/start"; }}; basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; rewrite "/end"; reenter; }}; rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; respond 200; }};"#
            )
        };
        let config = format!(
            "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ {rules} }}; }}"
        );
        let mut proxy = Proxy::start(address, &config)?;
        for (password, expected) in [
            (&password, if same_action { 200 } else { 401 }),
            (&format!("{password}x"), 401),
        ] {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("alice:{password}"));
            let mut stream = connect(address)?;
            write!(
                stream,
                "GET /start HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic {encoded}\r\nConnection: close\r\n\r\n"
            )?;
            assert_eq!(response(&mut stream)?.0, expected);
        }
        proxy.signal()?;
        assert_eq!(proxy.finish()?.0, 0);
    }
    Ok(())
}

#[test]
fn authentication_disconnected_clients_do_not_release_running_capacity() -> TestResult {
    use std::time::{Duration, Instant};
    let address = vacant()?;
    let hash = "$2b$14$EGdrhbKUv8Oc9vGiXX0HQOxSg445d458Muh7DAHskb6QbtCvdxcie";
    let config = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ basic-auth realm="private" {{ user "alice" hash="{hash}"; }}; respond 200; }}; }}; }}"#
    );
    let mut proxy = Proxy::start(address, &config)?;
    let request = b"GET / HTTP/1.1\r\nHost: app.test\r\nAuthorization: Basic YWxpY2U6YmFk\r\nConnection: close\r\n\r\n";
    let mut clients = Vec::new();
    for _ in 0..4 {
        let mut client = connect(address)?;
        client.write_all(request)?;
        clients.push(client);
    }
    thread::sleep(Duration::from_millis(100));
    let mut probe = connect(address)?;
    let started = Instant::now();
    probe.write_all(request)?;
    assert_eq!(response(&mut probe)?.0, 503);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "capacity probe did not establish saturation before timeout"
    );
    drop(clients);
    let mut probe = connect(address)?;
    let started = Instant::now();
    probe.write_all(request)?;
    assert_eq!(response(&mut probe)?.0, 503);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "post-disconnect probe did not observe capacity"
    );
    proxy.signal()?;
    let (status, logs) = proxy.finish()?;
    assert_eq!(status, 0);
    assert!(logs.matches("AUTH_CAPACITY").count() >= 2);
    Ok(())
}

#[test]
fn h2c_chain_proxy_uses_real_h2_upstream() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    backend.set_nonblocking(true)?;
    let worker = thread::spawn(move || -> TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(backend)?;
            let (stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await??;
            let mut connection = h2::server::handshake(stream).await?;
            let (request, mut response) =
                tokio::time::timeout(std::time::Duration::from_secs(5), connection.accept())
                    .await?
                    .ok_or("no h2 request")??;
            assert_eq!(
                request.uri().authority().ok_or("no authority")?.as_str(),
                "backend.test"
            );
            assert_eq!(request.uri().scheme_str(), Some("http"));
            let head = http::Response::builder()
                .status(200)
                .header("content-length", "2")
                .body(())?;
            let mut body = response.send_response(head, false)?;
            body.send_data(bytes::Bytes::from_static(b"ok"), true)?;
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(200), connection.accept())
                    .await;
            Ok(())
        })
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"http://{upstream}\" protocol=\"h2c\" host=\"backend.test\"; }}; }}; }}"
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    let (status, _, body) = send(address, "app.test", "/hello")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"ok");
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    worker.join().map_err(|_| "h2 backend panicked")??;
    Ok(())
}

#[test]
fn h2c_grpc_rejects_http1_downstream_before_business_request() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    backend.set_nonblocking(true)?;
    let worker = thread::spawn(move || -> TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(backend)?;
            let (stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await??;
            let mut connection = h2::server::handshake(stream).await?;
            let request =
                tokio::time::timeout(std::time::Duration::from_secs(1), connection.accept()).await;
            assert!(!matches!(request, Ok(Some(Ok(_)))));
            Ok(())
        })
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"http://{upstream}\" protocol=\"h2c\"; }}; }}; }}"
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    write!(
        stream,
        "POST /grpc HTTP/1.1\r\nHost: app.test\r\nContent-Type: application/grpc\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    assert_eq!(response(&mut stream)?.0, 502);
    worker.join().map_err(|_| "h2 backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn upstream_request_is_not_replayed_after_backend_receives_headers() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let worker = thread::spawn(move || -> TestResult {
        let mut stream = accept(&backend)?;
        let request = head(&mut stream)?;
        assert!(request.starts_with("GET /no-replay "));
        drop(stream);
        backend.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match backend.accept() {
                Ok(_) => return Err("request was replayed on another connection".into()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ proxy \"http://{upstream}\"; }}"
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(send(address, "app.test", "/no-replay")?.0, 502);
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn h2c_request_is_not_replayed_after_backend_receives_headers() -> TestResult {
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    backend.set_nonblocking(true)?;
    let worker = thread::spawn(move || -> TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(backend)?;
            let (stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await??;
            let mut connection = h2::server::handshake(stream).await?;
            let (request, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), connection.accept())
                    .await?
                    .ok_or("missing request")??;
            assert_eq!(request.uri().path(), "/no-replay");
            drop(connection);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await
                    .is_err(),
                "h2 request replayed"
            );
            Ok(())
        })
    });
    let address = vacant()?;
    let configuration = format!(
        "server {{ http \"{address}\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"http://{upstream}\" protocol=\"h2c\"; }}; }}; }}"
    );
    let mut proxy = Proxy::start(address, &configuration)?;
    assert_eq!(send(address, "app.test", "/no-replay")?.0, 502);
    worker.join().map_err(|_| "backend panicked")??;
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}
