use crate::{support::*, tls_support::Certificate};
use pingora::{
    connectors::http::Connector,
    http::RequestHeader,
    protocols::{http::client::HttpSession, tls::ALPN},
    upstreams::peer::HttpPeer,
};
use std::{fs, sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_transform_drains_before_trailers_and_failed_stream_does_not_break_sibling() -> TestResult
{
    let directory = Directory::new()?;
    let certificate = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
    certificate.save(&directory.0, "default", &[])?;
    fs::write(directory.0.join("large.html"), vec![b'a'; 1024 * 1024])?;
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream = backend.local_addr()?;
    let worker = tokio::spawn(async move {
        let (stream, _) = backend.accept().await?;
        let mut connection = h2::server::handshake(stream).await?;
        let (_, mut response) = connection.accept().await.ok_or("upstream request")??;
        let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
        let mut send = response.send_response(
            http::Response::builder()
                .status(200)
                .header("content-type", "text/html")
                .body(())?,
            false,
        )?;
        send.send_data(bytes::Bytes::from_static(b"ol"), false)?;
        send.send_data(bytes::Bytes::from_static(b"d cat"), false)?;
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-check", "kept".parse()?);
        trailers.insert("etag", "source".parse()?);
        send.send_trailers(trailers)?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        driver.abort();
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let address = vacant()?;
    let config = format!(
        r#"server {{ https "{address}"; }}
tls "default" {{ cert "{}"; key "{}"; }}
site "app.test" {{ tls "default"; chain {{
 rule {{ match {{ path-glob "/large.html"; }}; body-replace mime="text/html" {{ regex "a*" "x"; }}; serve-files root="{}"; }}
 rule {{ body-replace mime="text/html" {{ literal "old" "new"; }}; proxy "http://{upstream}" protocol="h2c"; }}
}} }}"#,
        directory.0.join("default.pem").display(),
        directory.0.join("default.key").display(),
        directory.0.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let connector = Connector::new(None);
    let mut peer = HttpPeer::new(address, true, "app.test".into());
    peer.options.ca = Some(Arc::new(vec![certificate.cert.clone()].into_boxed_slice()));
    peer.options.alpn = ALPN::H2;
    peer.options.max_h2_streams = 10;
    let (session, _) = connector.get_http_session(&peer).await?;
    let HttpSession::H2(mut bad) = session else {
        return Err("h2 required".into());
    };
    let mut request = RequestHeader::build("GET", b"/large.html", None)?;
    request.set_uri("https://app.test/large.html".parse()?);
    bad.write_request_header(Box::new(request), true)?;
    bad.read_response_header().await?;
    assert_eq!(bad.response_header().ok_or("header")?.status.as_u16(), 200);
    let failure = loop {
        match bad.read_response_body().await {
            Err(error) => break error,
            Ok(Some(_)) => (),
            Ok(None) => return Err("failed transform ended successfully".into()),
        }
    };
    assert!(!failure.to_string().is_empty());
    let (session, reused) = connector.get_http_session(&peer).await?;
    assert!(reused, "multiplexed connection was not available");
    let HttpSession::H2(mut good) = session else {
        return Err("h2 required".into());
    };
    assert_eq!(
        bad.fd(),
        good.fd(),
        "sibling must use the same physical connection"
    );
    let mut request = RequestHeader::build("GET", b"/proxy", None)?;
    request.set_uri("https://app.test/proxy".parse()?);
    good.write_request_header(Box::new(request), true)?;
    good.read_response_header().await?;
    let mut output = Vec::new();
    while let Some(bytes) = good.read_response_body().await? {
        output.extend_from_slice(&bytes);
    }
    assert_eq!(output, b"new cat");
    let trailers = good
        .read_trailers()
        .await?
        .ok_or("missing transformed trailers")?;
    assert_eq!(trailers.get("x-check").ok_or("x-check")?, "kept");
    assert!(!trailers.contains_key("etag"));
    worker.await??;
    Ok(())
}
