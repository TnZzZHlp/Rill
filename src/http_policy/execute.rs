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
        text: std::sync::Arc<str>,
        content_type: String,
    },
    Route(usize),
    Dynamic(usize),
    NotFound,
    NoMatch,
    BudgetExceeded,
    Authorize(Box<Cursor>),
    AuthenticationFailed {
        challenge: Option<String>,
    },
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

#[derive(Clone, Debug)]
pub struct Cursor {
    pub rule: usize,
    pub branches: Vec<(usize, usize)>,
    pub next: usize,
    pub captures: [Option<(usize, usize)>; 9],
    pub error_status: Option<u16>,
}

pub(super) enum ActionFlow {
    Authorize {
        branches: Vec<(usize, usize)>,
        next: usize,
    },
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
        Action::BasicAuth(_) | Action::StaticToken(_) => unreachable!(),
        Action::BodyReplace(replace) => {
            ctx.state
                .as_deref_mut()
                .ok_or(())?
                .register_replace(replace)
                .map_err(|_| ())?;
            Ok(None)
        }
        Action::Encode(encode) => {
            ctx.state
                .as_deref_mut()
                .ok_or(())?
                .register_encode(encode)
                .map_err(|_| ())?;
            Ok(None)
        }
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
        Action::DynamicProxy(dynamic) => {
            if !dynamic.anonymous && !ctx.state.as_ref().is_some_and(|s| s.authenticated()) {
                return Ok(Some(ActionFlow::Terminal(Outcome::PathFailure {
                    class: PathErrorClass::InvalidInput,
                    code: "DYNAMIC_AUTH_REQUIRED",
                    status: 403,
                })));
            }
            let index = *policy
                .dynamic_indices
                .get(&(dynamic.origin.source.path.clone(), dynamic.origin.offset))
                .ok_or(())?;
            Ok(Some(ActionFlow::Terminal(Outcome::Dynamic(index))))
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
        Action::Branch(_) => unreachable!(),
    }
}
fn sequence(
    actions: &[Action],
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
    path: &[(usize, usize)],
    next: usize,
) -> Result<ActionFlow, ()> {
    if let Some((&(action_index, branch_index), rest)) = path.split_first() {
        let Action::Branch(branches) = actions.get(action_index).ok_or(())? else {
            return Err(());
        };
        let mut flow = sequence(
            &branches.get(branch_index).ok_or(())?.actions,
            ctx,
            policy,
            rest,
            next,
        )?;
        if let ActionFlow::Authorize { branches, .. } = &mut flow {
            branches.insert(0, (action_index, branch_index));
        }
        return Ok(flow);
    }
    for (index, a) in actions.iter().enumerate().skip(next) {
        match a {
            Action::BasicAuth(_) | Action::StaticToken(_) => {
                step(ctx)?;
                return Ok(ActionFlow::Authorize {
                    branches: Vec::new(),
                    next: index + 1,
                });
            }
            Action::Branch(branches) => {
                step(ctx)?;
                for (branch_index, branch) in branches.iter().enumerate() {
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
                        let mut flow = sequence(&branch.actions, ctx, policy, &[], 0)?;
                        if let ActionFlow::Authorize { branches, .. } = &mut flow {
                            branches.insert(0, (index, branch_index));
                        }
                        return Ok(flow);
                    }
                }
                return Ok(ActionFlow::Terminal(Outcome::NotFound));
            }
            _ => {
                if let Some(flow) = action(a, ctx, policy)? {
                    return Ok(flow);
                }
            }
        }
    }
    Err(())
}

pub fn suspended_action<'a>(chain: &'a Chain, cursor: &Cursor) -> Option<&'a Action> {
    let rules = if cursor.error_status.is_some() {
        chain.errors.as_ref()?
    } else {
        &chain.rules
    };
    let mut actions = rules.get(cursor.rule)?.actions.as_slice();
    for &(action, branch) in &cursor.branches {
        let Action::Branch(branches) = actions.get(action)? else {
            return None;
        };
        actions = &branches.get(branch)?.actions;
    }
    actions.get(cursor.next.checked_sub(1)?)
}

fn execute_rules(
    rules: &[super::model::Rule],
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
    mut cursor: Option<Cursor>,
) -> Outcome {
    let mut rule_index = cursor.as_ref().map_or(0, |c| c.rule);
    'chain: loop {
        while let Some(rule) = rules.get(rule_index) {
            let resumed = cursor.take();
            if let Some(c) = &resumed {
                ctx.captures = c.captures;
            } else {
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
                    Ok(MatchResult { matched: false, .. }) => {
                        rule_index += 1;
                        continue;
                    }
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
            }
            let path = resumed.as_ref().map_or(&[][..], |c| c.branches.as_slice());
            let next = resumed.as_ref().map_or(0, |c| c.next);
            match sequence(&rule.actions, ctx, policy, path, next) {
                Ok(ActionFlow::Authorize { branches, next }) => {
                    return Outcome::Authorize(Box::new(Cursor {
                        rule: rule_index,
                        branches,
                        next,
                        captures: ctx.captures,
                        error_status: ctx.error_status,
                    }));
                }
                Ok(ActionFlow::Terminal(outcome)) => return outcome,
                Ok(ActionFlow::Continue) => rule_index += 1,
                Ok(ActionFlow::Reenter) => {
                    if step(ctx).is_err() || ctx.reentries >= 4 {
                        return Outcome::BudgetExceeded;
                    }
                    ctx.reentries += 1;
                    ctx.captures = [None; 9];
                    if let Some(state) = ctx.state.as_deref_mut()
                        && state.set_captures([None; 9]).is_err()
                    {
                        return Outcome::BudgetExceeded;
                    }
                    rule_index = 0;
                    continue 'chain;
                }
                Err(()) => return Outcome::BudgetExceeded,
            }
        }
        return Outcome::NoMatch;
    }
}

pub fn execute_resume(
    chain: &Chain,
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
    cursor: Option<Cursor>,
) -> Outcome {
    match execute_rules(&chain.rules, policy, ctx, cursor) {
        Outcome::NoMatch => Outcome::NotFound,
        outcome => outcome,
    }
}
pub fn execute(chain: &Chain, policy: &PreparedPolicy, ctx: &mut MatchContext<'_>) -> Outcome {
    execute_resume(chain, policy, ctx, None)
}
pub fn execute_errors(
    chain: &Chain,
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
    status: u16,
    cursor: Option<Cursor>,
) -> Option<Outcome> {
    let rules = chain.errors.as_ref()?;
    ctx.error_status = Some(status);
    if cursor.is_none() {
        ctx.captures = [None; 9];
        if let Some(state) = ctx.state.as_deref_mut()
            && state.set_captures([None; 9]).is_err()
        {
            return Some(Outcome::BudgetExceeded);
        }
    }
    Some(execute_rules(rules, policy, ctx, cursor))
}
