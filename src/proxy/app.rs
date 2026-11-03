use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::apps::{HttpServerApp, ReusedHttpStream, ServerApp};
use pingora::protocols::{Stream, http::ServerSession};
use pingora::proxy::HttpProxy;
use pingora::server::ShutdownWatch;
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout};

use super::handler::Handler;
use crate::observability::logging::Level;
use crate::runtime::state::RuntimeState;
use crate::runtime::stream::SharedStream;

#[derive(Clone, Copy, Debug)]
pub enum Phase {
    Ingress(Instant),
    Headers(Instant),
    Streaming,
}

pub struct App {
    pub proxy: Arc<HttpProxy<Handler>>,
    pub state: Arc<RuntimeState>,
}

#[async_trait]
impl HttpServerApp for App {
    async fn process_new_http(
        self: &Arc<Self>,
        mut session: ServerSession,
        shutdown: &ShutdownWatch,
    ) -> Option<ReusedHttpStream> {
        let shared = session
            .as_http1()
            .and_then(|session| session.stream().as_any().downcast_ref::<SharedStream>())
            .cloned();
        let idle = session
            .take_connection_user_context()
            .and_then(|context| context.downcast::<Duration>().ok())
            .map_or(Duration::from_secs(60), |value| *value);
        let (sender, mut receiver) = watch::channel(Phase::Ingress(Instant::now() + idle));
        if session.is_http2() {
            session.req_header_mut().extensions_mut().insert(sender);
        } else {
            session.set_connection_user_context(Some(Box::new(sender)));
        }
        let timed_out = {
            let request = self.proxy.process_new_http(session, shutdown);
            tokio::pin!(request);
            loop {
                let phase = *receiver.borrow_and_update();
                let (deadline, status) = match phase {
                    Phase::Ingress(deadline) => (deadline, 408),
                    Phase::Headers(deadline) => (deadline, 504),
                    Phase::Streaming => return request.await,
                };
                tokio::select! {
                    biased;
                    changed = receiver.changed() => {
                        if changed.is_err() { return request.await; }
                    }
                    result = &mut request => return result,
                    _ = sleep_until(deadline) => break status,
                }
            }
        };
        let Some(shared) = shared else {
            self.state.snapshot().logger.event(
                Level::Warn,
                "request_timeout",
                &[("protocol", "h2"), ("action", "reset_stream")],
            );
            return None;
        };
        self.state.snapshot().logger.event(
            Level::Warn,
            "request_timeout",
            &[("status", &timed_out.to_string())],
        );
        let mut error_session = ServerSession::new_http1(Box::new(shared));
        error_session.set_keepalive(None);
        let _ = timeout(
            Duration::from_secs(5),
            error_session.respond_error(timed_out),
        )
        .await;
        None
    }

    async fn http_cleanup(&self) {
        self.proxy.http_cleanup().await;
    }

    fn server_options(&self) -> Option<&pingora::apps::HttpServerOptions> {
        self.proxy.server_options.as_ref()
    }
}

pub struct Transport {
    pub app: Arc<App>,
}

#[async_trait]
impl ServerApp for Transport {
    async fn process_new(
        self: &Arc<Self>,
        stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        self.app
            .process_new(Box::new(SharedStream::new(stream)), shutdown)
            .await
    }
    async fn cleanup(&self) {
        self.app.http_cleanup().await;
    }
}
