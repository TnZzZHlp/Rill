use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use pingora::protocols::raw_connect::ProxyDigest;
use pingora::protocols::tls::{ALPN, SslDigest};
use pingora::protocols::{
    Digest, GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown, SocketDigest, Ssl,
    Stream, TimingDigest, UniqueID,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Debug)]
pub struct SharedStream {
    io: Arc<Mutex<Stream>>,
    digest: Digest,
    id: i32,
    alpn: Option<ALPN>,
}

impl SharedStream {
    pub fn new(stream: Stream) -> Self {
        Self {
            digest: Digest {
                ssl_digest: stream.get_ssl_digest(),
                timing_digest: stream.get_timing_digest(),
                proxy_digest: stream.get_proxy_digest(),
                socket_digest: stream.get_socket_digest(),
            },
            id: stream.id(),
            alpn: stream.selected_alpn_proto(),
            io: Arc::new(Mutex::new(stream)),
        }
    }
}

impl AsyncRead for SharedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.io.lock() {
            Ok(mut stream) => Pin::new(&mut **stream).poll_read(cx, buffer),
            Err(_) => Poll::Ready(Err(io::Error::other("stream lock poisoned"))),
        }
    }
}

impl AsyncWrite for SharedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.io.lock() {
            Ok(mut stream) => Pin::new(&mut **stream).poll_write(cx, buffer),
            Err(_) => Poll::Ready(Err(io::Error::other("stream lock poisoned"))),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.io.lock() {
            Ok(mut stream) => Pin::new(&mut **stream).poll_flush(cx),
            Err(_) => Poll::Ready(Err(io::Error::other("stream lock poisoned"))),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.io.lock() {
            Ok(mut stream) => Pin::new(&mut **stream).poll_shutdown(cx),
            Err(_) => Poll::Ready(Err(io::Error::other("stream lock poisoned"))),
        }
    }
}

#[async_trait]
impl Shutdown for SharedStream {
    async fn shutdown(&mut self) {
        let _ = tokio::io::AsyncWriteExt::shutdown(self).await;
    }
}
impl UniqueID for SharedStream {
    fn id(&self) -> i32 {
        self.id
    }
}
impl Ssl for SharedStream {
    fn get_ssl_digest(&self) -> Option<Arc<SslDigest>> {
        self.digest.ssl_digest.clone()
    }
    fn selected_alpn_proto(&self) -> Option<ALPN> {
        self.alpn.clone()
    }
}
impl GetTimingDigest for SharedStream {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        self.digest.timing_digest.clone()
    }
}
impl GetProxyDigest for SharedStream {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        self.digest.proxy_digest.clone()
    }
}
impl GetSocketDigest for SharedStream {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        self.digest.socket_digest.clone()
    }
}
impl Peek for SharedStream {}
