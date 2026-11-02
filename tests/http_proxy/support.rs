use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Directory(pub PathBuf);

impl Directory {
    pub fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "rill-003-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn vacant() -> io::Result<SocketAddr> {
    TcpListener::bind("127.0.0.1:0")?.local_addr()
}

pub fn connect(address: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

pub fn head(stream: &mut impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 128 * 1024 {
            return Err(io::Error::other("oversized test header"));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

pub fn response(stream: &mut TcpStream) -> io::Result<(u16, String, Vec<u8>)> {
    let headers = head(stream)?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("invalid status"))?;
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok((status, headers, body))
}

pub fn send(address: SocketAddr, host: &str, path: &str) -> TestResult<(u16, String, Vec<u8>)> {
    let mut stream = connect(address)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;
    Ok(response(&mut stream)?)
}

pub struct Proxy {
    pub child: Child,
    pub address: SocketAddr,
    _directory: Directory,
}

impl Proxy {
    pub fn start(address: SocketAddr, configuration: &str) -> TestResult<Self> {
        let directory = Directory::new()?;
        let file = directory.0.join("config.kdl");
        fs::write(&file, configuration)?;
        let child = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["run", "--config"])
            .arg(&file)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut result = Self {
            child,
            address,
            _directory: directory,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if result.child.try_wait()?.is_some() {
                let mut logs = String::new();
                if let Some(mut pipe) = result.child.stderr.take() {
                    pipe.read_to_string(&mut logs)?;
                }
                return Err(format!("proxy exited before listening: {logs}").into());
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err("proxy did not start listening".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(result)
    }

    pub fn signal(&mut self) -> TestResult {
        let result = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()?;
        if !result.success() {
            return Err("cannot signal proxy".into());
        }
        Ok(())
    }

    pub fn finish(&mut self) -> TestResult<(i32, String)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err("proxy did not terminate".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut logs = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_string(&mut logs)?;
        }
        Ok((status.code().unwrap_or(-1), logs))
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "backend not contacted",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn reply(stream: &mut TcpStream, body: &[u8], close: bool) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        body.len(),
        if close { "close" } else { "keep-alive" }
    )?;
    stream.write_all(body)?;
    stream.flush()
}
