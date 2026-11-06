use crate::{
    body_transform::{
        budget::{Buffer, ResponseBudget},
        headers::{self, Selection},
        pipeline::{Event, Pipeline},
    },
    http_policy::state::RequestPolicyState,
    runtime::state::RuntimeSnapshot,
};
use bytes::Bytes;
use pingora::{
    Result,
    http::{RequestHeader, ResponseHeader},
    proxy::Session,
};
use std::sync::Arc;

#[derive(Default)]
pub struct Transform {
    pipeline: Option<Pipeline>,
    discard: bool,
    discard_eof: bool,
}

pub struct Prepared {
    selection: Option<Selection>,
    unacceptable: bool,
    head: bool,
    policy_scope: bool,
}
impl Prepared {
    pub fn select(
        request: &RequestHeader,
        response: &ResponseHeader,
        policy: Option<&RequestPolicyState>,
        sidecar: bool,
    ) -> Self {
        let selection = headers::select(request, response, policy, sidecar);
        let unacceptable = selection.is_err();
        Self {
            selection: selection.ok().flatten(),
            unacceptable,
            head: request.method == http::Method::HEAD,
            policy_scope: policy.is_some(),
        }
    }
    pub(crate) fn transforms_body(&self) -> bool {
        !self.head && self.selection.as_ref().is_some_and(Selection::active)
    }

    pub async fn finish(
        self,
        snapshot: Arc<RuntimeSnapshot>,
        response: &mut ResponseHeader,
    ) -> Result<Transform> {
        self.finish_budgeted(snapshot, response, None).await
    }

    pub(crate) async fn finish_budgeted(
        self,
        snapshot: Arc<RuntimeSnapshot>,
        response: &mut ResponseHeader,
        budget: Option<Arc<ResponseBudget>>,
    ) -> Result<Transform> {
        let mut transform = Transform::default();
        let mut pipeline_selection = None;
        if self.unacceptable {
            response.set_status(406)?;
            for name in [
                "content-encoding",
                "transfer-encoding",
                "etag",
                "last-modified",
                "content-md5",
            ] {
                response.remove_header(name);
            }
            response.insert_header("content-length", "0")?;
            headers::vary(response)?;
            transform.discard = true;
        } else if let Some(selection) = self.selection {
            if selection.vary {
                headers::vary(response)?;
            }
            if selection.active() {
                headers::finalize(response, &selection, self.head)?;
                if !self.head {
                    pipeline_selection = Some(selection);
                }
            }
        }
        if self.policy_scope {
            crate::http_policy::headers::check(&response.headers).map_err(|_| {
                pingora::Error::explain(
                    pingora::ErrorType::HTTPStatus(503),
                    "policy final response header limit",
                )
            })?;
        }
        if let Some(selection) = pipeline_selection {
            transform.pipeline = Some(if let Some(budget) = budget {
                Pipeline::start_budgeted(snapshot, selection, budget).await?
            } else {
                Pipeline::start(snapshot, selection).await?
            });
        }
        Ok(transform)
    }
}

impl Transform {
    pub fn enabled(&self) -> bool {
        self.pipeline.is_some() || self.discard
    }
    pub fn push(&mut self, data: Option<Bytes>, eof: bool) -> Result<()> {
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.push(data, eof)
        } else {
            self.discard_eof = eof;
            Ok(())
        }
    }
    pub async fn next(&mut self) -> Result<pingora::proxy::ResponseBodyOutput> {
        if let Some(pipeline) = &mut self.pipeline {
            match pipeline.next().await? {
                Event::Chunk(bytes) => Ok(pingora::proxy::ResponseBodyOutput::Chunk(bytes)),
                Event::InputDone => Ok(pingora::proxy::ResponseBodyOutput::InputDone),
                Event::Finished => {
                    self.pipeline = None;
                    Ok(pingora::proxy::ResponseBodyOutput::Finished)
                }
                Event::Ready => Err(pingora::Error::explain(
                    pingora::ErrorType::InternalError,
                    "unexpected body readiness",
                )),
            }
        } else if self.discard_eof {
            self.discard = false;
            Ok(pingora::proxy::ResponseBodyOutput::Finished)
        } else {
            Ok(pingora::proxy::ResponseBodyOutput::InputDone)
        }
    }
    pub async fn write(
        &mut self,
        session: &mut Session,
        bytes: Option<Bytes>,
        eof: bool,
    ) -> Result<()> {
        if !self.enabled() {
            return session.write_response_body(bytes, eof).await;
        }
        self.push(bytes, eof)?;
        self.drain(session).await
    }

    pub(crate) async fn write_slice(
        &mut self,
        session: &mut Session,
        bytes: &[u8],
        eof: bool,
    ) -> Result<()> {
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.push_slice(bytes, eof)?;
            self.drain(session).await
        } else {
            self.write(session, Some(Bytes::copy_from_slice(bytes)), eof)
                .await
        }
    }

    pub(crate) fn input_buffer(&self, capacity: usize) -> Result<Option<Buffer>> {
        self.pipeline
            .as_ref()
            .map(|pipeline| pipeline.input_buffer(capacity))
            .transpose()
    }

    pub(crate) async fn write_buffer(
        &mut self,
        session: &mut Session,
        buffer: Buffer,
        eof: bool,
    ) -> Result<()> {
        self.pipeline
            .as_mut()
            .ok_or_else(|| super::pipeline::proxy_error(super::limits::Failure::Finished))?
            .push_buffer(buffer, eof)?;
        self.drain(session).await
    }

    async fn drain(&mut self, session: &mut Session) -> Result<()> {
        loop {
            match self.next().await? {
                pingora::proxy::ResponseBodyOutput::Chunk(bytes) => {
                    session.write_response_body(Some(bytes), false).await?
                }
                pingora::proxy::ResponseBodyOutput::InputDone => return Ok(()),
                pingora::proxy::ResponseBodyOutput::Finished => {
                    return session.write_response_body(None, true).await;
                }
            }
        }
    }
}
