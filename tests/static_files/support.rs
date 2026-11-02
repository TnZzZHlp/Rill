use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub address: SocketAddr,
    child: Option<Child>,
}
impl Fixture {
    pub fn new() -> TestResult<Self> {
        let dir = std::env::temp_dir().join(format!(
            "rill-static-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir)?;
        let root = dir.join("root");
        fs::create_dir(&root)?;
        let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
        Ok(Self {
            dir,
            root,
            address,
            child: None,
        })
    }
    pub fn config(&self, options: &str) -> String {
        format!(
            "server {{ http \"{}\"; }}\nsite \"example.test\" {{ root \"{}\"; {options} }}\n",
            self.address,
            self.root.display()
        )
    }
    pub fn start(&mut self, options: &str) -> TestResult {
        self.start_config(&self.config(options), false)
    }
    pub fn start_config(&mut self, contents: &str, unprivileged: bool) -> TestResult {
        let config = self.dir.join("config.kdl");
        fs::write(&config, contents)?;
        if unprivileged {
            fs::set_permissions(&config, fs::Permissions::from_mode(0o644))?;
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_rill"));
        command
            .args(["run", "--config"])
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if unprivileged && is_root()? {
            command.gid(0).uid(65534);
        }
        let child = command.spawn()?;
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect_timeout(&self.address, Duration::from_millis(100)).is_ok() {
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
                return Err("server did not start".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn pid(&self) -> TestResult<u32> {
        Ok(self.child.as_ref().ok_or("missing child")?.id())
    }
    pub fn signal(&self) -> TestResult {
        let status = Command::new("kill")
            .args(["-TERM", &self.pid()?.to_string()])
            .status()?;
        if !status.success() {
            return Err("failed to signal server".into());
        }
        Ok(())
    }
    pub fn finish(&mut self) -> TestResult<std::process::ExitStatus> {
        let child = self.child.as_mut().ok_or("missing child")?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err("server did not exit".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn stream(&self, path: &str) -> TestResult<(TcpStream, String)> {
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n"
        )?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= 65536 {
                return Err("header too large".into());
            }
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        Ok((stream, String::from_utf8(head)?))
    }
    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &str,
    ) -> TestResult<(u16, String, Vec<u8>)> {
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: example.test\r\n{headers}Connection: close\r\n\r\n"
        )?;
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() > 131072 {
                return Err("header too large".into());
            }
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
        }
        let head = String::from_utf8(bytes)?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .ok_or("missing status")?
            .parse()?;
        let length = header(&head, "content-length")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut body = vec![
            0;
            if method == "HEAD" || status == 304 {
                0
            } else {
                length
            }
        ];
        stream.read_exact(&mut body)?;
        if method == "HEAD" {
            let mut extra = [0];
            assert_eq!(stream.read(&mut extra)?, 0);
        }
        Ok((status, head, body))
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
pub fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

pub fn is_root() -> TestResult<bool> {
    let status = fs::read_to_string("/proc/self/status")?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or("missing Uid in /proc/self/status")?
        .split_whitespace()
        .next()
        .ok_or("missing real uid")?
        .parse::<u32>()?;
    Ok(uid == 0)
}
