use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::protocol::{self, Request, Response};
use nix::unistd::geteuid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

pub async fn reload(path: &Path, duration: Duration) -> io::Result<Response> {
    let deadline = tokio::time::Instant::now()
        .checked_add(duration)
        .ok_or_else(|| io::Error::other("invalid timeout"))?;
    tokio::time::timeout_at(deadline, async {
        let mut stream = UnixStream::connect(path).await?;
        let uid = stream.peer_cred()?.uid();
        if uid != 0 && uid != geteuid().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "untrusted control peer",
            ));
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| io::Error::other("clock error"))?
            .as_nanos();
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let request = Request {
            version: 1,
            id,
            operation: "reload".into(),
        };
        protocol::write_frame(&mut stream, &protocol::encode_request(&request)?).await?;
        let mut reader = BufReader::new(stream);
        reader.fill_buf().await?;
        let frame = protocol::read_frame(&mut reader).await?;
        protocol::decode_response(&frame, &request.id)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "reload timed out"))?
}
