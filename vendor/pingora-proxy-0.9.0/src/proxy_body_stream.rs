// Copyright 2026 Cloudflare, Inc.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy at http://www.apache.org/licenses/LICENSE-2.0

use super::*;

impl<SV, C> HttpProxy<SV, C>
where
    C: custom::Connector,
{
    async fn drain_response_body(
        &self,
        session: &mut Session,
        ctx: &mut SV::CTX,
        finish: bool,
    ) -> Result<bool>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        loop {
            match self.inner.response_body_next(session, ctx).await? {
                ResponseBodyOutput::Chunk(data) => {
                    if data.len() > 64 * 1024 {
                        return Error::e_explain(InternalError, "streaming response chunk limit");
                    }
                    session.write_response_body(Some(data), false).await?;
                }
                ResponseBodyOutput::InputDone => return Ok(false),
                ResponseBodyOutput::Finished => {
                    if finish {
                        session.write_response_body(None, true).await?;
                    }
                    return Ok(true);
                }
            }
        }
    }

    pub(crate) async fn write_response_tasks_streaming(
        &self,
        session: &mut Session,
        tasks: Vec<HttpTask>,
        ctx: &mut SV::CTX,
    ) -> Result<bool>
    where
        SV: ProxyHttp + Send + Sync,
        SV::CTX: Send + Sync,
    {
        if !self.inner.response_body_streaming(session, ctx) {
            return session.write_response_tasks(tasks).await;
        }
        let mut done = false;
        for task in tasks {
            if !self.inner.response_body_streaming(session, ctx) {
                done |= session.write_response_tasks(vec![task]).await?;
                continue;
            }
            match task {
                HttpTask::Header(header, eos) => {
                    session
                        .write_response_tasks(vec![HttpTask::Header(header, false)])
                        .await?;
                    if eos {
                        self.inner.response_body_push(session, None, true, ctx)?;
                        done |= self.drain_response_body(session, ctx, true).await?;
                    }
                }
                HttpTask::Body(body, eos) => {
                    self.inner.response_body_push(session, body, eos, ctx)?;
                    done |= self.drain_response_body(session, ctx, true).await?;
                }
                HttpTask::Done => {
                    self.inner.response_body_push(session, None, true, ctx)?;
                    done |= self.drain_response_body(session, ctx, true).await?;
                }
                HttpTask::Trailer(trailers) => {
                    self.inner.response_body_push(session, None, true, ctx)?;
                    done |= self.drain_response_body(session, ctx, false).await?;
                    session
                        .write_response_tasks(vec![HttpTask::Trailer(trailers)])
                        .await?;
                }
                other => {
                    done |= session.write_response_tasks(vec![other]).await?;
                }
            }
        }
        Ok(done)
    }
}
