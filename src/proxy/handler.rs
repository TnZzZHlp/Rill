use std::any::Any;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::protocols::Digest;
use pingora::proxy::{FailToProxy, ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType, Result};
use tokio::sync::watch;
use tokio::time::Instant;

use super::app::{Phase, PhaseChannel};
use super::request::{Forwarding, authority, connection_names, forwarding};
use crate::observability::logging::Level;
use crate::observability::{access::Outcome, request::RequestGuard};
use crate::runtime::state::{PreparedRoute, RuntimeSnapshot, RuntimeState};

pub struct Handler {
    pub state: Arc<RuntimeState>,
}

pub struct Context {
    snapshot: Arc<RuntimeSnapshot>,
    policy: Option<crate::http_policy::state::RequestPolicyState>,
    transform: crate::body_transform::response::Transform,
    error_status: Option<u16>,
    error_headers: Option<http::HeaderMap>,
    selected: Option<(usize, usize)>,
    dynamic: Option<super::dynamic::bridge::Ready>,
    forwarding: Option<Forwarding>,
    connection_names: Vec<String>,
    attempt: usize,
    addresses: Option<Arc<Vec<SocketAddr>>>,
    pool_lease: Option<Arc<super::pool_policy::PoolLease>>,
    websocket: Option<super::websocket::Handshake>,
    budget: Option<watch::Sender<Phase>>,
    idle: Duration,
    response_started: bool,
    observation: RequestGuard,
}

fn begin_response(budget: Option<&watch::Sender<Phase>>) -> Result<()> {
    if let Some(budget) = budget {
        let status = match *budget.borrow() {
            Phase::Ingress(deadline) if Instant::now() >= deadline => Some(408),
            Phase::Headers(deadline) if Instant::now() >= deadline => Some(504),
            _ => None,
        };
        if let Some(status) = status {
            return Err(Error::explain(
                ErrorType::HTTPStatus(status),
                "response deadline exceeded",
            ));
        }
        budget.send_replace(Phase::Streaming);
    }
    Ok(())
}

impl Context {
    fn start_response(&mut self) {
        if let Some(budget) = &self.budget {
            budget.send_replace(Phase::Streaming);
        }
        self.response_started = true;
    }

    fn begin_response(&mut self) -> Result<()> {
        begin_response(self.budget.as_ref())?;
        self.response_started = true;
        Ok(())
    }
}

async fn reject(session: &mut Session, ctx: &mut Context, status: u16) -> Result<bool> {
    let status = match ctx.begin_response() {
        Ok(()) => status,
        Err(error) => {
            let status = match error.etype() {
                ErrorType::HTTPStatus(status) => *status,
                _ => status,
            };
            ctx.start_response();
            status
        }
    };
    let status = if ctx.error_status == Some(503) {
        503
    } else {
        status
    };
    session.set_keepalive(None);
    session.respond_error(status).await?;
    Ok(true)
}

async fn reject_dynamic(session: &mut Session, ctx: &mut Context, status: u16) -> Result<bool> {
    let status = if ctx.error_status == Some(503) {
        503
    } else {
        status
    };
    reject(session, ctx, status).await
}

async fn start_dynamic(
    session: &mut Session,
    ctx: &mut Context,
    site: usize,
    index: usize,
    names: &[String],
) -> Result<bool> {
    let Some(dynamic) = ctx.snapshot.policy.dynamic_actions.get(index) else {
        return reject_dynamic(session, ctx, 503).await;
    };
    let Ok(raw) = std::str::from_utf8(session.req_header().raw_path()) else {
        return reject_dynamic(session, ctx, 400).await;
    };
    let mut deadline = Instant::now() + Duration::from_secs(300);
    match ctx.budget.as_ref().map(|budget| *budget.borrow()) {
        Some(Phase::Ingress(existing)) if Instant::now() >= existing => {
            return reject_dynamic(session, ctx, 408).await;
        }
        Some(Phase::Headers(existing)) => deadline = deadline.min(existing),
        _ => (),
    }
    if let Some(budget) = &ctx.budget {
        budget.send_replace(Phase::Headers(deadline));
    }
    let result = match tokio::time::timeout_at(
        deadline,
        super::dynamic::bridge::prepare(raw, dynamic, &ctx.snapshot, site),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => return reject_dynamic(session, ctx, 504).await,
    };
    let ready = match result {
        Ok(ready) => ready,
        Err(status) => {
            let code = match status {
                400 => "DYNAMIC_TARGET_INVALID",
                403 => "DYNAMIC_DENIED",
                502 => "DYNAMIC_DNS_FAILED",
                _ => "DYNAMIC_CAPACITY",
            };
            ctx.snapshot
                .logger
                .event(Level::Warn, "policy_error", &[("code", code)]);
            return reject_dynamic(session, ctx, status).await;
        }
    };
    if session.req_header().method == http::Method::CONNECT {
        return reject_dynamic(session, ctx, 400).await;
    }
    let handshake =
        match super::websocket::request(session.req_header(), session.is_http2(), names, true) {
            Ok(value) => value,
            Err(status) => return reject_dynamic(session, ctx, status).await,
        };
    ctx.websocket = handshake;
    ctx.connection_names = names.to_vec();
    ctx.addresses = Some(Arc::new(ready.addresses.clone()));
    ctx.dynamic = Some(ready);
    session.set_read_timeout(Some(Duration::from_secs(60)));
    session.set_write_timeout(Some(Duration::from_secs(60)));
    session.set_total_drain_timeout(Some(Duration::from_secs(60)));
    Ok(false)
}

async fn start_dav(
    session: &mut Session,
    ctx: &mut Context,
    site: usize,
    index: usize,
    secure: bool,
    authority: &str,
) -> Result<bool> {
    if let Some(status) = ctx.error_status {
        return reject(session, ctx, status).await;
    }
    let Some(state) = ctx.policy.as_ref() else {
        return reject(session, ctx, 403).await;
    };
    let mut deadline = crate::webdav::request::deadline();
    match ctx.budget.as_ref().map(|budget| *budget.borrow()) {
        Some(Phase::Ingress(existing)) if Instant::now() >= existing => {
            return reject(session, ctx, 408).await;
        }
        Some(Phase::Headers(existing)) => deadline = deadline.min(existing),
        _ => (),
    }
    if let Some(budget) = &ctx.budget {
        budget.send_replace(Phase::Headers(deadline));
    }
    session.set_read_timeout(Some(Duration::from_secs(60)));
    session.set_write_timeout(Some(Duration::from_secs(60)));
    let prepared = match crate::webdav::request::prepare(
        session,
        crate::webdav::request::RequestInput {
            snapshot: ctx.snapshot.clone(),
            site,
            index,
            state,
            scheme: ctx
                .forwarding
                .as_ref()
                .map_or(if secure { "https" } else { "http" }, |forwarding| {
                    forwarding.proto
                }),
            authority,
            deadline,
        },
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            ctx.snapshot
                .logger
                .event(Level::Warn, "dav_error", &[("code", error.code)]);
            return reject(session, ctx, error.status).await;
        }
    };
    let state = ctx.policy.take();
    let result = crate::webdav::request::send(
        session,
        prepared,
        ctx.snapshot.clone(),
        state.as_ref(),
        |_| ctx.begin_response(),
    )
    .await;
    ctx.policy = state;
    result?;
    Ok(true)
}

async fn authentication_failed(
    session: &mut Session,
    ctx: &mut Context,
    challenge: Option<String>,
) -> Result<bool> {
    let mut response = ResponseHeader::build(401, None)?;
    response.insert_header("content-length", "0")?;
    if let Some(challenge) = challenge {
        response.insert_header("www-authenticate", challenge)?;
    }
    ctx.begin_response()?;
    session
        .write_response_header(Box::new(response), true)
        .await?;
    Ok(true)
}

fn preserve_error_headers(ctx: &Context, response: &mut ResponseHeader) -> Result<()> {
    if ctx.error_status == Some(response.status.as_u16())
        && let Some(headers) = &ctx.error_headers
    {
        for name in [http::header::ALLOW, http::header::CONTENT_RANGE] {
            if let Some(value) = headers.get(&name) {
                response.insert_header(name, value.clone())?;
            }
        }
    }
    Ok(())
}

async fn policy_respond(
    session: &mut Session,
    ctx: &mut Context,
    status: u16,
    text: &str,
    content_type: &str,
) -> Result<bool> {
    if ctx
        .error_status
        .is_some_and(|original| status < 400 || (original == 503 && status != 503))
    {
        return reject(session, ctx, 503).await;
    }
    let (mut response, body) = match crate::http_policy::response::prepare(
        status,
        text,
        content_type,
        session.req_header().method == http::Method::HEAD,
    ) {
        Ok(value) => value,
        Err(_) => return reject(session, ctx, 503).await,
    };
    let transform = crate::body_transform::response::Prepared::select(
        session.req_header(),
        &response,
        ctx.policy.as_ref(),
        false,
    );
    preserve_error_headers(ctx, &mut response)?;
    if let Some(policy) = &ctx.policy
        && policy.apply_response(&mut response, &[]).is_err()
    {
        return reject(session, ctx, 503).await;
    }
    preserve_error_headers(ctx, &mut response)?;
    let mut transform = match transform.finish(ctx.snapshot.clone(), &mut response).await {
        Ok(transform) => transform,
        Err(_) => return reject(session, ctx, 503).await,
    };
    if ctx.error_status == Some(503) && response.status.as_u16() != 503 {
        return reject(session, ctx, 503).await;
    }
    let body = body || (transform.enabled() && session.req_header().method != http::Method::HEAD);
    ctx.begin_response()?;
    session
        .write_response_header(Box::new(response), !body)
        .await?;
    if body {
        if text.is_empty() {
            transform.write(session, None, true).await?;
        }
        for (index, chunk) in text.as_bytes().chunks(64 * 1024).enumerate() {
            transform
                .write_slice(session, chunk, (index + 1) * 64 * 1024 >= text.len())
                .await?;
        }
    }
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
        let snapshot = self.state.snapshot();
        let observation = RequestGuard::new(
            self.state.observability.clone(),
            snapshot.generation,
            &snapshot.configuration.server.observability.policy,
        );
        Context {
            snapshot,
            policy: None,
            transform: crate::body_transform::response::Transform::default(),
            error_status: None,
            error_headers: None,
            selected: None,
            dynamic: None,
            forwarding: None,
            connection_names: Vec::new(),
            attempt: 0,
            addresses: None,
            pool_lease: None,
            websocket: None,
            budget: None,
            idle: Duration::from_secs(60),
            response_started: false,
            observation,
        }
    }

    fn on_connection_reuse(
        &self,
        _: &mut Session,
        ctx: &mut Context,
        previous: Box<dyn Any + Send + Sync>,
    ) {
        if let Ok(channel) = previous.downcast::<PhaseChannel>() {
            ctx.observation.timeout_flag(channel.timeout.clone());
            ctx.budget = Some(channel.sender.clone());
        }
    }

    fn persist_connection_context(
        &self,
        _: &Session,
        ctx: &Context,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        Some(Box::new(ctx.idle))
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Context) -> Result<bool> {
        if ctx.budget.is_none()
            && let Some(channel) = session.req_header().extensions.get::<PhaseChannel>()
        {
            ctx.observation.timeout_flag(channel.timeout.clone());
            ctx.budget = Some(channel.sender.clone());
        }
        let request = session.req_header();
        ctx.observation.method(request.method.as_str());
        let size = request.raw_path().len()
            + request
                .headers
                .iter()
                .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
                .sum::<usize>();
        if size > 64 * 1024 {
            return reject(session, ctx, 431).await;
        }
        let authority = match authority(request, session.is_http2()) {
            Ok(value) => value,
            Err(status) => return reject(session, ctx, status).await,
        };
        let secure = session
            .digest()
            .is_some_and(|digest| digest.ssl_digest.is_some());
        if secure {
            let identity = session
                .digest()
                .and_then(|digest| digest.ssl_digest.as_ref())
                .and_then(|digest| digest.extension.get::<crate::tls::resolver::Handshake>());
            if identity.is_none_or(|identity| identity.server_name != authority.host) {
                return reject(session, ctx, 421).await;
            }
        }
        let mut path = match path(request) {
            Some(value) => value,
            None => return reject(session, ctx, 400).await,
        };
        let names = match connection_names(request) {
            Ok(value) => value,
            Err(status) => return reject(session, ctx, status).await,
        };
        let Some(site) = ctx.snapshot.router.site(&authority.host) else {
            return reject(session, ctx, 404).await;
        };
        if secure && ctx.snapshot.configuration.sites[site].tls.is_none() {
            return reject(session, ctx, 404).await;
        }
        if !secure && ctx.snapshot.configuration.server.redirect_https {
            let port = ctx
                .snapshot
                .configuration
                .server
                .redirect_port
                .ok_or_else(|| {
                    Error::explain(ErrorType::InternalError, "redirect port not prepared")
                })?;
            let suffix = if port == 443 {
                String::new()
            } else {
                format!(":{port}")
            };
            let target = std::str::from_utf8(request.raw_path()).map_err(|_| {
                Error::explain(ErrorType::HTTPStatus(400), "invalid redirect target")
            })?;
            let mut response = ResponseHeader::build(308, None)?;
            response.insert_header(
                "location",
                format!("https://{}{suffix}{target}", authority.host),
            )?;
            response.insert_header("content-length", "0")?;
            ctx.begin_response()?;
            session.set_keepalive(None);
            session
                .write_response_header(Box::new(response), true)
                .await?;
            return Ok(true);
        }
        let selected_route = if ctx.snapshot.configuration.sites[site].chain.is_some() {
            let peer = session
                .client_addr()
                .and_then(|a| a.as_inet())
                .map(|a| a.ip())
                .ok_or_else(|| {
                    Error::explain(ErrorType::InternalError, "missing client address")
                })?;
            let local_port = session
                .server_addr()
                .and_then(|a| a.as_inet())
                .map_or(80, |a| a.port());
            let forwarded = match forwarding(
                request,
                &authority,
                peer,
                local_port,
                secure,
                &ctx.snapshot.configuration.server,
            ) {
                Ok(value) => value,
                Err(status) => return reject(session, ctx, status).await,
            };
            let snapshot = ctx.snapshot.clone();
            let method = request.method.clone();
            let policy_path = request.uri.path().to_owned();
            let headers = request.headers.clone();
            let mut state = crate::http_policy::state::RequestPolicyState::new(
                forwarded.client_ip,
                snapshot.configuration.sites[site].domains[0].value.clone(),
                policy_path.clone(),
                names.clone(),
            );
            state.set_auth_headers(snapshot.policy.auth_headers[site].clone());
            if snapshot.policy.dav_sites[site] && state.set_entry_headers(&headers).is_err() {
                return reject(session, ctx, 503).await;
            }
            let initial_policy_path = policy_path;
            let (deadline, timeout_status) = ctx.budget.as_ref().map_or(
                (Instant::now() + Duration::from_secs(60), 408),
                |budget| match *budget.borrow() {
                    Phase::Ingress(deadline) => (deadline, 408),
                    Phase::Headers(deadline) => (deadline, 504),
                    Phase::Streaming => (Instant::now() + Duration::from_secs(60), 408),
                },
            );
            let result = super::policy::execute(
                super::policy::Input {
                    snapshot,
                    site,
                    method: method.clone(),
                    headers,
                    state,
                    client_ip: forwarded.client_ip,
                    error_status: None,
                },
                deadline,
            )
            .await;
            let super::policy::Execution {
                outcome,
                headers,
                state,
                error_status,
            } = match result {
                Ok(result) => result,
                Err(super::policy::Failure::Timeout) => {
                    return reject(session, ctx, timeout_status).await;
                }
                Err(super::policy::Failure::Unavailable) => return reject(session, ctx, 503).await,
            };
            ctx.error_status = error_status;
            if state.path() != initial_policy_path {
                path = state.path().to_owned();
            }
            super::policy::update_request(session, state.path(), &headers)?;
            ctx.forwarding = Some(forwarded);
            ctx.policy = Some(state);
            match outcome {
                crate::http_policy::execute::Outcome::AuthenticationFailed { challenge } => {
                    return authentication_failed(session, ctx, challenge).await;
                }
                crate::http_policy::execute::Outcome::Authorize(_) => {
                    return reject(session, ctx, 503).await;
                }
                crate::http_policy::execute::Outcome::Route(route) => Some((site, route)),
                crate::http_policy::execute::Outcome::Dynamic(index) => {
                    return start_dynamic(session, ctx, site, index, &names).await;
                }
                crate::http_policy::execute::Outcome::WebDav(index) => {
                    return start_dav(session, ctx, site, index, secure, &authority.original).await;
                }

                crate::http_policy::execute::Outcome::PathFailure { code, status, .. } => {
                    ctx.snapshot
                        .logger
                        .event(Level::Warn, "policy_error", &[("code", code)]);
                    return reject(
                        session,
                        ctx,
                        if ctx.error_status == Some(503) {
                            503
                        } else {
                            status
                        },
                    )
                    .await;
                }
                crate::http_policy::execute::Outcome::NotFound
                | crate::http_policy::execute::Outcome::NoMatch => {
                    return reject(session, ctx, ctx.error_status.unwrap_or(404)).await;
                }
                crate::http_policy::execute::Outcome::BudgetExceeded => {
                    ctx.snapshot.logger.event(
                        Level::Warn,
                        "policy_error",
                        &[("code", "POLICY_BUDGET_EXCEEDED")],
                    );
                    return reject(session, ctx, 503).await;
                }
                crate::http_policy::execute::Outcome::Respond {
                    status,
                    text,
                    content_type,
                } => {
                    return policy_respond(session, ctx, status, &text, &content_type).await;
                }
            }
        } else {
            ctx.snapshot.router.find(&authority.host, &path)
        };
        let Some((site, mut route)) = selected_route else {
            return reject(session, ctx, 404).await;
        };
        let mut selected_deadline: Option<Instant> = None;
        let deadline = loop {
            if let Some(observed) = ctx
                .snapshot
                .observation_routes
                .get(site)
                .and_then(|routes| routes.get(route))
            {
                ctx.observation.route(*observed);
            }
            let prepared = ctx
                .snapshot
                .routes
                .get(site)
                .and_then(|routes| routes.get(route))
                .ok_or_else(|| Error::explain(ErrorType::InternalError, "route not prepared"))?;
            ctx.idle = prepared.idle();
            let candidate_deadline = Instant::now() + prepared.request();
            let deadline = selected_deadline.map_or(candidate_deadline, |existing| {
                existing.min(candidate_deadline)
            });
            selected_deadline = Some(deadline);
            if let Some(budget) = &ctx.budget {
                budget.send_replace(Phase::Headers(deadline));
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
            let PreparedRoute::Static(static_route) = prepared else {
                break deadline;
            };
            let static_route = static_route.clone();
            let has_errors = ctx.policy.is_some()
                && ctx.snapshot.configuration.sites[site]
                    .chain
                    .as_ref()
                    .is_some_and(|c| c.errors.is_some());
            let error_page = ctx.error_status.is_some();
            let mut static_request = session.req_header().clone();
            if error_page {
                if static_request.method != http::Method::HEAD {
                    static_request.set_method(http::Method::GET);
                }
                for header in [
                    "if-none-match",
                    "if-modified-since",
                    "if-match",
                    "if-unmodified-since",
                    "if-range",
                    "range",
                    "accept-encoding",
                ] {
                    static_request.remove_header(header);
                }
            }
            let mut response = crate::static_files::prepare_with_policy(
                static_route,
                &static_request,
                &path,
                has_errors && !error_page,
                error_page,
                ctx.policy.as_ref(),
            )
            .await?;
            if !error_page
                && has_errors
                && let Some(local) = response.local_error
            {
                let status = match local {
                    crate::static_files::LocalError::Missing => 404,
                    crate::static_files::LocalError::Method => 405,
                    crate::static_files::LocalError::Unavailable => 503,
                };
                let state = ctx.policy.take().ok_or_else(|| {
                    Error::explain(ErrorType::InternalError, "missing policy state")
                })?;
                let client_ip = ctx
                    .forwarding
                    .as_ref()
                    .ok_or_else(|| Error::explain(ErrorType::InternalError, "missing forwarding"))?
                    .client_ip;
                let input = super::policy::Input {
                    snapshot: ctx.snapshot.clone(),
                    site,
                    method: session.req_header().method.clone(),
                    headers: session.req_header().headers.clone(),
                    state,
                    client_ip,
                    error_status: Some(status),
                };
                let result = super::policy::execute(input, deadline).await;
                let super::policy::Execution {
                    outcome,
                    headers,
                    state,
                    ..
                } = match result {
                    Ok(result) => result,
                    Err(super::policy::Failure::Timeout) => return reject(session, ctx, 504).await,
                    Err(super::policy::Failure::Unavailable) => {
                        return reject(session, ctx, 503).await;
                    }
                };
                path = state.path().to_owned();
                super::policy::update_request(session, state.path(), &headers)?;
                ctx.policy = Some(state);
                if !matches!(outcome, crate::http_policy::execute::Outcome::NoMatch) {
                    ctx.error_status = Some(status);
                    ctx.error_headers = Some(response.header.headers.clone());
                    match outcome {
                        crate::http_policy::execute::Outcome::AuthenticationFailed {
                            challenge,
                        } => return authentication_failed(session, ctx, challenge).await,
                        crate::http_policy::execute::Outcome::Authorize(_) => {
                            return reject(session, ctx, 503).await;
                        }
                        crate::http_policy::execute::Outcome::Dynamic(index) => {
                            return start_dynamic(session, ctx, site, index, &names).await;
                        }
                        crate::http_policy::execute::Outcome::WebDav(_) => {
                            return reject(session, ctx, status).await;
                        }
                        crate::http_policy::execute::Outcome::Route(next) => {
                            route = next;
                            continue;
                        }
                        crate::http_policy::execute::Outcome::Respond {
                            status,
                            text,
                            content_type,
                        } => {
                            return policy_respond(session, ctx, status, &text, &content_type)
                                .await;
                        }
                        crate::http_policy::execute::Outcome::BudgetExceeded => {
                            return reject(session, ctx, 503).await;
                        }
                        crate::http_policy::execute::Outcome::PathFailure {
                            status: failure,
                            ..
                        } => {
                            return reject(session, ctx, if status == 503 { 503 } else { failure })
                                .await;
                        }
                        crate::http_policy::execute::Outcome::NotFound => {
                            return reject(session, ctx, if status == 503 { 503 } else { 404 })
                                .await;
                        }
                        crate::http_policy::execute::Outcome::NoMatch => unreachable!(),
                    }
                }
            }
            if let Some(original) = ctx.error_status {
                let status = response.header.status.as_u16();
                if (200..300).contains(&status) {
                    response.header.set_status(original)?;
                    preserve_error_headers(ctx, &mut response.header)?;
                } else {
                    return reject(session, ctx, if original == 503 { 503 } else { status }).await;
                }
            }
            let transform = crate::body_transform::response::Prepared::select(
                session.req_header(),
                &response.header,
                ctx.policy.as_ref(),
                true,
            );
            if let Some(policy) = &ctx.policy {
                policy
                    .apply_response(&mut response.header, &[])
                    .map_err(|_| {
                        Error::explain(ErrorType::HTTPStatus(503), "policy header limit")
                    })?;
            }
            preserve_error_headers(ctx, &mut response.header)?;
            let body_budget = if transform.transforms_body() {
                let budget =
                    crate::body_transform::pipeline::Pipeline::acquire(ctx.snapshot.clone())?;
                response.retain_source(&budget)?;
                Some(budget)
            } else {
                None
            };
            let transform = transform
                .finish_budgeted(ctx.snapshot.clone(), &mut response.header, body_budget)
                .await?;
            if ctx.error_status == Some(503) && response.header.status.as_u16() != 503 {
                return reject(session, ctx, 503).await;
            }
            crate::static_files::send_transformed(session, response, transform, |_| {
                ctx.begin_response()
            })
            .await?;
            return Ok(true);
        };
        let prepared = ctx
            .snapshot
            .routes
            .get(site)
            .and_then(|routes| routes.get(route))
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "route not prepared"))?;
        let request = session.req_header();
        let allowed = ctx.snapshot.configuration.sites[site].routes[route]
            .parameters
            .websocket
            .as_ref()
            .is_none_or(|value| value.value);
        let handshake =
            match super::websocket::request(request, session.is_http2(), &names, allowed) {
                Ok(value) => value,
                Err(status) => return reject(session, ctx, status).await,
            };
        let peer = session
            .client_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.ip())
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "missing client address"))?;
        let local_port = session
            .server_addr()
            .and_then(|address| address.as_inet())
            .map_or(80, |address| address.port());
        let forwarding = if let Some(forwarded) = ctx.forwarding.take() {
            forwarded
        } else {
            match forwarding(
                request,
                &authority,
                peer,
                local_port,
                secure,
                &ctx.snapshot.configuration.server,
            ) {
                Ok(value) => value,
                Err(status) => return reject(session, ctx, status).await,
            }
        };
        let proxy = prepared
            .as_proxy()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "proxy route not prepared"))?;
        if let Some(policy) = &mut ctx.policy
            && let crate::config::model::Action::Proxy(upstream) =
                &ctx.snapshot.configuration.sites[site].routes[route].action
        {
            let host = if upstream.host.contains(':') {
                format!("[{}]", upstream.host)
            } else {
                upstream.host.clone()
            };
            policy.set_upstream_authority(format!("{host}:{}", upstream.port));
        }
        let error_503 = ctx.error_status == Some(503);
        let addresses = tokio::time::timeout_at(deadline, proxy.addresses())
            .await
            .map_err(|_| {
                Error::explain(
                    ErrorType::HTTPStatus(if error_503 { 503 } else { 504 }),
                    "upstream resolution deadline exceeded",
                )
                .into_up()
            })?
            .map_err(|_| {
                Error::explain(
                    ErrorType::HTTPStatus(if error_503 { 503 } else { 502 }),
                    "upstream resolution failed",
                )
                .into_up()
            })?;
        ctx.addresses = Some(addresses);
        ctx.websocket = handshake;
        ctx.selected = Some((site, route));
        ctx.forwarding = Some(forwarding);
        ctx.connection_names = names;
        Ok(false)
    }

    async fn upstream_peer(&self, _: &mut Session, ctx: &mut Context) -> Result<Box<HttpPeer>> {
        if let Some(ready) = &ctx.dynamic {
            let listeners = ctx
                .snapshot
                .configuration
                .server
                .http
                .iter()
                .chain(&ctx.snapshot.configuration.server.https)
                .map(|address| address.value)
                .collect();
            super::dynamic::dns::revalidate(ready.addresses.clone(), listeners)
                .await
                .map_err(|failure| {
                    let status = if matches!(failure, super::dynamic::dns::Failure::Denied) {
                        403
                    } else {
                        503
                    };
                    Error::explain(
                        ErrorType::HTTPStatus(status),
                        "DYNAMIC_ADDRESS_REVALIDATION_FAILED",
                    )
                    .into_up()
                })?;
            let address = *ready
                .addresses
                .get(ctx.attempt)
                .ok_or_else(|| Error::explain(ErrorType::ConnectError, "DYNAMIC_DNS_FAILED"))?;
            let (peer, lease) =
                super::dynamic::peer::make(ready, address, ctx.snapshot._dynamic_trust.clone())?;
            ctx.pool_lease = Some(lease);
            return Ok(Box::new(peer));
        }
        let (site, route) = ctx
            .selected
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "route not selected"))?;
        let prepared = ctx
            .snapshot
            .routes
            .get(site)
            .and_then(|routes| routes.get(route))
            .and_then(PreparedRoute::as_proxy)
            .ok_or_else(|| {
                Error::explain(ErrorType::ConnectError, "no upstream address").into_up()
            })?;
        let addresses = ctx.addresses.as_deref().ok_or_else(|| {
            Error::explain(ErrorType::ConnectError, "no upstream address").into_up()
        })?;
        let mut peer = prepared.peer(addresses, ctx.attempt).ok_or_else(|| {
            Error::explain(ErrorType::ConnectError, "no upstream address").into_up()
        })?;
        ctx.pool_lease = None;
        {
            let identity = prepared.pool_identity(addresses[ctx.attempt], &peer);
            let lease = super::pool_policy::registry()
                .acquire(identity)
                .map_err(|_| {
                    Error::explain(ErrorType::InternalError, "UPSTREAM_POOL_CAPACITY").into_up()
                })?;
            peer.group_key = lease.token;
            let birth_lease = Arc::clone(&lease);
            let connection_trust = prepared.trust.clone();
            peer.options.upstream_connection_birth_hook = Some(Arc::new(move |digest| {
                super::pool_policy::registry()
                    .birth(&digest, &birth_lease, connection_trust.clone())
                    .map_err(|_| {
                        Error::explain(ErrorType::InternalError, "UPSTREAM_POOL_CAPACITY").into_up()
                    })
            }));
            ctx.pool_lease = Some(lease);
        }
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        crate::body_transform::headers::suppress_upstream_validators(request, ctx.policy.as_ref());
        super::headers::request(
            request,
            &ctx.connection_names,
            ctx.forwarding.as_ref().ok_or_else(|| {
                Error::explain(ErrorType::InternalError, "forwarding policy not selected")
            })?,
        )?;
        if let Some((site, route)) = ctx.selected
            && let Some(host) = &ctx.snapshot.configuration.sites[site].routes[route].proxy_host
        {
            request.insert_header("host", host.as_str())?;
        }
        if let Some((site, route)) = ctx.selected
            && let Some(prepared) = ctx
                .snapshot
                .routes
                .get(site)
                .and_then(|routes| routes.get(route))
                .and_then(PreparedRoute::as_proxy)
            && prepared.protocol != crate::config::model::UpstreamProtocol::Http1
        {
            let grpc = request
                .headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|mime| mime.trim().starts_with("application/grpc"))
                });
            if grpc && session.req_header().version != http::Version::HTTP_2 {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(502),
                    "gRPC trailers require HTTP/2 downstream",
                )
                .into_up());
            }
            let authority = request.headers.get(http::header::HOST).ok_or_else(|| {
                Error::explain(ErrorType::InvalidHTTPHeader, "missing upstream authority")
            })?;
            let uri = http::uri::Builder::new()
                .scheme(if prepared.tls { "https" } else { "http" })
                .authority(authority.as_bytes())
                .path_and_query(
                    request
                        .uri
                        .path_and_query()
                        .map_or("/", http::uri::PathAndQuery::as_str),
                )
                .build()
                .map_err(|_| {
                    Error::explain(ErrorType::InvalidHTTPHeader, "invalid upstream authority")
                })?;
            request.set_uri(uri);
        }
        if let Some(handshake) = &ctx.websocket {
            super::websocket::request_headers(handshake, request)?;
        }
        for name in ["traceparent", "tracestate", "baggage"] {
            request.remove_header(name);
        }
        if let Some(ready) = &ctx.dynamic {
            super::dynamic::headers::request(
                request,
                ready,
                &ctx.snapshot.policy.auth_headers[ready.site],
            )?;
        }
        if ctx.policy.is_some() {
            crate::http_policy::headers::check(&request.headers).map_err(|_| {
                Error::explain(ErrorType::HTTPStatus(503), "policy request header limit")
            })?;
        }
        Ok(())
    }

    async fn connected_to_upstream(
        &self,
        _: &mut Session,
        _: bool,
        peer: &HttpPeer,
        _: RawFd,
        digest: Option<&Digest>,
        ctx: &mut Context,
    ) -> Result<()> {
        if let Some(lease) = &ctx.pool_lease {
            let socket = digest
                .and_then(|d| d.socket_digest.as_ref())
                .ok_or_else(|| {
                    Error::explain(ErrorType::InternalError, "UPSTREAM_POLICY_MISMATCH").into_up()
                })?;
            super::pool_policy::registry()
                .verify(socket, lease)
                .map_err(|_| {
                    Error::explain(ErrorType::InternalError, "UPSTREAM_POLICY_MISMATCH").into_up()
                })?;
        }
        super::upstream_tls::verify(peer, digest).map_err(|mut error| {
            error.set_retry(false);
            error
        })
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Context,
    ) -> Result<()> {
        if ctx.policy.is_some() {
            crate::http_policy::headers::check(&response.headers).map_err(|_| {
                Error::explain(ErrorType::HTTPStatus(503), "policy response header limit")
            })?;
        }
        if let Some(original) = ctx.error_status {
            let status = response.status.as_u16();
            if status < 400 || (original == 503 && status != 503) {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(503),
                    "invalid error chain response",
                ));
            }
            preserve_error_headers(ctx, response)?;
        }
        let transform = crate::body_transform::response::Prepared::select(
            session.req_header(),
            response,
            ctx.policy.as_ref(),
            false,
        );
        let original_status = response.status.as_u16();
        let original_mime = response
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let connection_names = if ctx.policy.is_some() {
            super::headers::response_connection_names(response)?
        } else {
            Vec::new()
        };
        if response.status.as_u16() == 101
            && let Some(handshake) = &ctx.websocket
        {
            super::websocket::response(handshake, response)?;
        } else {
            super::headers::response(response)?;
        }
        if let Some(policy) = &ctx.policy
            && (response.status.as_u16() == 101 || response.status.as_u16() >= 200)
        {
            policy
                .apply_response_with_metadata(
                    response,
                    &connection_names,
                    original_status,
                    original_mime.as_deref(),
                )
                .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "policy header failure"))?;
        }
        preserve_error_headers(ctx, response)?;
        if let Some(ready) = &ctx.dynamic {
            super::dynamic::headers::response(response, ready)?;
        }
        ctx.transform = transform.finish(ctx.snapshot.clone(), response).await?;
        if ctx.error_status == Some(503) && response.status.as_u16() != 503 {
            return Err(Error::explain(
                ErrorType::HTTPStatus(503),
                "invalid transformed error response",
            ));
        }
        if ctx.transform.enabled() {
            session.set_proxy_tasks_enabled(false);
        }
        if (response.status.as_u16() == 101 || response.status.as_u16() >= 200)
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

    async fn response_trailer_filter(
        &self,
        _: &mut Session,
        trailers: &mut http::HeaderMap,
        ctx: &mut Context,
    ) -> Result<Option<bytes::Bytes>> {
        if ctx.transform.enabled() {
            for name in [
                "etag",
                "last-modified",
                "content-md5",
                "content-length",
                "content-encoding",
            ] {
                trailers.remove(name);
            }
        }
        Ok(None)
    }

    fn response_body_streaming(&self, _: &Session, ctx: &Context) -> bool {
        ctx.transform.enabled()
    }

    fn response_body_push(
        &self,
        _: &mut Session,
        body: Option<bytes::Bytes>,
        eof: bool,
        ctx: &mut Context,
    ) -> Result<()> {
        ctx.transform.push(body, eof)
    }

    async fn response_body_next(
        &self,
        _: &mut Session,
        ctx: &mut Context,
    ) -> Result<pingora::proxy::ResponseBodyOutput> {
        ctx.transform.next().await
    }

    fn fail_to_connect(
        &self,
        _: &mut Session,
        _: &HttpPeer,
        ctx: &mut Context,
        mut error: Box<Error>,
    ) -> Box<Error> {
        ctx.attempt += 1;
        let retry = error.etype() != &ErrorType::InternalError
            && ctx
                .addresses
                .as_ref()
                .is_some_and(|addresses| ctx.attempt < addresses.len().min(8));
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

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Context) {
        let status = session
            .response_written()
            .map(|response| response.status.as_u16());
        let bytes = u64::try_from(session.as_ref().body_bytes_sent()).ok();
        ctx.observation.finish(
            status,
            bytes,
            if error.is_some() {
                Outcome::Error
            } else {
                Outcome::Completed
            },
        );
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
        ctx: &mut Context,
    ) -> FailToProxy {
        let status = match error.etype() {
            ErrorType::HTTPStatus(status) => *status,
            ErrorType::ReadTimedout if error.esource() == &pingora::ErrorSource::Downstream => 408,
            _ if error.esource() == &pingora::ErrorSource::Downstream => 400,
            ErrorType::ConnectTimedout | ErrorType::ReadTimedout | ErrorType::WriteTimedout => 504,
            _ => 502,
        };
        let status = if ctx.error_status == Some(503) {
            503
        } else {
            status
        };
        let status = if ctx.error_status.is_some() && (200..400).contains(&status) {
            503
        } else {
            status
        };
        session.set_keepalive(None);
        if session.response_written().is_none() && !ctx.response_started {
            ctx.start_response();
            let _ = session.respond_error(status).await;
        }
        FailToProxy {
            error_code: status,
            can_reuse_downstream: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dynamic_error_chain_preserves_unavailable_for_invalid_and_denied_targets() {
        use crate::config::prepare::build;
        use crate::runtime::state::HttpPreparer;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path =
            std::env::temp_dir().join(format!("rill-dynamic-error-{}.kdl", std::process::id()));
        std::fs::write(&path, "server { http \"127.0.0.1:8080\"; }\nsite \"app.test\" { chain { rule { respond 404; }; handle-errors { rule { dynamic-proxy prefix=\"/fetch/\" anonymous=#true; }; }; }; }").unwrap();
        let prepared = build(&path, &HttpPreparer).unwrap();
        std::fs::remove_file(path).unwrap();
        let handler = Handler {
            state: RuntimeState::new(prepared).unwrap(),
        };
        for (target, original, expected, expired) in [
            ("/fetch/not-a-url", Some(503), 503, false),
            ("/fetch/http%3A%2F%2F127.0.0.1%2F", Some(503), 503, false),
            ("/fetch/not-a-url", None, 400, false),
            ("/fetch/http%3A%2F%2F127.0.0.1%2F", None, 403, false),
            ("/fetch/not-a-url", Some(503), 503, true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (server, _) = listener.accept().await.unwrap();
            client
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: app.test\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let raw: pingora::protocols::l4::stream::Stream = server.into();
            let mut session = Session::new_h1(Box::new(raw));
            assert!(session.as_downstream_mut().read_request().await.unwrap());
            let mut ctx = handler.new_ctx();
            ctx.error_status = original;
            if expired {
                ctx.budget =
                    Some(watch::channel(Phase::Headers(Instant::now() - Duration::from_secs(1))).0);
            }
            assert!(
                start_dynamic(&mut session, &mut ctx, 0, 0, &[])
                    .await
                    .unwrap()
            );
            let mut response = [0; 1024];
            let count = client.read(&mut response).await.unwrap();
            let head = std::str::from_utf8(&response[..count]).unwrap();
            assert!(head.starts_with(&format!("HTTP/1.1 {expected} ")), "{head}");
        }
    }

    #[test]
    fn begin_response_rejects_expired_headers_before_writing() {
        let (budget, _) = watch::channel(Phase::Headers(Instant::now() - Duration::from_secs(1)));
        let error = begin_response(Some(&budget)).expect_err("expired budget accepted");
        assert!(matches!(error.etype(), ErrorType::HTTPStatus(504)));
    }

    #[test]
    fn begin_response_switches_headers_budget_to_streaming() {
        let (budget, receiver) =
            watch::channel(Phase::Headers(Instant::now() + Duration::from_secs(60)));
        begin_response(Some(&budget)).unwrap();
        assert!(matches!(*receiver.borrow(), Phase::Streaming));
    }
}

#[cfg(test)]
mod pool_collision_tests {
    use super::*;
    use crate::config::prepare::build;
    use crate::runtime::state::HttpPreparer;
    use pingora::connectors::http::Connector;
    use pingora::protocols::http::client::HttpSession;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_group_collision_reuses_connection_but_callback_rejects_before_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let target = listener.local_addr().expect("address");
        let backend = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut connection = h2::server::handshake(stream).await.expect("handshake");
            let (request, mut response) = connection
                .accept()
                .await
                .expect("first request")
                .expect("valid request");
            assert_eq!(request.uri().path(), "/first");
            response
                .send_response(
                    http::Response::builder()
                        .status(200)
                        .body(())
                        .expect("response"),
                    true,
                )
                .expect("send response");
            assert!(
                tokio::time::timeout(Duration::from_millis(500), connection.accept())
                    .await
                    .is_err(),
                "colliding request reached backend"
            );
        });
        let path = std::env::temp_dir().join(format!("rill-collision-{}.kdl", std::process::id()));
        std::fs::write(&path, format!("server {{ http \"127.0.0.1:8080\"; }}\nsite \"app.test\" {{ chain {{ rule {{ proxy \"http://{target}\" protocol=\"h2c\"; }}; }}; }}")).expect("config");
        let prepared = build(&path, &HttpPreparer).expect("prepare");
        std::fs::remove_file(&path).expect("remove config");
        let handler = Handler {
            state: RuntimeState::new(prepared).expect("runtime"),
        };
        let local_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener");
        let connecting = tokio::net::TcpStream::connect(local_listener.local_addr().expect("addr"));
        let (client, accepted) = tokio::join!(connecting, local_listener.accept());
        let _client = client.expect("client");
        let raw: pingora::protocols::l4::stream::Stream = accepted.expect("accept").0.into();
        let mut downstream = Session::new_h1(Box::new(raw));
        let mut a = handler.new_ctx();
        a.selected = Some((0, 0));
        a.addresses = Some(Arc::new(vec![target]));
        let peer_a = handler
            .upstream_peer(&mut downstream, &mut a)
            .await
            .expect("peer A");
        let connector = Connector::new(None);
        let (session, reused) = connector
            .get_http_session(&*peer_a)
            .await
            .expect("connect A");
        assert!(!reused);
        let HttpSession::H2(mut session) = session else {
            panic!("h2 required")
        };
        handler
            .connected_to_upstream(
                &mut downstream,
                false,
                &peer_a,
                session.fd(),
                session.digest(),
                &mut a,
            )
            .await
            .expect("guard A");
        let mut request = RequestHeader::build("GET", b"/first", None).expect("request");
        request.insert_header("host", "app.test").expect("host");
        session
            .write_request_header(Box::new(request), true)
            .expect("send A");
        session.read_response_header().await.expect("response A");
        let mut b = handler.new_ctx();
        let prepared = b.snapshot.routes[0][0].as_proxy().expect("prepared peer");
        let mut identity = prepared.pool_identity(target, &peer_a);
        identity.dynamic = true;
        identity.protocol = crate::config::model::UpstreamProtocol::Http1;
        identity.tls_name = Some("dynamic.example".to_owned());
        b.pool_lease = Some(
            super::super::pool_policy::registry()
                .acquire(identity)
                .expect("lease B"),
        );
        let mut peer_b = (*peer_a).clone();
        peer_b.group_key = peer_a.group_key;
        let (collided, reused) = connector.get_http_session(&peer_b).await.expect("reuse");
        assert!(reused);
        let HttpSession::H2(collided) = collided else {
            panic!("h2 required")
        };
        let error = handler
            .connected_to_upstream(
                &mut downstream,
                true,
                &peer_b,
                collided.fd(),
                collided.digest(),
                &mut b,
            )
            .await
            .expect_err("must reject collision");
        assert!(error.to_string().contains("UPSTREAM_POLICY_MISMATCH"));
        backend.await.expect("backend");
    }
}
