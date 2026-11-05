use crate::{
    http_policy::{
        execute::{self, MatchContext, Outcome, PathErrorClass},
        state::RequestPolicyState,
    },
    runtime::state::RuntimeSnapshot,
};
use http::{HeaderMap, Method};
use std::{net::IpAddr, sync::Arc};
use tokio::time::Instant;

pub struct Execution {
    pub outcome: Outcome,
    pub headers: HeaderMap,
    pub state: RequestPolicyState,
    pub error_status: Option<u16>,
}
pub struct Input {
    pub snapshot: Arc<RuntimeSnapshot>,
    pub site: usize,
    pub method: Method,
    pub headers: HeaderMap,
    pub state: RequestPolicyState,
    pub client_ip: IpAddr,
    pub error_status: Option<u16>,
}
#[derive(Debug)]
pub enum Failure {
    Timeout,
    Unavailable,
}
fn run(mut input: Input, cursor: Option<execute::Cursor>) -> Execution {
    let Some(chain) = input.snapshot.configuration.sites[input.site]
        .chain
        .as_ref()
    else {
        return Execution {
            outcome: Outcome::BudgetExceeded,
            headers: input.headers,
            state: input.state,
            error_status: None,
        };
    };
    let mut entered = input.state.error_entered;
    let mut error_status = input.error_status;
    let mut ctx = MatchContext {
        method: &input.method,
        path: input.state.path().to_owned(),
        headers: &mut input.headers,
        steps_left: input.state.steps_left,
        reentries: input.state.reentries,
        error_status: None,
        state: Some(&mut input.state),
        client_ip: input.client_ip,
        captures: [None; 9],
    };
    let mut outcome = if let Some(status) = error_status {
        if entered && cursor.is_none() {
            Outcome::BudgetExceeded
        } else {
            entered = true;
            execute::execute_errors(chain, &input.snapshot.policy, &mut ctx, status, cursor)
                .unwrap_or(Outcome::NoMatch)
        }
    } else {
        execute::execute_resume(chain, &input.snapshot.policy, &mut ctx, cursor)
    };
    if error_status.is_none() && !entered {
        let eligible = match &outcome {
            Outcome::NotFound => Some(404),
            Outcome::PathFailure {
                class: PathErrorClass::LocalNotFound | PathErrorClass::LocalUnavailable,
                status,
                ..
            } => Some(*status),
            _ => None,
        };
        if let Some(status) = eligible
            && chain.errors.is_some()
        {
            entered = true;
            let handled =
                execute::execute_errors(chain, &input.snapshot.policy, &mut ctx, status, None)
                    .unwrap_or(Outcome::NoMatch);
            if !matches!(handled, Outcome::NoMatch) {
                outcome = handled;
            }
            error_status = Some(status);
        }
    }
    let steps_left = ctx.steps_left;
    let reentries = ctx.reentries;
    drop(ctx);
    input.state.steps_left = steps_left;
    input.state.reentries = reentries;
    input.state.error_entered = entered;
    Execution {
        outcome,
        headers: input.headers,
        state: input.state,
        error_status,
    }
}
pub async fn execute(mut input: Input, deadline: Instant) -> Result<Execution, Failure> {
    let mut cursor = None;
    loop {
        let snapshot = input.snapshot.clone();
        let site = input.site;
        let method = input.method.clone();
        let client_ip = input.client_ip;
        let filesystem = snapshot
            .policy
            .filesystem_sites
            .get(site)
            .copied()
            .unwrap_or(false);
        let result = if filesystem {
            let permit = tokio::time::timeout_at(
                deadline,
                crate::config::prepare::static_capacity().acquire_owned(),
            )
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Unavailable)?;
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                run(input, cursor)
            });
            tokio::time::timeout_at(deadline, task)
                .await
                .map_err(|_| Failure::Timeout)?
                .map_err(|_| Failure::Unavailable)?
        } else {
            run(input, cursor)
        };
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        let Execution {
            outcome,
            mut headers,
            mut state,
            error_status,
        } = result;
        let Outcome::Authorize(next) = outcome else {
            if state.authenticated() {
                headers.remove(http::header::AUTHORIZATION);
                headers.remove(http::header::PROXY_AUTHORIZATION);
                for name in snapshot.policy.auth_headers[site].iter() {
                    headers.remove(name);
                }
            }
            return Ok(Execution {
                outcome,
                headers,
                state,
                error_status,
            });
        };
        let Some(auth_action) = snapshot.configuration.sites[site]
            .chain
            .as_ref()
            .and_then(|chain| execute::suspended_action(chain, &next))
        else {
            return Err(Failure::Unavailable);
        };
        // The immutable snapshot owns the action for the entire request.
        let action_id = std::ptr::from_ref(auth_action) as usize;
        if state.authenticated_action(action_id) {
            cursor = Some(*next);
            input = Input {
                snapshot,
                site,
                method,
                headers,
                state,
                client_ip,
                error_status,
            };
            continue;
        }
        let verified = tokio::time::timeout_at(
            deadline,
            crate::auth::verify::authorize(snapshot.clone(), site, &next, &headers),
        )
        .await
        .map_err(|_| Failure::Timeout)?;
        match verified {
            crate::auth::verify::Result::Success => {
                headers.remove(http::header::AUTHORIZATION);
                headers.remove(http::header::PROXY_AUTHORIZATION);
                if let Some(crate::http_policy::model::Action::StaticToken(token)) =
                    snapshot.configuration.sites[site]
                        .chain
                        .as_ref()
                        .and_then(|chain| execute::suspended_action(chain, &next))
                {
                    headers.remove(&token.header);
                }
                state.mark_authenticated(action_id);
            }
            crate::auth::verify::Result::Invalid(challenge) => {
                return Ok(Execution {
                    outcome: Outcome::AuthenticationFailed { challenge },
                    headers,
                    state,
                    error_status,
                });
            }
            crate::auth::verify::Result::Capacity => {
                return Ok(Execution {
                    outcome: Outcome::PathFailure {
                        class: PathErrorClass::Capacity,
                        code: "AUTH_CAPACITY",
                        status: 503,
                    },
                    headers,
                    state,
                    error_status,
                });
            }
            crate::auth::verify::Result::Timeout => {
                return Ok(Execution {
                    outcome: Outcome::PathFailure {
                        class: PathErrorClass::Capacity,
                        code: "AUTH_TIMEOUT",
                        status: 503,
                    },
                    headers,
                    state,
                    error_status,
                });
            }
        }
        cursor = Some(*next);
        input = Input {
            snapshot,
            site,
            method,
            headers,
            state,
            client_ip,
            error_status,
        };
    }
}

pub fn update_request(
    session: &mut pingora::proxy::Session,
    path: &str,
    headers: &HeaderMap,
) -> pingora::Result<()> {
    if session.req_header().uri.path() != path {
        let query = session
            .req_header()
            .raw_path()
            .iter()
            .position(|b| *b == b'?')
            .map(|i| session.req_header().raw_path()[i..].to_vec());
        let mut target = path.as_bytes().to_vec();
        if let Some(query) = query {
            target.extend_from_slice(&query);
        }
        session.req_header_mut().set_raw_path(&target)?;
    }
    let request = session.req_header_mut();
    let names: Vec<_> = request.headers.keys().cloned().collect();
    for name in names {
        request.remove_header(&name);
    }
    for (name, value) in headers {
        request.append_header(name, value)?;
    }
    Ok(())
}
