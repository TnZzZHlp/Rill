use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};
use tokio::sync::watch;
use tokio::time::Instant;

use super::app::Phase;
use super::request::{Forwarding, authority, connection_names, forwarding};
use crate::observability::logging::Level;
use crate::runtime::state::{RuntimeSnapshot, RuntimeState};

pub struct Handler {
    pub state: Arc<RuntimeState>,
}

pub struct Context {
    snapshot: Arc<RuntimeSnapshot>,
    selected: Option<(usize, usize)>,
    forwarding: Option<Forwarding>,
    connection_names: Vec<String>,
    attempt: usize,
    budget: Option<watch::Sender<Phase>>,
    idle: Duration,
}

async fn reject(session: &mut Session, status: u16) -> Result<bool> {
    session.set_keepalive(None);
    session.respond_error(status).await?;
    Ok(true)
}

fn path(request: &RequestHeader) -> Option<String> {
    let target = std::str::from_utf8(request.raw_path()).ok()?;
    if target.contains('#') {
        return None;
    }
    let path = target.split('?').next()?;
    if !path.starts_with('/')
        || !path.is_ascii()
        || path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\')
    {
        return None;
    }
    let mut bytes = path.as_bytes().to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return None;
            }
            bytes[index + 1].make_ascii_uppercase();
            bytes[index + 2].make_ascii_uppercase();
            if &bytes[index + 1..index + 3] == b"00" || &bytes[index + 1..index + 3] == b"5C" {
                return None;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

#[async_trait]
impl ProxyHttp for Handler {
    type CTX = Context;

    fn new_ctx(&self) -> Context {
        Context {
            snapshot: self.state.snapshot(),
            selected: None,
            forwarding: None,
            connection_names: Vec::new(),
            attempt: 0,
            budget: None,
            idle: Duration::from_secs(60),
        }
    }

    fn on_connection_reuse(
        &self,
        _: &mut Session,
        ctx: &mut Context,
        previous: Box<dyn Any + Send + Sync>,
    ) {
        ctx.budget = previous
            .downcast::<watch::Sender<Phase>>()
            .ok()
            .map(|sender| *sender);
    }

    fn persist_connection_context(
        &self,
        _: &Session,
        ctx: &Context,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        Some(Box::new(ctx.idle))
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Context) -> Result<bool> {
        let request = session.req_header();
        let size = request.raw_path().len()
            + request
                .headers
                .iter()
                .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
                .sum::<usize>();
        if size > 64 * 1024 {
            return reject(session, 431).await;
        }
        let authority = match authority(request) {
            Ok(value) => value,
            Err(status) => return reject(session, status).await,
        };
        let path = match path(request) {
            Some(value) => value,
            None => return reject(session, 400).await,
        };
        let names = match connection_names(request) {
            Ok(value) => value,
            Err(status) => return reject(session, status).await,
        };
        let Some((site, route)) = ctx.snapshot.router.find(&authority.host, &path) else {
            return reject(session, 404).await;
        };
        let prepared = &ctx.snapshot.peers[site][route];
        if request.headers.contains_key("upgrade") || names.iter().any(|name| name == "upgrade") {
            let allowed = ctx.snapshot.configuration.sites[site].routes[route]
                .parameters
                .websocket
                .as_ref()
                .is_none_or(|value| value.value);
            return reject(session, if allowed { 501 } else { 403 }).await;
        }
        let peer = session
            .client_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.ip())
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "missing client address"))?;
        let local_port = session
            .server_addr()
            .and_then(|address| address.as_inet())
            .map_or(80, |address| address.port());
        let forwarding = match forwarding(
            request,
            &authority,
            peer,
            local_port,
            &ctx.snapshot.configuration.server,
        ) {
            Ok(value) => value,
            Err(status) => return reject(session, status).await,
        };
        ctx.idle = prepared.idle;
        if let Some(budget) = &ctx.budget {
            budget.send_replace(Phase::Headers(Instant::now() + prepared.request));
        }
        if session.get_keepalive().is_some() {
            session.set_keepalive(Some(
                ctx.idle
                    .as_secs()
                    .saturating_add(u64::from(ctx.idle.subsec_nanos() != 0))
                    .max(1),
            ));
        }
        session.set_read_timeout(Some(ctx.idle));
        session.set_write_timeout(Some(ctx.idle));
        session.set_total_drain_timeout(Some(ctx.idle));
        ctx.selected = Some((site, route));
        ctx.forwarding = Some(forwarding);
        ctx.connection_names = names;
        Ok(false)
    }

    async fn upstream_peer(&self, _: &mut Session, ctx: &mut Context) -> Result<Box<HttpPeer>> {
        let (site, route) = ctx
            .selected
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "route not selected"))?;
        ctx.snapshot.peers[site][route]
            .peer(ctx.attempt)
            .map(Box::new)
            .ok_or_else(|| Error::explain(ErrorType::ConnectError, "no upstream address").into_up())
    }

    async fn upstream_request_filter(
        &self,
        _: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        super::headers::request(
            request,
            &ctx.connection_names,
            ctx.forwarding.as_ref().ok_or_else(|| {
                Error::explain(ErrorType::InternalError, "forwarding policy not selected")
            })?,
        )
    }

    async fn response_filter(
        &self,
        _: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        super::headers::response(response)?;
        if response.status.as_u16() >= 200
            && let Some(budget) = &ctx.budget
        {
            if matches!(*budget.borrow(), Phase::Headers(deadline) if Instant::now() >= deadline) {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(504),
                    "upstream header deadline exceeded",
                )
                .into_up());
            }
            budget.send_replace(Phase::Streaming);
        }
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _: &mut Session,
        _: &HttpPeer,
        ctx: &mut Context,
        mut error: Box<Error>,
    ) -> Box<Error> {
        ctx.attempt += 1;
        let retry = ctx.selected.is_some_and(|(site, route)| {
            ctx.attempt < ctx.snapshot.peers[site][route].addresses.len().min(8)
        });
        error.set_retry(retry);
        error
    }

    fn error_while_proxy(
        &self,
        _: &HttpPeer,
        _: &mut Session,
        mut error: Box<Error>,
        _: &mut Context,
        _: bool,
    ) -> Box<Error> {
        error.set_retry(false);
        error
    }

    fn suppress_error_log(&self, _: &Session, _: &Context, _: &Error) -> bool {
        true
    }

    fn suppress_proxy_warn_log(
        &self,
        _: &Session,
        _: &Context,
        _: &Error,
        _: pingora::proxy::ProxyWarnLogContext,
    ) -> bool {
        true
    }

    async fn logging(&self, _: &mut Session, error: Option<&Error>, ctx: &mut Context) {
        if let Some(error) = error {
            ctx.snapshot.logger.event(
                Level::Warn,
                "proxy_error",
                &[
                    ("kind", &format!("{:?}", error.etype())),
                    ("generation", &ctx.snapshot.generation.to_string()),
                ],
            );
        }
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &Error,
        _: &mut Context,
    ) -> FailToProxy {
        let status = match error.etype() {
            ErrorType::HTTPStatus(status) => *status,
            ErrorType::ReadTimedout if error.esource() == &pingora::ErrorSource::Downstream => 408,
            _ if error.esource() == &pingora::ErrorSource::Downstream => 400,
            ErrorType::ConnectTimedout | ErrorType::ReadTimedout | ErrorType::WriteTimedout => 504,
            _ => 502,
        };
        session.set_keepalive(None);
        if session.response_written().is_none() {
            let _ = session.respond_error(status).await;
        }
        FailToProxy {
            error_code: status,
            can_reuse_downstream: false,
        }
    }
}
