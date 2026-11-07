use super::support::{Fixture, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MASKED: &[u8] = &[0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
const CLOSE: &[u8] = &[0x88, 0x80, 1, 2, 3, 4];

fn head(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 128 * 1024 {
            return Err("header too large".into());
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    Ok(String::from_utf8(bytes)?)
}

fn exact(stream: &mut TcpStream, expected: &[u8]) -> Result {
    let mut bytes = vec![0; expected.len()];
    stream.read_exact(&mut bytes)?;
    assert_eq!(bytes, expected);
    Ok(())
}

fn accept(listener: &TcpListener) -> Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(e.into()),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn config(f: &Fixture, backend: SocketAddr) -> String {
    format!(
        "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ proxy \"http://{backend}\"; }}\n",
        f.address,
        f.socket.display()
    )
}

fn new_backend() -> Result<(SocketAddr, thread::JoinHandle<Result>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        head(&mut stream)?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew")?;
        Ok(())
    });
    Ok((address, worker))
}

fn assert_new(f: &Fixture) -> Result {
    let mut stream = f.connect()?;
    let (status, _, body) = Fixture::request(&mut stream, "/")?;
    assert_eq!(status, 200);
    assert_eq!(body, b"new");
    Ok(())
}

fn chunk(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        if line.len() >= 128 {
            return Err("chunk line too large".into());
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    let size = usize::from_str_radix(
        std::str::from_utf8(&line[..line.len() - 2])?
            .split(';')
            .next()
            .ok_or("missing chunk size")?,
        16,
    )?;
    if size > 1024 {
        return Err("chunk too large".into());
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    exact(stream, b"\r\n")?;
    Ok(data)
}

#[test]
fn websocket_survives_proxy_reload() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    fs::write(&f.entry, config(&f, listener.local_addr()?))?;
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n")?;
        for _ in 0..2 {
            exact(&mut stream, MASKED)?;
            stream.write_all(b"\x81\x02hi")?;
        }
        exact(&mut stream, CLOSE)?;
        stream.write_all(b"\x88\x00")?;
        Ok(())
    });
    f.start()?;
    let mut client = f.connect()?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    let headers = head(&mut client)?;
    assert!(headers.starts_with("HTTP/1.1 101 "), "{headers}");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=")
    );
    client.write_all(MASKED)?;
    exact(&mut client, b"\x81\x02hi")?;
    let (address, new_worker) = new_backend()?;
    fs::write(&f.entry, config(&f, address))?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_new(&f)?;
    new_worker.join().map_err(|_| "new backend panicked")??;
    client.write_all(MASKED)?;
    exact(&mut client, b"\x81\x02hi")?;
    client.write_all(CLOSE)?;
    exact(&mut client, b"\x88\x00")?;
    worker.join().map_err(|_| "websocket backend panicked")??;
    Ok(())
}

#[test]
fn sse_chunks_survive_proxy_reload() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    fs::write(&f.entry, config(&f, listener.local_addr()?))?;
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: 1\n\n\r\n")?;
        rx.recv_timeout(Duration::from_secs(5))?;
        stream.write_all(b"9\r\ndata: 2\n\n\r\n0\r\n\r\n")?;
        Ok(())
    });
    f.start()?;
    let mut client = f.connect()?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")?;
    let headers = head(&mut client)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ")
            && headers.contains("text/event-stream")
            && headers.contains("transfer-encoding: chunked"),
        "{headers}"
    );
    assert_eq!(chunk(&mut client)?, b"data: 1\n\n");
    let (address, new_worker) = new_backend()?;
    fs::write(&f.entry, config(&f, address))?;
    assert_eq!(f.reload()?.generation, Some(2));
    assert_new(&f)?;
    new_worker.join().map_err(|_| "new backend panicked")??;
    tx.send(())?;
    assert_eq!(chunk(&mut client)?, b"data: 2\n\n");
    assert!(chunk(&mut client)?.is_empty());
    worker.join().map_err(|_| "sse backend panicked")??;
    Ok(())
}

#[test]
fn large_file_keeps_old_root_during_reload() -> Result {
    const SIZE: usize = 64 * 1024 * 1024;
    let mut f = Fixture::new()?;
    let a = f.dir.join("a");
    let b = f.dir.join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    let mut file = File::create(a.join("file"))?;
    let block = [b'a'; 64 * 1024];
    for _ in 0..SIZE / block.len() {
        file.write_all(&block)?;
    }
    drop(file);
    fs::write(b.join("file"), b"b")?;
    fs::write(&f.entry, f.config(&a, "max-age=10"))?;
    f.start()?;
    let mut old = f.connect()?;
    old.write_all(b"GET /file HTTP/1.1\r\nHost: example.test\r\n\r\n")?;
    let headers = head(&mut old)?.to_ascii_lowercase();
    assert!(
        headers.starts_with("http/1.1 200 ") && headers.contains("cache-control: max-age=10"),
        "{headers}"
    );
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .ok_or("missing content length")?;
    assert_eq!(length, SIZE);
    let mut buf = [0; 64 * 1024];
    old.read_exact(&mut buf)?;
    assert!(buf.iter().all(|&byte| byte == b'a'));
    fs::write(&f.entry, f.config(&b, "max-age=20"))?;
    assert_eq!(f.reload()?.generation, Some(2));
    let mut new = f.connect()?;
    let (status, headers, body) = Fixture::request(&mut new, "/file")?;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("cache-control: max-age=20")
    );
    assert_eq!(body, b"b");
    let mut remaining = SIZE - buf.len();
    while remaining > 0 {
        let amount = remaining.min(buf.len());
        old.read_exact(&mut buf[..amount])?;
        assert!(buf[..amount].iter().all(|&byte| byte == b'a'));
        remaining -= amount;
    }
    Ok(())
}

#[test]
fn chain_proxy_stream_keeps_old_snapshot_across_reload() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let backend = listener.local_addr()?;
    let prefix = format!(
        "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ chain {{ rule {{ ",
        f.address,
        f.socket.display()
    );
    fs::write(
        &f.entry,
        format!("{prefix}proxy \"http://{backend}\"; }}; }}; }}"),
    )?;
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nold\r\n")?;
        rx.recv_timeout(Duration::from_secs(5))?;
        stream.write_all(b"4\r\ndone\r\n0\r\n\r\n")?;
        Ok(())
    });
    f.start()?;
    let mut client = f.connect()?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n")?;
    assert!(head(&mut client)?.starts_with("HTTP/1.1 200 "));
    assert_eq!(chunk(&mut client)?, b"old");
    fs::write(
        &f.entry,
        format!("{prefix}respond 200 text=\"new\"; }}; }}; }}"),
    )?;
    assert_eq!(f.reload()?.generation, Some(2));
    let mut new_client = f.connect()?;
    assert_eq!(Fixture::request(&mut new_client, "/")?.2, b"new");
    tx.send(())?;
    assert_eq!(chunk(&mut client)?, b"done");
    worker.join().map_err(|_| "backend panicked")??;
    Ok(())
}

#[test]
fn authentication_old_stream_survives_token_reload() -> Result {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new()?;
    let token = f.dir.join("token");
    fs::write(&token, "dummy-old")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let backend = listener.local_addr()?;
    fs::write(
        &f.entry,
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ chain {{ rule {{ static-token header=\"X-Token\" file=\"{}\"; proxy \"http://{backend}\"; }}; }}; }}",
            f.address,
            f.socket.display(),
            token.display()
        ),
    )?;
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        assert!(!head(&mut stream)?.to_ascii_lowercase().contains("x-token:"));
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nold\r\n")?;
        rx.recv_timeout(Duration::from_secs(5))?;
        stream.write_all(b"4\r\ndone\r\n0\r\n\r\n")?;
        Ok(())
    });
    f.start()?;
    let mut client = f.connect()?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nX-Token: dummy-old\r\n\r\n")?;
    assert!(head(&mut client)?.starts_with("HTTP/1.1 200 "));
    assert_eq!(chunk(&mut client)?, b"old");
    fs::write(&token, "dummy-new")?;
    assert_eq!(f.reload()?.generation, Some(2));
    let mut new_client = f.connect()?;
    new_client.write_all(
        b"GET / HTTP/1.1\r\nHost: example.test\r\nX-Token: dummy-old\r\nConnection: close\r\n\r\n",
    )?;
    assert!(head(&mut new_client)?.starts_with("HTTP/1.1 401 "));
    tx.send(())?;
    assert_eq!(chunk(&mut client)?, b"done");
    worker.join().map_err(|_| "backend panic")??;
    Ok(())
}

#[test]
fn file_log_follows_old_sse_snapshot_until_stream_ends() -> Result {
    let mut f = Fixture::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let backend = listener.local_addr()?;
    let old_log = f.dir.join("old.jsonl");
    let new_log = f.dir.join("new.jsonl");
    let old = config(&f, backend).replace("site \"example.test\" {", &format!("site \"example.test\" {{ site-label \"old\"; access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1;", old_log.display()));
    fs::write(&f.entry, old)?;
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || -> Result {
        let mut stream = accept(&listener)?;
        head(&mut stream)?;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: 1\n\n\r\n")?;
        rx.recv_timeout(Duration::from_secs(5))?;
        stream.write_all(b"0\r\n\r\n")?;
        Ok(())
    });
    f.start()?;
    let mut client = f.connect()?;
    client.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")?;
    assert!(head(&mut client)?.starts_with("HTTP/1.1 200"));
    assert_eq!(chunk(&mut client)?, b"data: 1\n\n");
    assert!(fs::read(&old_log)?.is_empty());
    let next = format!(
        "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ site-label \"new\"; access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1; chain {{ rule \"new\" {{ respond 200 text=\"ok\"; }} }} }}",
        f.address,
        f.socket.display(),
        new_log.display()
    );
    fs::write(&f.entry, next)?;
    assert_eq!(f.reload()?.generation, Some(2));
    let mut new_client = f.connect()?;
    assert_eq!(Fixture::request(&mut new_client, "/")?.0, 200);
    tx.send(())?;
    assert!(chunk(&mut client)?.is_empty());
    worker.join().map_err(|_| "backend panicked")??;
    for _ in 0..100 {
        if !fs::read(&old_log)?.is_empty() && !fs::read(&new_log)?.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let old = fs::read_to_string(&old_log)?;
    let new = fs::read_to_string(&new_log)?;
    assert!(old.contains("\"site\":\"old\""));
    assert!(old.contains("\"route\":\"/*\""));
    assert!(new.contains("\"site\":\"new\""));
    assert!(new.contains("\"route\":\"new\""));
    Ok(())
}
