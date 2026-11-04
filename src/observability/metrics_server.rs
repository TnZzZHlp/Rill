use super::metrics::Metrics;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

const HEADER_LIMIT: usize = 8192;
const TIMEOUT: Duration = Duration::from_secs(2);

fn response(status: &str, body: &[u8], extra: &str) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}
fn parse_header(bytes: &[u8]) -> Result<(), u16> {
    let text = std::str::from_utf8(bytes).map_err(|_| 400u16)?;
    if !text.is_ascii()
        || text
            .bytes()
            .any(|b| (b < 32 && b != b'\r' && b != b'\n' && b != b'\t') || b == 127)
    {
        return Err(400);
    }
    let mut lines = text.strip_suffix("\r\n\r\n").ok_or(400u16)?.split("\r\n");
    let request = lines.next().ok_or(400u16)?;
    if request.bytes().any(|b| b < 32) {
        return Err(400);
    }
    let mut parts = request.split(' ');
    let method = parts.next().ok_or(400u16)?;
    let path = parts.next().ok_or(400u16)?;
    let version = parts.next().ok_or(400u16)?;
    if parts.next().is_some()
        || method.is_empty()
        || path.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(400);
    }
    let mut content_length = false;
    for line in lines {
        if line.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(400);
        }
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            return Err(400);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(400);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length || value.trim_matches([' ', '\t']) != "0" {
                return Err(400);
            }
            content_length = true;
        }
    }
    if method != "GET" {
        return Err(405);
    }
    if path != "/metrics" {
        return Err(404);
    }
    Ok(())
}
async fn connection(mut stream: TcpStream, metrics: Arc<Metrics>) {
    let read = async {
        let mut header = Vec::with_capacity(HEADER_LIMIT);
        loop {
            if let Some(end) = header.windows(4).position(|v| v == b"\r\n\r\n") {
                if end + 4 != header.len() {
                    return Err(400);
                }
                return parse_header(&header);
            }
            if header.len() == HEADER_LIMIT {
                return Err(400);
            }
            let mut buf = [0; 1024];
            let cap = (HEADER_LIMIT - header.len()).min(buf.len());
            match stream.read(&mut buf[..cap]).await {
                Ok(0) | Err(_) => return Err(400),
                Ok(n) => header.extend_from_slice(&buf[..n]),
            }
        }
    };
    let status = tokio::time::timeout(TIMEOUT, read)
        .await
        .unwrap_or(Err(408));
    let bytes = match status {
        Ok(()) => match metrics.render() {
            Ok(body) => response(
                "200 OK",
                body.as_bytes(),
                "Content-Type: text/plain; version=0.0.4\r\n",
            ),
            Err(_) => response("500 Internal Server Error", b"", ""),
        },
        Err(405) => response("405 Method Not Allowed", b"", "Allow: GET\r\n"),
        Err(404) => response("404 Not Found", b"", ""),
        Err(408) => response("408 Request Timeout", b"", ""),
        Err(_) => response("400 Bad Request", b"", ""),
    };
    let _ = tokio::time::timeout(TIMEOUT, stream.write_all(&bytes)).await;
}
pub(crate) async fn serve(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(16));
    let mut tasks = JoinSet::new();
    let result = loop {
        if *shutdown.borrow() {
            break Ok(());
        }
        tokio::select! {
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break Ok(()); } }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept(), if tasks.len() < 16 => match accepted {
                Ok((stream, _)) => if let Ok(permit) = Arc::clone(&permits).try_acquire_owned() {
                    if socket2::SockRef::from(&stream).set_send_buffer_size(16 * 1024).is_err() {
                        continue;
                    }
                    let metrics = Arc::clone(&metrics);
                    tasks.spawn(async move { let _permit = permit; connection(stream, metrics).await; });
                },
                Err(error) => break Err(error),
            },
        }
    };
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(TIMEOUT, drain).await.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_and_route_policy() {
        assert!(parse_header(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").is_ok());
        assert_eq!(parse_header(b"POST /metrics HTTP/1.1\r\n\r\n"), Err(405));
        assert_eq!(parse_header(b"GET /other HTTP/1.0\r\n\r\n"), Err(404));
        assert_eq!(
            parse_header(
                b"GET /metrics HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"
            ),
            Err(400)
        );
        assert_eq!(
            parse_header(b"GET /metrics HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(400)
        );
    }
}
