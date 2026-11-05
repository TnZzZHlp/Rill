use std::thread;

use tokio::net::TcpListener;

use super::snapshot::TestResult;

const PATTERN: &[u8] = b"abcd";

struct ChunkReplacer {
    pending: Vec<u8>,
    emitted: Vec<u8>,
}

impl ChunkReplacer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            emitted: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        let keep = PATTERN.len() - 1;
        while let Some(index) = self
            .pending
            .windows(PATTERN.len())
            .position(|window| window == PATTERN)
        {
            self.emitted.extend_from_slice(&self.pending[..index]);
            self.emitted.push(b'X');
            self.pending.drain(..index + PATTERN.len());
        }
        if self.pending.len() > keep {
            let stable = self.pending.len() - keep;
            self.emitted.extend_from_slice(&self.pending[..stable]);
            self.pending.drain(..stable);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.emitted.append(&mut self.pending);
        self.emitted
    }
}

fn replace_in_chunks(chunks: &[&[u8]]) -> Vec<u8> {
    let mut replacer = ChunkReplacer::new();
    for chunk in chunks {
        replacer.push(chunk);
    }
    replacer.finish()
}

const INPUT: &[u8] = b"0123456789abcd0123456789abcd0123456789abcd";

#[test]
fn literal_replacement_output_is_independent_of_input_chunking() {
    let expected = b"0123456789X0123456789X0123456789X".to_vec();
    for split in 0..=INPUT.len() {
        let (head, tail) = INPUT.split_at(split);
        assert_eq!(
            replace_in_chunks(&[head, tail]),
            expected,
            "split at {split}"
        );
    }
    for stride in [1usize, 2, 3, 5, 7, 11, 13, 17, 19, 23] {
        let mut chunks = Vec::new();
        let mut rest = INPUT;
        while rest.len() > stride {
            let (head, tail) = rest.split_at(stride);
            chunks.push(head);
            rest = tail;
        }
        chunks.push(rest);
        assert_eq!(replace_in_chunks(&chunks), expected, "stride {stride}");
    }
    assert_eq!(replace_in_chunks(&[INPUT]), expected);
}

#[test]
fn literal_match_holds_the_bounded_prefix_until_end_of_stream() {
    let mut matched = ChunkReplacer::new();
    matched.push(b"abc");
    assert!(matched.emitted.is_empty());
    assert_eq!(matched.pending, b"abc".to_vec());
    matched.push(b"d");
    assert_eq!(matched.emitted, b"X".to_vec());
    assert_eq!(matched.finish(), b"X".to_vec());

    let mut tail = ChunkReplacer::new();
    tail.push(b"abc");
    assert_eq!(tail.finish(), b"abc".to_vec());

    assert_eq!(replace_in_chunks(&[b"ab", b"cd"]), b"X".to_vec());
    assert_eq!(replace_in_chunks(&[b"a", b"b", b"c", b"d"]), b"X".to_vec());
    assert_eq!(replace_in_chunks(&[b"abc", b"dc"]), b"Xc".to_vec());
}

fn serve_h2(
    api: &'static str,
) -> TestResult<(std::net::SocketAddr, thread::JoinHandle<TestResult>)> {
    let (address_sender, address_receiver) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || -> TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            address_sender.send(listener.local_addr()?)?;
            let (stream, _) = listener.accept().await?;
            let mut connection = h2::server::handshake(stream).await?;
            while let Some(request) = connection.accept().await {
                let Ok((request, mut respond)) = request else {
                    continue;
                };
                let mut body = request.into_body();
                let mut received = Vec::new();
                let mut frames = 0usize;
                loop {
                    match body.data().await {
                        Some(Ok(chunk)) => {
                            frames += 1;
                            received.extend_from_slice(&chunk);
                            body.flow_control().release_capacity(chunk.len())?;
                        }
                        Some(Err(_)) => break,
                        None => break,
                    }
                }
                let reply = http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .header("x-request-frames", frames.to_string())
                    .header("trailer", "grpc-status")
                    .body(());
                let Ok(reply) = reply else { continue };
                let Ok(mut send) = respond.send_response(reply, false) else {
                    continue;
                };
                let mut payload = api.as_bytes().to_vec();
                payload.extend_from_slice(&received);
                if send.send_data(bytes::Bytes::from(payload), false).is_err() {
                    continue;
                }
                let mut trailers = http::HeaderMap::new();
                trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                trailers.insert("grpc-message", http::HeaderValue::from_static("ok"));
                let _ = send.send_trailers(trailers);
            }
            Ok(())
        })
    });
    Ok((address_receiver.recv()?, handle))
}

async fn h2_call(
    address: std::net::SocketAddr,
    target: &'static str,
    chunks: &'static [&'static [u8]],
) -> TestResult<(u16, usize, Vec<u8>, Option<http::HeaderMap>)> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    let (mut client, connection) = h2::client::handshake(stream).await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("http://example.test{target}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())?;
    let (response, mut send) = client.send_request(request, false)?;
    let last = chunks.len().saturating_sub(1);
    for (index, chunk) in chunks.iter().enumerate() {
        send.send_data(bytes::Bytes::from_static(chunk), index == last)?;
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), response).await??;
    let status = response.status().as_u16();
    let frames = response
        .headers()
        .get("x-request-frames")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or("missing frame count")?;
    let mut body = response.into_body();
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await.transpose()? {
        data.extend_from_slice(&chunk);
    }
    let trailers = body.trailers().await?;
    driver.abort();
    Ok((status, frames, data, trailers))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2c_unary_call_carries_grpc_trailers() -> TestResult {
    let (address, backend) = serve_h2("unary:")?;
    let (status, frames, body, trailers) = h2_call(address, "/unary", &[b"payload"]).await?;
    assert_eq!(status, 200);
    assert_eq!(frames, 1);
    assert_eq!(body, b"unary:payload");
    let trailers = trailers.ok_or("missing trailers")?;
    assert_eq!(
        trailers.get("grpc-status").map(|value| value.as_bytes()),
        Some(&b"0"[..])
    );
    assert_eq!(
        trailers.get("grpc-message").map(|value| value.as_bytes()),
        Some(&b"ok"[..])
    );
    drop(backend);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2c_request_streaming_preserves_chunk_order_and_trailers() -> TestResult {
    let (address, backend) = serve_h2("stream:")?;
    let (status, frames, body, trailers) =
        h2_call(address, "/stream", &[b"one", b"two", b"three"]).await?;
    assert_eq!(status, 200);
    assert_eq!(
        frames, 3,
        "each written chunk must stay a separate DATA frame"
    );
    assert_eq!(body, b"stream:onetwothree");
    assert!(trailers.is_some_and(|trailers| trailers.contains_key("grpc-status")));
    drop(backend);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2c_cancel_of_one_stream_keeps_the_connection_usable() -> TestResult {
    let (address, backend) = serve_h2("unary:")?;
    let stream = tokio::net::TcpStream::connect(address).await?;
    let (mut client, connection) = h2::client::handshake(stream).await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });

    let cancelled = http::Request::builder()
        .method("POST")
        .uri("http://example.test/cancel")
        .body(())?;
    let (response, mut send) = client.send_request(cancelled, false)?;
    send.send_data(bytes::Bytes::from_static(b"never delivered"), false)?;
    drop(response);
    send.send_reset(h2::Reason::CANCEL);

    let request = http::Request::builder()
        .method("POST")
        .uri("http://example.test/after")
        .body(())?;
    let (response, mut send) = client.send_request(request, false)?;
    send.send_data(bytes::Bytes::from_static(b"payload"), true)?;
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), response).await??;
    assert_eq!(
        response.status(),
        200,
        "sibling stream must still be served"
    );
    let mut body = response.into_body();
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await.transpose()? {
        data.extend_from_slice(&chunk);
    }
    assert_eq!(data, b"unary:payload");
    driver.abort();
    drop(backend);
    Ok(())
}
