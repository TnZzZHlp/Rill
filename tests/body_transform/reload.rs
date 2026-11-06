use super::http::read_body;
use crate::{reload_support::Fixture, support::*};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::Duration,
};

#[test]
fn reload_keeps_existing_transform_stream_on_its_original_snapshot() -> TestResult {
    let mut fixture = Fixture::new()?;
    let backend = TcpListener::bind("127.0.0.1:0")?;
    let upstream = backend.local_addr()?;
    let address = fixture.address;
    let socket = fixture.socket.clone();
    let configuration = |replacement: &str| {
        format!(
            r#"server {{ http "{}"; control-socket "{}"; }}
site "example.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "old" "{replacement}"; }}; proxy "http://{upstream}"; }}; }}; }}"#,
            address,
            socket.display()
        )
    };
    fs::write(&fixture.entry, configuration("OLD"))?;
    let (advance, wait) = mpsc::channel();
    let worker = thread::spawn(move || -> TestResult {
        let mut old = accept(&backend)?;
        head(&mut old)?;
        old.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nold\r\n")?;
        let first = thread::spawn(move || -> TestResult {
            wait.recv_timeout(Duration::from_secs(5))?;
            old.write_all(b"3\r\nold\r\n0\r\n\r\n")?;
            Ok(())
        });
        let mut new = accept(&backend)?;
        head(&mut new)?;
        new.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 3\r\nConnection: close\r\n\r\nold")?;
        first.join().map_err(|_| "old backend panicked")??;
        Ok(())
    });
    fixture.start()?;
    let mut old = fixture.connect()?;
    old.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut old)?.starts_with("HTTP/1.1 200 "));
    let mut first = [0; 8];
    old.read_exact(&mut first)?;
    assert_eq!(&first, b"3\r\nOLD\r\n");
    fs::write(&fixture.entry, configuration("NEW"))?;
    assert_eq!(fixture.reload()?.generation, Some(2));
    let mut new = fixture.connect()?;
    new.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")?;
    let (_, body) = read_body(&mut new)?;
    assert_eq!(body, b"NEW");
    advance.send(())?;
    let mut rest = Vec::new();
    old.read_to_end(&mut rest)?;
    assert_eq!(rest, b"3\r\nOLD\r\n0\r\n\r\n");
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}
