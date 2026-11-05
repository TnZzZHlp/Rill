use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use http::{Request as HttpRequest, Response as HttpResponse};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming, body::Body};
use tower::{Service, service_fn};

#[derive(Clone, PartialEq, prost::Message)]
pub struct Packet {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(uint32, tag = "2")]
    pub count: u32,
}

#[derive(Clone, Default)]
pub struct Probe {
    cancelled: Arc<AtomicUsize>,
}

struct Pending {
    counter: Arc<AtomicUsize>,
    finished: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.finished {
            let count = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            println!("cancelled={count}");
        }
    }
}

impl tonic::server::NamedService for Probe {
    const NAME: &'static str = "acceptance.Probe";
}

impl Service<HttpRequest<Body>> for Probe {
    type Response = HttpResponse<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: HttpRequest<Body>) -> Self::Future {
        let path = request.uri().path().to_owned();
        let probe = self.clone();
        Box::pin(async move {
            let mut grpc =
                tonic::server::Grpc::new(tonic_prost::ProstCodec::<Packet, Packet>::default())
                    .max_decoding_message_size(1024 * 1024)
                    .max_encoding_message_size(1024 * 1024);
            let response = match path.as_str() {
                "/acceptance.Probe/Unary" => {
                    grpc.unary(
                        service_fn(move |request| probe.clone().unary(request)),
                        request,
                    )
                    .await
                }
                "/acceptance.Probe/ClientStream" => {
                    grpc.client_streaming(service_fn(client_stream), request)
                        .await
                }
                "/acceptance.Probe/ServerStream" => {
                    grpc.server_streaming(
                        service_fn(move |request| probe.clone().server_stream(request)),
                        request,
                    )
                    .await
                }
                "/acceptance.Probe/Bidi" => {
                    grpc.streaming(
                        service_fn(move |request| probe.clone().bidi(request)),
                        request,
                    )
                    .await
                }
                _ => Status::unimplemented("unknown method").into_http(),
            };
            Ok(response)
        })
    }
}

impl Probe {
    async fn unary(
        self,
        request: Request<Packet>,
    ) -> std::result::Result<Response<Packet>, Status> {
        let packet = request.into_inner();
        if packet.text == "error" {
            let mut error = Status::invalid_argument("probe error ü");
            error
                .metadata_mut()
                .insert("x-proof", "trailers-only".parse().unwrap());
            return Err(error);
        }
        if packet.text == "stats" {
            return Ok(Response::new(Packet {
                text: "cancelled".into(),
                count: self.cancelled.load(Ordering::SeqCst) as u32,
            }));
        }
        if packet.text == "slow" {
            let mut pending = Pending {
                counter: self.cancelled.clone(),
                finished: false,
            };
            tokio::time::sleep(Duration::from_secs(10)).await;
            pending.finished = true;
        }
        Ok(Response::new(packet))
    }

    async fn server_stream(
        self,
        request: Request<Packet>,
    ) -> std::result::Result<Response<ReceiverStream<std::result::Result<Packet, Status>>>, Status>
    {
        let packet = request.into_inner();
        if packet.count > 100 {
            return Err(Status::resource_exhausted("count exceeds 100"));
        }
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut pending = Pending {
                counter: self.cancelled.clone(),
                finished: false,
            };
            for index in 0..packet.count {
                if packet.text == "stream-error" && index == 1 {
                    let _ = sender.send(Err(Status::data_loss("stream failed ü"))).await;
                    pending.finished = true;
                    return;
                }
                let reply = Packet {
                    text: packet.text.clone(),
                    count: index,
                };
                if sender.send(Ok(reply)).await.is_err() {
                    return;
                }
                tokio::select! {
                    _ = sender.closed() => return,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
            pending.finished = true;
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn bidi(
        self,
        request: Request<Streaming<Packet>>,
    ) -> std::result::Result<Response<ReceiverStream<std::result::Result<Packet, Status>>>, Status>
    {
        let mut stream = request.into_inner();
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut pending = Pending {
                counter: self.cancelled.clone(),
                finished: false,
            };
            loop {
                let message = tokio::select! {
                    _ = sender.closed() => return,
                    message = stream.message() => message,
                };
                match message {
                    Ok(Some(packet)) => {
                        if sender.send(Ok(packet)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        pending.finished = true;
                        return;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

async fn client_stream(
    request: Request<Streaming<Packet>>,
) -> std::result::Result<Response<Packet>, Status> {
    let mut stream = request.into_inner();
    let mut text = String::new();
    let mut count = 0;
    while let Some(packet) = stream.message().await? {
        text.push_str(&packet.text);
        count += 1;
        if text.len() > 4096 || count > 100 {
            return Err(Status::resource_exhausted("input exceeds test limits"));
        }
    }
    Ok(Response::new(Packet { text, count }))
}
