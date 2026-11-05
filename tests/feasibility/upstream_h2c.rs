use std::thread;

use pingora::http::RequestHeader;
use pingora::protocols::http::client::HttpSession;
use tokio::net::TcpListener;

use super::snapshot::TestResult;

fn serve(api: &'static str) -> TestResult<(std::net::SocketAddr, thread::JoinHandle<TestResult>)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || -> TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            sender.send(listener.local_addr()?)?;
            let (stream, _) = listener.accept().await?;
            let mut connection = h2::server::handshake(stream).await?;
            while let Some(request) = connection.accept().await {
                let (request, mut respond) = request?;
                let mut body = request.into_body();
                let mut received = Vec::new();
                while let Some(chunk) = body.data().await.transpose()? {
                    received.extend_from_slice(&chunk);
                    body.flow_control().release_capacity(chunk.len())?;
                }
                let reply = http::Response::builder().status(200).body(())?;
                let mut send = respond.send_response(reply, false)?;
                let mut payload = api.as_bytes().to_vec();
                payload.extend_from_slice(&received);
                send.send_data(bytes::Bytes::from(payload), true)?;
            }
            Ok(())
        })
    });
    Ok((receiver.recv()?, handle))
}

async fn call(address: std::net::SocketAddr) -> TestResult<Vec<u8>> {
    let connector = pingora::connectors::http::Connector::new(None);
    let mut peer = pingora::upstreams::peer::HttpPeer::new(address, false, "example.test".into());
    peer.options.set_http_version(2, 2);
    let (mut session, reused) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        connector.get_http_session(&peer),
    )
    .await??;
    assert!(!reused);
    let HttpSession::H2(h2) = &mut session else {
        return Err("connector did not open an HTTP/2 session".into());
    };
    let mut request = RequestHeader::build("POST", b"/unary", None)?;
    request.insert_header("content-type", "application/grpc")?;
    request.insert_header("host", "example.test")?;
    request.set_version(pingora::http::Version::HTTP_2);
    h2.write_request_header(Box::new(request), false)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        h2.write_request_body(bytes::Bytes::from_static(b"payload"), true),
    )
    .await??;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        h2.read_response_header(),
    )
    .await??;
    let mut body = Vec::new();
    while let Some(chunk) = h2.read_response_body().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

// Ignored on purpose: see docs/validation/013-caddy-001-contracts-feasibility.md 4.4.
// The raw h2 client/server pair in streaming.rs completes every run, while the
// Pingora 0.9.0 connector completes the same exchange only intermittently on this
// host. The probe repeats the exchange and prints each outcome so the failure
// rate stays reproducible instead of being asserted away.
#[ignore = "Pingora 0.9.0 h2 connector is intermittently unable to complete a call"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pingora_h2c_connector_completion_rate_is_recorded() -> TestResult {
    let mut completed = 0usize;
    let mut failed = 0usize;
    for attempt in 1..=10 {
        let (address, backend) = serve("unary:")?;
        match call(address).await {
            Ok(body) if body == b"unary:payload" => {
                completed += 1;
                println!("attempt {attempt}: completed");
            }
            Ok(body) => {
                failed += 1;
                println!("attempt {attempt}: unexpected body {body:?}");
            }
            Err(error) => {
                failed += 1;
                println!("attempt {attempt}: failed {error}");
            }
        }
        drop(backend);
    }
    println!("pingora connector: ok={completed} fail={failed}");
    assert_eq!(completed + failed, 10);
    Ok(())
}

// Control probe for the same record: identical topology and payload, direct h2
// client, repeated ten times. Run together with the connector probe above.
#[ignore = "comparison control for the connector completion-rate probe"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_h2_client_completion_rate_is_recorded() -> TestResult {
    let mut completed = 0usize;
    let mut failed = 0usize;
    for attempt in 1..=10 {
        let (address, backend) = serve("unary:")?;
        let result = raw_call(address).await;
        match result {
            Ok(body) if body == b"unary:payload" => {
                completed += 1;
                println!("raw attempt {attempt}: completed");
            }
            Ok(body) => {
                failed += 1;
                println!("raw attempt {attempt}: unexpected body {body:?}");
            }
            Err(error) => {
                failed += 1;
                println!("raw attempt {attempt}: failed {error}");
            }
        }
        drop(backend);
    }
    println!("raw h2 client: ok={completed} fail={failed}");
    assert_eq!(
        completed, 10,
        "the direct h2 client must complete every attempt"
    );
    Ok(())
}

async fn raw_call(address: std::net::SocketAddr) -> TestResult<Vec<u8>> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    let (mut client, connection) = h2::client::handshake(stream).await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = http::Request::builder()
        .method("POST")
        .uri("http://example.test/unary")
        .header("content-type", "application/grpc")
        .body(())?;
    let (response, mut send) = client.send_request(request, false)?;
    send.send_data(bytes::Bytes::from_static(b"payload"), true)?;
    let response = tokio::time::timeout(std::time::Duration::from_secs(15), response).await??;
    let mut body = response.into_body();
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await.transpose()? {
        data.extend_from_slice(&chunk);
    }
    driver.abort();
    Ok(data)
}
