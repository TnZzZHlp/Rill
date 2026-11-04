use async_trait::async_trait;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use std::future::Future;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 64 * 1024;

pub(super) fn unsupported_env_present() -> bool {
    [
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_COMPRESSION",
        "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some())
}

fn valid_endpoint(endpoint: &str) -> bool {
    endpoint.len() <= 256
        && endpoint.is_ascii()
        && !endpoint
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        && endpoint
            .strip_prefix("http://")
            .and_then(|s| s.strip_suffix("/v1/traces"))
            .and_then(|s| s.parse::<SocketAddr>().ok())
            .is_some_and(|addr| addr.ip().is_loopback() && addr.port() != 0)
}
fn valid_request(endpoint: &str, request: &Request<Bytes>) -> bool {
    request.method() == "POST" && request.uri() == endpoint && request.body().len() <= MAX_REQUEST
}
fn read_response(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RESPONSE {
        return Err(io::Error::other("trace response too large"));
    }
    Ok(bytes)
}

struct BoundedClient {
    client: reqwest::blocking::Client,
    endpoint: String,
}
impl std::fmt::Debug for BoundedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoundedClient")
    }
}
impl BoundedClient {
    fn new(endpoint: &str) -> Result<Self, ()> {
        if !valid_endpoint(endpoint) {
            return Err(());
        }
        let timeout = Duration::from_secs(2);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .connect_timeout(timeout)
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|_| ())?;
        Ok(Self {
            client,
            endpoint: endpoint.to_owned(),
        })
    }
}
#[async_trait]
impl HttpClient for BoundedClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        if !valid_request(&self.endpoint, &request) {
            return Err(io::Error::other("invalid trace request").into());
        }
        let mut response = self
            .client
            .post(&self.endpoint)
            .header("content-type", "application/x-protobuf")
            .body(request.into_body().to_vec())
            .send()
            .map_err(|_| io::Error::other("trace transport failed"))?;
        if response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > MAX_RESPONSE as u64)
        {
            return Err(io::Error::other("trace response too large").into());
        }
        let status = response.status();
        let body =
            read_response(&mut response).map_err(|_| io::Error::other("trace response failed"))?;
        Response::builder()
            .status(status)
            .body(Bytes::from(body))
            .map_err(|_| io::Error::other("invalid trace response").into())
    }
}

pub(super) fn build(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, ()> {
    let timeout = Duration::from_secs(2);
    let client = BoundedClient::new(endpoint)?;
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(timeout)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_http_client(client)
        .build()
        .map_err(|_| ())
}

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
pub(super) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_requires_exact_loopback_endpoint_and_bounded_body() {
        let endpoint = "http://127.0.0.1:4318/v1/traces";
        assert!(valid_endpoint(endpoint));
        assert!(!valid_endpoint("http://127.0.0.1.evil:4318/v1/traces"));
        assert!(!valid_endpoint("http://127.0.0.1:4318/v1/traces?secret"));
        let request = |method, uri, len| {
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Bytes::from(vec![0; len]))
                .unwrap()
        };
        assert!(valid_request(
            endpoint,
            &request("POST", endpoint, MAX_REQUEST)
        ));
        assert!(!valid_request(
            endpoint,
            &request("POST", endpoint, MAX_REQUEST + 1)
        ));
        assert!(!valid_request(endpoint, &request("GET", endpoint, 0)));
        assert!(!valid_request(
            endpoint,
            &request("POST", "http://127.0.0.1:4319/v1/traces", 0)
        ));
    }
    #[test]
    fn response_reader_rejects_one_byte_over_limit() {
        assert_eq!(
            read_response(&vec![0; MAX_RESPONSE][..]).unwrap().len(),
            MAX_RESPONSE
        );
        assert!(read_response(&vec![0; MAX_RESPONSE + 1][..]).is_err());
    }
}
