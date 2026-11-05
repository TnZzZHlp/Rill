use std::time::Duration;

use http::uri::PathAndQuery;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Request,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};

use crate::{Result, service::Packet};

type Client = tonic::client::Grpc<Channel>;

fn packet(text: &str, count: u32) -> Packet {
    Packet {
        text: text.into(),
        count,
    }
}

async fn unary(client: &mut Client, value: Packet) -> std::result::Result<Packet, tonic::Status> {
    client
        .ready()
        .await
        .map_err(|error| tonic::Status::unavailable(error.to_string()))?;
    let response = client
        .unary(
            Request::new(value),
            PathAndQuery::from_static("/acceptance.Probe/Unary"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    Ok(response.into_inner())
}

pub async fn run(args: &[String]) -> Result {
    if args.len() != 3 {
        return Err("client URL CA ok|reject".into());
    }
    let mut endpoint = Endpoint::from_shared(args[0].clone())?
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8));
    if args[0].starts_with("https:") {
        endpoint = endpoint.tls_config(
            ClientTlsConfig::new()
                .domain_name("grpc.test")
                .ca_certificate(Certificate::from_pem(std::fs::read(&args[1])?)),
        )?;
    }
    let mut client = Client::new(endpoint.connect().await?);
    if args[2] == "poolcheck" {
        for (policy, succeeds) in [
            ("good", true),
            ("good", true),
            ("wrong-ca", false),
            ("wrong-name", false),
            ("h1", false),
            ("good", true),
            ("target-one", true),
            ("target-two", true),
            ("target-one", true),
            ("forced-collision", false),
            ("good", true),
        ] {
            client.ready().await?;
            let mut request = Request::new(packet(
                if policy.starts_with("target-") {
                    "backend-id"
                } else {
                    "pool"
                },
                1,
            ));
            request
                .metadata_mut()
                .insert("x-pool-policy", policy.parse()?);
            let response: std::result::Result<tonic::Response<Packet>, tonic::Status> = client
                .unary(
                    request,
                    PathAndQuery::from_static("/acceptance.Probe/Unary"),
                    tonic_prost::ProstCodec::<Packet, Packet>::default(),
                )
                .await;
            assert_eq!(response.is_ok(), succeeds, "policy {policy}: {response:?}");
            if policy.starts_with("target-") {
                let expected = if policy == "target-one" { "one" } else { "two" };
                assert_eq!(response?.into_inner().text, expected);
            }
            println!("PASS pool policy={policy} success={succeeds}");
        }
        return Ok(());
    }
    if args[2] == "reject" {
        let error = unary(&mut client, packet("ping", 1))
            .await
            .expect_err("invalid upstream TLS must reject");
        assert_eq!(error.code(), Code::Unavailable);
        println!("PASS {} upstream TLS rejection: {error}", args[0]);
        return Ok(());
    }
    if args[2] != "ok" {
        return Err("invalid client mode".into());
    }
    tokio::time::timeout(Duration::from_secs(40), exercise(&mut client, &args[0])).await??;
    Ok(())
}

async fn exercise(client: &mut Client, route: &str) -> Result {
    let original = packet("unary-protobuf", 42);
    assert_eq!(unary(client, original.clone()).await?, original);
    println!("PASS {route} unary");

    let original = packet(&"x".repeat(512 * 1024), 43);
    assert_eq!(unary(client, original.clone()).await?, original);
    println!("PASS {route} 512KiB flow-control");

    let (sender, receiver) = mpsc::channel(1);
    let upload = tokio::spawn(async move {
        for word in ["one", "two", "three"] {
            sender
                .send(packet(word, 0))
                .await
                .expect("client stream receiver alive");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });
    client.ready().await?;
    let response = client
        .client_streaming(
            Request::new(ReceiverStream::new(receiver)),
            PathAndQuery::from_static("/acceptance.Probe/ClientStream"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    upload.await?;
    assert_eq!(response.into_inner(), packet("onetwothree", 3));
    println!("PASS {route} client-stream");

    client.ready().await?;
    let response = client
        .server_streaming(
            Request::new(packet("server", 3)),
            PathAndQuery::from_static("/acceptance.Probe/ServerStream"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    for index in 0..3 {
        assert_eq!(stream.message().await?, Some(packet("server", index)));
    }
    assert_eq!(stream.message().await?, None);
    stream.trailers().await?;
    println!("PASS {route} server-stream and success trailers");

    client.ready().await?;
    let response = client
        .server_streaming(
            Request::new(packet("stream-error", 3)),
            PathAndQuery::from_static("/acceptance.Probe/ServerStream"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    assert_eq!(stream.message().await?, Some(packet("stream-error", 0)));
    let error = stream
        .message()
        .await
        .expect_err("error trailers after DATA required");
    assert_eq!(error.code(), Code::DataLoss);
    assert_eq!(error.message(), "stream failed ü");
    println!("PASS {route} grpc-status=15 and grpc-message after DATA");

    let (sender, receiver) = mpsc::channel(1);
    client.ready().await?;
    let response = client
        .streaming(
            Request::new(ReceiverStream::new(receiver)),
            PathAndQuery::from_static("/acceptance.Probe/Bidi"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    for index in 0..3 {
        let value = packet("bidi", index);
        sender.send(value.clone()).await?;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.message()).await??,
            Some(value)
        );
    }
    drop(sender);
    assert_eq!(stream.message().await?, None);
    stream.trailers().await?;
    println!("PASS {route} bidi responses before request EOF");

    let error = unary(client, packet("error", 0))
        .await
        .expect_err("trailers-only error required");
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(error.message(), "probe error ü");
    assert_eq!(
        error
            .metadata()
            .get("x-proof")
            .and_then(|v| v.to_str().ok()),
        Some("trailers-only")
    );
    println!("PASS {route} trailers-only grpc-status=3 and grpc-message");

    let before = unary(client, packet("stats", 0)).await?.count;
    client.ready().await?;
    let response = client
        .server_streaming(
            Request::new(packet("cancel", 100)),
            PathAndQuery::from_static("/acceptance.Probe/ServerStream"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await?;
    let mut stream = response.into_inner();
    assert_eq!(stream.message().await?, Some(packet("cancel", 0)));
    drop(stream);
    wait_cancel(client, before).await?;
    assert_eq!(
        unary(client, packet("sibling", 1)).await?,
        packet("sibling", 1)
    );
    println!("PASS {route} cancel reaches backend and sibling RPC succeeds");

    let before = unary(client, packet("stats", 0)).await?.count;
    client.ready().await?;
    let mut request = Request::new(packet("slow", 1));
    request.set_timeout(Duration::from_millis(150));
    let response: std::result::Result<tonic::Response<Packet>, tonic::Status> = client
        .unary(
            request,
            PathAndQuery::from_static("/acceptance.Probe/Unary"),
            tonic_prost::ProstCodec::<Packet, Packet>::default(),
        )
        .await;
    let error = response.expect_err("deadline must fail");
    assert!(
        matches!(error.code(), Code::Cancelled | Code::DeadlineExceeded),
        "unexpected deadline code: {error}"
    );
    wait_cancel(client, before).await?;
    assert_eq!(
        unary(client, packet("after-deadline", 1)).await?,
        packet("after-deadline", 1)
    );
    println!("PASS {route} deadline cancels backend and sibling RPC succeeds");
    Ok(())
}

async fn wait_cancel(client: &mut Client, before: u32) -> Result {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if unary(client, packet("stats", 0)).await?.count > before {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("backend did not observe cancellation within 3s".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
