use super::{
    compile::PreparedPolicy,
    model::{Action, Chain, Matcher, MatcherKind},
};
use regex_automata::Input;
use std::net::IpAddr;

pub struct MatchContext<'a> {
    pub method: &'a http::Method,
    pub path: &'a str,
    pub headers: &'a http::HeaderMap,
    pub client_ip: IpAddr,
    pub steps_left: u16,
    pub captures: [Option<&'a str>; 9],
}
#[derive(Debug)]
pub enum Outcome<'a> {
    Respond {
        status: u16,
        text: &'a str,
        content_type: &'a str,
    },
    Route(usize),
    NotFound,
    BudgetExceeded,
}
fn step(ctx: &mut MatchContext<'_>) -> Result<(), ()> {
    ctx.steps_left = ctx.steps_left.checked_sub(1).ok_or(())?;
    Ok(())
}
type Captures<'a> = [Option<&'a str>; 9];
struct MatchResult<'a> {
    matched: bool,
    captures: Option<Captures<'a>>,
}
fn matches<'a>(
    m: &Matcher,
    ctx: &mut MatchContext<'a>,
    policy: &PreparedPolicy,
) -> Result<MatchResult<'a>, ()> {
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
        MatcherKind::ErrorStatus(_) => false,
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
                .search_slots(&mut cache, &Input::new(ctx.path), &mut slots[..slot_count])
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
                        *value = Some(capture);
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

fn action<'a>(
    a: &'a Action,
    ctx: &mut MatchContext<'_>,
    policy: &PreparedPolicy,
) -> Result<Option<Outcome<'a>>, ()> {
    step(ctx)?;
    match a {
        Action::Continue => Ok(None),
        Action::Route { index, .. } => Ok(Some(Outcome::Route(index.ok_or(())?))),
        Action::Respond {
            status,
            text,
            content_type,
        } => Ok(Some(Outcome::Respond {
            status: *status,
            text,
            content_type,
        })),
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
                    ctx.captures = result.captures.unwrap_or([None; 9]);
                    return action(&branch.action, ctx, policy);
                }
            }
            Ok(Some(Outcome::NotFound))
        }
    }
}
pub fn execute<'a>(
    chain: &'a Chain,
    policy: &PreparedPolicy,
    ctx: &mut MatchContext<'_>,
) -> Outcome<'a> {
    for rule in &chain.rules {
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
            Ok(result) => ctx.captures = result.captures.unwrap_or([None; 9]),
        }
        match action(&rule.action, ctx, policy) {
            Ok(Some(outcome)) => return outcome,
            Ok(None) => (),
            Err(()) => return Outcome::BudgetExceeded,
        }
    }
    Outcome::NotFound
}
