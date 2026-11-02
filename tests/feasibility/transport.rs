use std::fs::{DirBuilder, remove_dir_all};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use pingora::server::configuration::ServerConf;
use pingora::server::{ExecutionPhase, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora::upstreams::peer::HttpPeer;
use tokio::sync::Notify;

use super::snapshot::TestResult;

struct TemporaryDirectory(PathBuf);

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = remove_dir_all(&self.0);
    }
}

struct Stop(Arc<Notify>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[async_trait]
impl ShutdownSignalWatch for Stop {
    async fn recv(&self) -> ShutdownSignal {
        self.0.notified().await;
        ShutdownSignal::GracefulTerminate
    }
}

struct StreamingProxy(SocketAddr);

#[async_trait]
impl ProxyHttp for StreamingProxy {
    type CTX = ();

    fn new_ctx(&self) {}

    async fn upstream_peer(&self, _: &mut Session, _: &mut ()) -> pingora::Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(self.0, false, String::new())))
    }
}

#[test]
fn pingora_streams_before_upstream_completion_and_drains_during_shutdown() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!("rill-001-{}-{unique}", std::process::id()));
    DirBuilder::new().mode(0o700).create(&directory)?;
    let directory = TemporaryDirectory(directory);
    let socket = directory.0.join("proxy.sock");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let upstream = listener.local_addr()?;
    let notify = Arc::new(Notify::new());
    let configuration = ServerConf {
        threads: 1,
        grace_period_seconds: Some(1),
        graceful_shutdown_timeout_seconds: Some(2),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, configuration);
    let mut phase = server.watch_execution_phase();
    let mut service = http_proxy_service(&server.configuration, StreamingProxy(upstream));
    service.add_uds(
        socket.to_str().ok_or("non-UTF8 socket path")?,
        Some(std::fs::Permissions::from_mode(0o600)),
    );
    server.add_service(service);
    let (release, released) = mpsc::channel();
    thread::scope(|scope| -> TestResult {
        let backend = scope.spawn(move || -> TestResult {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err("upstream was never contacted".into());
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            connection.set_read_timeout(Some(Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                if headers.len() >= 8192 {
                    return Err("oversized test request".into());
                }
                let mut byte = [0];
                connection.read_exact(&mut byte)?;
                headers.push(byte[0]);
            }
            connection.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n1\r\na\r\n",
            )?;
            connection.flush()?;
            released.recv_timeout(Duration::from_secs(10))?;
            connection.write_all(b"1\r\nb\r\n0\r\n\r\n")?;
            connection.flush()?;
            Ok(())
        });
        let stop_watch = Stop(notify.clone());
        let service = scope.spawn(move || {
            server.run(RunArgs {
                shutdown_signal: Box::new(stop_watch),
            });
        });
        let _ensure_shutdown = Stop(notify.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut client = loop {
            match UnixStream::connect(&socket) {
                Ok(client) => break client,
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        client.set_read_timeout(Some(Duration::from_secs(5)))?;
        client.set_write_timeout(Some(Duration::from_secs(5)))?;
        client.write_all(
            b"GET /events HTTP/1.1\r\nHost: app.example.test\r\nConnection: close\r\n\r\n",
        )?;
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            if headers.len() >= 8192 {
                return Err("oversized test response".into());
            }
            let mut byte = [0];
            client.read_exact(&mut byte)?;
            headers.push(byte[0]);
        }
        assert!(headers.starts_with(b"HTTP/1.1 200"));
        let mut chunk = [0; 6];
        client.read_exact(&mut chunk)?;
        assert_eq!(&chunk, b"1\r\na\r\n");
        notify.notify_one();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match phase.try_recv() {
                Ok(ExecutionPhase::GracefulTerminate) => break,
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err("graceful shutdown was not observed".into());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        }
        release.send(())?;
        client.read_exact(&mut chunk)?;
        assert_eq!(&chunk, b"1\r\nb\r\n");
        backend.join().map_err(|_| "backend thread panicked")??;
        service.join().map_err(|_| "Pingora thread panicked")?;
        Ok(())
    })
}
