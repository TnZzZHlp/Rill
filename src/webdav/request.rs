use super::{
    authorization::Authorization,
    limits::{self, CancelOnDrop, Cancellation},
    methods::{self, Message},
    model::{DavError, Result},
    namespace, put,
    request_model::{DavRequest, single},
};
use crate::{http_policy::state::RequestPolicyState, runtime::state::RuntimeSnapshot};
use pingora::{http::ResponseHeader, proxy::Session};
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::Instant};

pub enum Prepared {
    Read(crate::static_files::PreparedResponse),
    Stream {
        header: ResponseHeader,
        receiver: mpsc::Receiver<Message>,
        _cancel: CancelOnDrop,
    },
}
fn error() -> DavError {
    DavError::new(500, "DAV_WORKER")
}
async fn within<T>(deadline: Instant, future: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| DavError::new(504, "DAV_TIMEOUT"))
}
async fn interim(session: &mut Session) -> Result<()> {
    if let Some(expect) = single(&session.req_header().headers, "expect")? {
        if !expect.eq_ignore_ascii_case("100-continue") {
            return Err(DavError::new(417, "DAV_EXPECT"));
        }
        session
            .write_response_header(
                Box::new(ResponseHeader::build(100, None).map_err(|_| error())?),
                false,
            )
            .await
            .map_err(|_| DavError::new(400, "DAV_BODY"))?;
    }
    Ok(())
}
fn response_header(status: u16, headers: Vec<(&'static str, String)>) -> Result<ResponseHeader> {
    let mut header = ResponseHeader::build(status, None).map_err(|_| error())?;
    header
        .insert_header(
            "date",
            httpdate::fmt_http_date(std::time::SystemTime::now()),
        )
        .map_err(|_| error())?;
    for (name, value) in headers {
        header.insert_header(name, value).map_err(|_| error())?;
    }
    Ok(header)
}
pub struct RequestInput<'a> {
    pub snapshot: Arc<RuntimeSnapshot>,
    pub site: usize,
    pub index: usize,
    pub state: &'a RequestPolicyState,
    pub scheme: &'static str,
    pub authority: &'a str,
    pub deadline: Instant,
}
pub async fn prepare(session: &mut Session, input: RequestInput<'_>) -> Result<Prepared> {
    let RequestInput {
        snapshot,
        site,
        index,
        state,
        scheme,
        authority,
        deadline,
    } = input;
    let storage = snapshot
        .policy
        .dav_handles
        .get(index)
        .cloned()
        .ok_or_else(error)?;
    let method = session.req_header().method.clone();
    let headers = session.req_header().headers.clone();
    let length = single(&headers, "content-length")?
        .map(|s| s.parse::<u64>().map_err(|_| DavError::invalid()))
        .transpose()?;
    let put = method.as_str() == "PUT";
    let xml = matches!(method.as_str(), "PROPFIND" | "PROPPATCH" | "LOCK" | "MKCOL");
    if length.is_some_and(|n| {
        n > if put {
            limits::PUT_FILE
        } else if xml {
            super::xml::XML_LIMIT as u64
        } else {
            u64::MAX
        }
    }) {
        return Err(DavError::new(413, "DAV_LIMIT"));
    }
    if single(&headers, "expect")?.is_some_and(|s| !s.eq_ignore_ascii_case("100-continue")) {
        return Err(DavError::new(417, "DAV_EXPECT"));
    }
    let put_permit = if put {
        Some(
            limits::puts()
                .try_acquire_owned()
                .map_err(|_| DavError::limit())?,
        )
    } else {
        None
    };
    let path = session.req_header().uri.path().to_owned();
    let authorization = Authorization::new(
        snapshot.clone(),
        site,
        index,
        method.clone(),
        state,
        scheme,
        authority,
    )?;
    let request_memory = super::budget::Memory::acquire(
        headers
            .capacity()
            .checked_mul(128)
            .and_then(|n| {
                n.checked_add(
                    headers
                        .iter()
                        .map(|(name, value)| name.as_str().len() + value.len())
                        .sum::<usize>()
                        .checked_mul(64)?,
                )
            })
            .and_then(|n| n.checked_add(path.len() * 8 + 4096))
            .ok_or_else(DavError::limit)?,
    )?;
    let permit = crate::config::prepare::static_capacity()
        .try_acquire_owned()
        .map_err(|_| DavError::limit())?;
    let guard = within(deadline, storage.namespace.clone().write_owned()).await?;
    let request_storage = storage.clone();
    let request = within(
        deadline,
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let namespace = Arc::new(namespace::NamespaceSession::new(request_storage, guard)?);
            DavRequest::prepare(
                method,
                &path,
                headers,
                authorization,
                request_memory,
                namespace,
            )
        }),
    )
    .await?
    .map_err(|_| error())??;
    let cancellation = Cancellation::default();
    let cancel = CancelOnDrop(cancellation.clone());
    if matches!(request.method.as_str(), "GET" | "HEAD") {
        let permit = crate::config::prepare::static_capacity()
            .try_acquire_owned()
            .map_err(|_| DavError::limit())?;
        let opened_storage = storage.clone();
        let request = within(
            deadline,
            tokio::task::spawn_blocking(move || -> Result<_> {
                let _permit = permit;
                let mut request = request;
                request.headers.remove("if-none-match");
                request.check(&opened_storage)?;
                let file = namespace::open(&request.namespace, &request.path)?;
                if file.metadata()?.is_dir() {
                    return Err(DavError::new(405, "DAV_COLLECTION"));
                }
                Ok(request)
            }),
        )
        .await?
        .map_err(|_| error())??;
        let response = within(
            deadline,
            crate::static_files::prepare_with_policy(
                storage.read.clone(),
                session.req_header(),
                &request.path.href(),
                false,
                false,
                Some(state),
            ),
        )
        .await?
        .map_err(|_| error())?;
        drop(request);
        return Ok(Prepared::Read(response));
    }
    if put {
        let permit = put_permit.ok_or_else(error)?;
        let worker_cancel = cancellation.clone();
        let expires = deadline.into_std();
        let mut upload = within(
            deadline,
            tokio::task::spawn_blocking(move || {
                put::Upload::create(storage, request, permit, &worker_cancel, expires)
            }),
        )
        .await?
        .map_err(|_| error())??;
        within(deadline, interim(session)).await??;
        loop {
            let Some(bytes) = within(deadline, session.read_request_body())
                .await?
                .map_err(|_| DavError::new(400, "DAV_BODY"))?
            else {
                break;
            };
            if bytes.len() > limits::BLOCK {
                return Err(DavError::new(413, "DAV_LIMIT"));
            }
            let owned = super::budget::Buffer::copy_from(&bytes)?;
            drop(bytes);
            let worker_cancel = cancellation.clone();
            upload = within(
                deadline,
                tokio::task::spawn_blocking(move || upload.write(owned, &worker_cancel, expires)),
            )
            .await?
            .map_err(|_| error())??;
        }
        let worker_cancel = cancellation.clone();
        let status = within(
            deadline,
            tokio::task::spawn_blocking(move || upload.finish(&worker_cancel, expires)),
        )
        .await
        .map_err(|error| {
            if cancellation.committing() {
                DavError::new(504, "DAV_COMMIT_RESULT_UNKNOWN")
            } else {
                error
            }
        })?
        .map_err(|_| error())??;
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(Message::End).map_err(|_| error())?;
        return Ok(Prepared::Stream {
            header: response_header(status, vec![("content-length", "0".into())])?,
            receiver,
            _cancel: cancel,
        });
    }
    let permit = crate::config::prepare::static_capacity()
        .try_acquire_owned()
        .map_err(|_| DavError::limit())?;
    let preflight_storage = storage.clone();
    let request = within(
        deadline,
        tokio::task::spawn_blocking(move || -> Result<_> {
            let _permit = permit;
            request.check(&preflight_storage)?;
            Ok(request)
        }),
    )
    .await?
    .map_err(|_| error())??;
    let mut body = super::budget::Buffer::default();
    if xml {
        within(deadline, interim(session)).await??;
        loop {
            let Some(bytes) = within(deadline, session.read_request_body())
                .await?
                .map_err(|_| DavError::new(400, "DAV_BODY"))?
            else {
                break;
            };
            body.extend(&bytes, super::xml::XML_LIMIT)?;
        }
    } else if length.is_some_and(|n| n > 0)
        || session
            .req_header()
            .headers
            .contains_key("transfer-encoding")
    {
        session.set_keepalive(None);
    }
    let permit = crate::config::prepare::static_capacity()
        .try_acquire_owned()
        .map_err(|_| DavError::limit())?;
    let (sender, mut receiver) = mpsc::channel(1);
    let worker_cancel = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Err(error) = methods::execute(
            request,
            body.as_ref(),
            &sender,
            &worker_cancel,
            deadline.into_std(),
        ) {
            let _ = sender.blocking_send(Message::Error(error));
        }
    });
    let first = within(deadline, receiver.recv()).await?.ok_or_else(error)?;
    let (status, headers) = match first {
        Message::Header(status, headers) => (status, headers),
        Message::Error(error) => return Err(error),
        _ => return Err(error()),
    };
    Ok(Prepared::Stream {
        header: response_header(status, headers)?,
        receiver,
        _cancel: cancel,
    })
}
pub async fn send<F>(
    session: &mut Session,
    prepared: Prepared,
    snapshot: Arc<RuntimeSnapshot>,
    state: Option<&RequestPolicyState>,
    on_headers: F,
) -> pingora::Result<()>
where
    F: FnOnce(&mut ResponseHeader) -> pingora::Result<()> + Send,
{
    match prepared {
        Prepared::Read(mut response) => {
            let transform = crate::body_transform::response::Prepared::select(
                session.req_header(),
                &response.header,
                state,
                true,
            );
            if let Some(state) = state {
                state
                    .apply_response(&mut response.header, &[])
                    .map_err(|_| {
                        pingora::Error::explain(
                            pingora::ErrorType::HTTPStatus(503),
                            "DAV response policy",
                        )
                    })?;
            }
            let budget = if transform.transforms_body() {
                let budget = crate::body_transform::pipeline::Pipeline::acquire(snapshot.clone())?;
                response.retain_source(&budget)?;
                Some(budget)
            } else {
                None
            };
            let transform = transform
                .finish_budgeted(snapshot, &mut response.header, budget)
                .await?;
            crate::static_files::send_transformed(session, response, transform, on_headers).await
        }
        Prepared::Stream {
            mut header,
            mut receiver,
            _cancel,
        } => {
            if let Some(state) = state {
                state.apply_response(&mut header, &[]).map_err(|_| {
                    pingora::Error::explain(
                        pingora::ErrorType::HTTPStatus(503),
                        "DAV response policy",
                    )
                })?;
            }
            on_headers(&mut header)?;
            session
                .write_response_header(Box::new(header), false)
                .await?;
            while let Some(message) = receiver.recv().await {
                match message {
                    Message::Data(bytes) => {
                        session
                            .write_response_body(Some(bytes::Bytes::from_owner(bytes)), false)
                            .await?
                    }
                    Message::End => {
                        session.write_response_body(None, true).await?;
                        return Ok(());
                    }
                    Message::Error(error) => {
                        return Err(pingora::Error::explain(
                            pingora::ErrorType::HTTPStatus(error.status),
                            error.code,
                        ));
                    }
                    Message::Header(_, _) => {
                        return Err(pingora::Error::explain(
                            pingora::ErrorType::InternalError,
                            "DAV response state",
                        ));
                    }
                }
            }
            Err(pingora::Error::explain(
                pingora::ErrorType::InternalError,
                "DAV response cancelled",
            ))
        }
    }
}
pub fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(300)
}
