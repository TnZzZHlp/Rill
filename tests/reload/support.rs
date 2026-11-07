use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub dir: PathBuf,
    pub entry: PathBuf,
    pub socket: PathBuf,
    pub address: SocketAddr,
    child: Option<Child>,
}
impl Fixture {
    pub fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "rill-reload-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
        Ok(Self {
            entry: dir.join("entry.kdl"),
            socket: dir.join("control.sock"),
            dir,
            address,
            child: None,
        })
    }
    pub fn config(&self, root: &Path, cache: &str) -> String {
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ root \"{}\"; cache-control \"{}\"; }}\n",
            self.address,
            self.socket.display(),
            root.display(),
            cache
        )
    }
    pub fn start(&mut self) -> Result {
        let log = fs::File::create(self.dir.join("watch.log"))?;
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_rill"))
                .args(["run", "--config"])
                .arg(&self.entry)
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect_timeout(&self.address, Duration::from_millis(100)).is_ok()
                && fs::symlink_metadata(&self.socket)
                    .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                return Ok(());
            }
            if self
                .child
                .as_mut()
                .ok_or("missing child")?
                .try_wait()?
                .is_some()
            {
                return Err("server exited before listening".into());
            }
            if Instant::now() >= deadline {
                return Err("server startup timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn reload(&self) -> Result<rill::control::protocol::Response> {
        Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(rill::control::client::reload(
                &self.socket,
                Duration::from_secs(30),
            ))?)
    }
    pub fn cli(&self, args: &[&str]) -> Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_rill"))
            .arg("reload")
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .output()?)
    }
    pub fn connect(&self) -> Result<TcpStream> {
        let s = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(s)
    }
    pub fn request(stream: &mut TcpStream, path: &str) -> Result<(u16, String, Vec<u8>)> {
        write!(stream, "GET {path} HTTP/1.1\r\nHost: example.test\r\n\r\n")?;
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() >= 131072 {
                return Err("header too large".into());
            }
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
        }
        let headers = String::from_utf8(bytes)?;
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
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body)?;
        Ok((status, headers, body))
    }
    pub fn log_bytes(&self) -> Result<Vec<u8>> {
        Ok(fs::read(self.dir.join("watch.log"))?)
    }
    pub fn pid(&self) -> Result<u32> {
        Ok(self.child.as_ref().ok_or("missing child")?.id())
    }
    pub fn request_shutdown(&self) -> Result {
        if !Command::new("kill")
            .arg("-TERM")
            .arg(self.pid()?.to_string())
            .status()?
            .success()
        {
            return Err("signal failed".into());
        }
        Ok(())
    }
    pub fn signal(&mut self) -> Result {
        self.request_shutdown()?;
        let child = self.child.as_mut().ok_or("missing child")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err("shutdown timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}
