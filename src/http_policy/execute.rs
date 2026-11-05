use super::{
    compile::PreparedPolicy,
    model::{Action, Chain, Matcher, MatcherKind},
};
use regex_automata::Input;
use std::net::IpAddr;

pub struct MatchContext<'a> {
    pub method: &'a http::Method,
    pub path: String,
    pub headers: &'a mut http::HeaderMap,
    pub state: Option<&'a mut super::state::RequestPolicyState>,
    pub client_ip: IpAddr,
    pub steps_left: u16,
    pub reentries: u8,
    pub error_status: Option<u16>,
    pub captures: [Option<(usize, usize)>; 9],
}
#[derive(Debug)]
pub enum PathErrorClass {
    LocalNotFound,
    InvalidInput,
    LocalUnavailable,
    Capacity,
}
#[derive(Debug)]
pub enum Outcome {
    Respond {
        status: u16,
        text: String,
        content_type: String,
    },
    Route(usize),
    NotFound,
    NoMatch,
    BudgetExceeded,
    PathFailure {
        class: PathErrorClass,
        code: &'static str,
        status: u16,
    },
}
fn step(ctx: &mut MatchContext<'_>) -> Result<(), ()> {
    ctx.steps_left = ctx.steps_left.checked_sub(1).ok_or(())?;
    Ok(())
}
type Captures = [Option<(usize, usize)>; 9];
struct MatchResult {
    matched: bool,
    captures: Option<Captures>,
}
fn matches(
    m: &Matcher,
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
) -> Result<MatchResult, ()> {
    step(ctx)?;
    let mut captures = None;
    let matched = match &m.kind {
        MatcherKind::All(items) => {
            for item in items {
                let result = matches(item, ctx, policy)?;
                if !result.matched {
                    return Ok(MatchResult {
                        matched: false,
                        captures: None,
                    });
                }
                if result.captures.is_some() {
                    captures = result.captures;
                }
            }
            true
        }
        MatcherKind::Any(items) => {
            for item in items {
                let result = matches(item, ctx, policy)?;
                if result.matched {
                    return Ok(result);
                }
            }
            false
        }
        MatcherKind::Not(item) => !matches(item, ctx, policy)?.matched,
        MatcherKind::Method(method) => ctx.method.as_str() == method,
        MatcherKind::HeaderEqual(name, value) => ctx
            .headers
            .get_all(name)
            .iter()
            .any(|h| h.as_bytes() == value.as_bytes()),
        MatcherKind::HeaderPrefix(name, value) => ctx
            .headers
            .get_all(name)
            .iter()
            .any(|h| h.as_bytes().starts_with(value.as_bytes())),
        MatcherKind::ClientCidr(net) => net.contains(ctx.client_ip),
        MatcherKind::ErrorStatus(status) => ctx.error_status == Some(*status),
        MatcherKind::PathGlob(_) | MatcherKind::PathRegex(_) => {
            let index = *policy
                .indices
                .get(&(m.origin.source.path.clone(), m.origin.offset))
                .ok_or(())?;
            let re = policy.regexes.get(index).ok_or(())?;
            let _permit = super::workspace::reserve(&re.vm).ok_or(())?;
            let mut cache = re.vm.create_cache();
            let mut slots = [None; 20];
            let slot_count = re.vm.get_nfa().group_info().slot_len().min(slots.len());
            let found = re
                .vm
                .search_slots(&mut cache, &Input::new(&ctx.path), &mut slots[..slot_count])
                .is_some();
            if found && matches!(&m.kind, MatcherKind::PathRegex(_)) {
                let mut values = [None; 9];
                let mut bytes = 0usize;
                for (i, value) in values.iter_mut().enumerate() {
                    if let (Some(start), Some(end)) = (slots[2 * (i + 1)], slots[2 * (i + 1) + 1]) {
                        let capture = ctx.path.get(start.get()..end.get()).ok_or(())?;
                        bytes = bytes.checked_add(capture.len()).ok_or(())?;
                        if bytes > 36 * 1024 {
                            return Err(());
                        }
                        *value = Some((start.get(), end.get()));
                    }
                }
                captures = Some(values);
            }
            found
        }
    };
    Ok(MatchResult {
        matched,
        captures: if matched { captures } else { None },
    })
}

pub(super) enum ActionFlow {
    Continue,
    Reenter,
    Terminal(Outcome),
}

fn action(
    a: &Action,
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
) -> Result<Option<ActionFlow>, ()> {
    step(ctx)?;
    match a {
        Action::Header(header) => {
            ctx.state
                .as_deref_mut()
                .ok_or(())?
                .apply_header(header, ctx.headers)
                .map_err(|_| ())?;
            Ok(None)
        }
        Action::Rewrite(_) | Action::StripPrefix(_) | Action::TryFiles { .. } => {
            super::path_actions::execute(a, ctx, policy)
        }
        Action::Continue => Ok(Some(ActionFlow::Continue)),
        Action::Reenter => Ok(Some(ActionFlow::Reenter)),
        Action::Route { index, .. } => {
            Ok(Some(ActionFlow::Terminal(Outcome::Route(index.ok_or(())?))))
        }
        Action::Respond {
            status,
            text,
            content_type,
        } if ctx
            .error_status
            .is_some_and(|original| *status < 400 || (original == 503 && *status != 503)) =>
        {
            Ok(Some(ActionFlow::Terminal(Outcome::BudgetExceeded)))
        }
        Action::Respond {
            status,
            text,
            content_type,
        } => Ok(Some(ActionFlow::Terminal(Outcome::Respond {
            status: *status,
            text: text.clone(),
            content_type: content_type.clone(),
        }))),
        Action::Branch(branches) => {
            for branch in branches {
                step(ctx)?;
                let result = branch.matcher.as_ref().map_or(
                    Ok(MatchResult {
                        matched: true,
                        captures: None,
                    }),
                    |m| matches(m, ctx, policy),
                )?;
                if result.matched {
                    if let Some(captures) = result.captures {
                        ctx.captures = captures;
                        if let Some(state) = ctx.state.as_deref_mut() {
                            state
                                .set_captures(std::array::from_fn(|i| {
                                    ctx.captures[i].and_then(|(a, b)| ctx.path.get(a..b))
                                }))
                                .map_err(|_| ())?;
                        }
                    }
                    return sequence(&branch.actions, ctx, policy).map(Some);
                }
            }
            Ok(Some(ActionFlow::Terminal(Outcome::NotFound)))
        }
    }
}
fn sequence(
    actions: &[Action],
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
) -> Result<ActionFlow, ()> {
    for a in actions {
        if let Some(flow) = action(a, ctx, policy)? {
            return Ok(flow);
        }
    }
    Err(())
}
fn execute_rules(
    rules: &[super::model::Rule],
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
) -> Outcome {
    'chain: loop {
        for rule in rules {
            if step(ctx).is_err() {
                return Outcome::BudgetExceeded;
            }
            match rule.matcher.as_ref().map_or(
                Ok(MatchResult {
                    matched: true,
                    captures: None,
                }),
                |m| matches(m, ctx, policy),
            ) {
                Ok(MatchResult { matched: false, .. }) => continue,
                Err(()) => return Outcome::BudgetExceeded,
                Ok(result) => {
                    ctx.captures = result.captures.unwrap_or([None; 9]);
                    if let Some(state) = ctx.state.as_deref_mut()
                        && state
                            .set_captures(std::array::from_fn(|i| {
                                ctx.captures[i].and_then(|(a, b)| ctx.path.get(a..b))
                            }))
                            .is_err()
                    {
                        return Outcome::BudgetExceeded;
                    }
                }
            }
            match sequence(&rule.actions, ctx, policy) {
                Ok(ActionFlow::Terminal(outcome)) => return outcome,
                Ok(ActionFlow::Continue) => (),
                Ok(ActionFlow::Reenter) => {
                    if step(ctx).is_err() || ctx.reentries >= 4 {
                        return Outcome::BudgetExceeded;
                    }
                    ctx.reentries += 1;
                    ctx.captures = [None; 9];
                    if let Some(state) = ctx.state.as_deref_mut()
                        && state
                            .set_captures(std::array::from_fn(|i| {
                                ctx.captures[i].and_then(|(a, b)| ctx.path.get(a..b))
                            }))
                            .is_err()
                    {
                        return Outcome::BudgetExceeded;
                    }
                    continue 'chain;
                }
                Err(()) => return Outcome::BudgetExceeded,
            }
        }
        return Outcome::NoMatch;
    }
}

pub fn execute(chain: &Chain, policy: &PreparedPolicy, ctx: &mut MatchContext<'_>) -> Outcome {
    match execute_rules(&chain.rules, policy, ctx) {
        Outcome::NoMatch => Outcome::NotFound,
        outcome => outcome,
    }
}
pub fn execute_errors(
    chain: &Chain,
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
    status: u16,
) -> Option<Outcome> {
    let rules = chain.errors.as_ref()?;
    ctx.error_status = Some(status);
    ctx.captures = [None; 9];
    if let Some(state) = ctx.state.as_deref_mut()
        && state.set_captures([None; 9]).is_err()
    {
        return Some(Outcome::BudgetExceeded);
    }
    Some(execute_rules(rules, policy, ctx))
}
