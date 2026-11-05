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
fn run(mut input: Input) -> Execution {
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
        if entered {
            Outcome::BudgetExceeded
        } else {
            entered = true;
            execute::execute_errors(chain, &input.snapshot.policy, &mut ctx, status)
                .unwrap_or(Outcome::NoMatch)
        }
    } else {
        execute::execute(chain, &input.snapshot.policy, &mut ctx)
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
            let handled = execute::execute_errors(chain, &input.snapshot.policy, &mut ctx, status)
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
pub async fn execute(input: Input, deadline: Instant) -> Result<Execution, Failure> {
    if !input
        .snapshot
        .policy
        .filesystem_sites
        .get(input.site)
        .copied()
        .unwrap_or(false)
    {
        return Ok(run(input));
    }
    let permit = tokio::time::timeout_at(
        deadline,
        crate::config::prepare::static_capacity().acquire_owned(),
    )
    .await
    .map_err(|_| Failure::Timeout)?
    .map_err(|_| Failure::Unavailable)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run(input)
    });
    let result = tokio::time::timeout_at(deadline, task)
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Unavailable)?;
    if Instant::now() >= deadline {
        return Err(Failure::Timeout);
    }
    Ok(result)
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
