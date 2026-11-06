use super::model::{Encode, Replace, RuleKind};
use crate::{
    config::{ConfigError, Result, source::Origin},
    http_policy::{
        compile::{PreparedPolicy, compile_regex, origin_bytes},
        model::{Action, Chain, Rule},
    },
};
use std::collections::{BTreeMap, VecDeque};

pub(crate) fn replace(policy: &mut PreparedPolicy, replace: &Replace) -> Result<()> {
    let bytes = std::mem::size_of::<Replace>()
        + replace.mimes.iter().map(String::capacity).sum::<usize>()
        + replace.mimes.capacity() * std::mem::size_of::<String>()
        + replace.rules.capacity() * std::mem::size_of::<super::model::Rule>()
        + origin_bytes(&replace.origin)
        + 2 * std::mem::size_of::<usize>();
    policy.reserve(bytes, &replace.origin)?;
    for rule in &replace.rules {
        policy.reserve(origin_bytes(&rule.origin), &rule.origin)?;
        match &rule.kind {
            RuleKind::Literal(pattern, replacement) => {
                policy.reserve(pattern.capacity() + replacement.capacity(), &rule.origin)?
            }
            RuleKind::Regex(pattern, parts) => {
                policy.reserve(
                    pattern.capacity()
                        + parts.capacity() * std::mem::size_of::<super::replacement::Part>()
                        + parts
                            .iter()
                            .map(|p| match p {
                                super::replacement::Part::Literal(b) => b.len(),
                                super::replacement::Part::Capture(_) => 0,
                            })
                            .sum::<usize>(),
                    &rule.origin,
                )?;
                compile_regex(policy, pattern, &rule.origin, true, true)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn encode_bytes(encode: &Encode) -> usize {
    std::mem::size_of::<Encode>()
        + encode.mimes.capacity() * std::mem::size_of::<String>()
        + encode.mimes.iter().map(String::capacity).sum::<usize>()
        + origin_bytes(&encode.origin)
        + 2 * std::mem::size_of::<usize>()
}

fn limit(origin: &Origin) -> ConfigError {
    ConfigError::new(
        "POLICY_LIMIT",
        "body-transform",
        "body transform declaration limit",
        origin,
    )
}

#[derive(Clone, Copy)]
enum Flow {
    Continue,
    Reenter,
    Terminal,
    Error,
}

fn paths(actions: &[Action], mut count: usize, out: &mut Vec<(Flow, usize)>) -> Result<()> {
    for action in actions {
        match action {
            Action::BodyReplace(r) => {
                count += r.rules.len();
                if count > 16 {
                    return Err(limit(&r.origin));
                }
            }
            Action::TryFiles { .. } | Action::StripPrefix(_) => out.push((Flow::Error, count)),
            Action::Continue => {
                out.push((Flow::Continue, count));
                return Ok(());
            }
            Action::Reenter => {
                out.push((Flow::Reenter, count));
                return Ok(());
            }
            Action::Branch(branches) => {
                for branch in branches {
                    paths(&branch.actions, count, out)?;
                }
                if branches
                    .last()
                    .is_none_or(|branch| branch.matcher.is_some())
                {
                    out.push((Flow::Error, count));
                }
                return Ok(());
            }
            Action::Route { raw, .. } => {
                if matches!(raw.action, crate::config::model::RawAction::Root(_)) {
                    out.push((Flow::Error, count));
                }
                out.push((Flow::Terminal, count));
                return Ok(());
            }
            Action::DynamicProxy(_) | Action::Respond { .. } => {
                out.push((Flow::Terminal, count));
                return Ok(());
            }
            _ => (),
        }
    }
    Ok(())
}

fn validate_encoders<'a>(actions: &'a [Action], first: &mut Option<&'a Origin>) -> Result<()> {
    for action in actions {
        match action {
            Action::Encode(encode) => {
                if let Some(origin) = first {
                    return Err(limit(&encode.origin).related(origin));
                }
                *first = Some(&encode.origin);
            }
            Action::Branch(branches) => {
                for branch in branches {
                    validate_encoders(&branch.actions, first)?;
                }
            }
            _ => (),
        }
    }
    Ok(())
}

pub(crate) fn validate(chain: &Chain) -> Result<()> {
    let mut encoder = None;
    for rule in chain.rules.iter().chain(chain.errors.iter().flatten()) {
        validate_encoders(&rule.actions, &mut encoder)?;
    }
    let mut queue = VecDeque::from([(false, 0usize, 0u8, 0usize)]);
    let mut visited = BTreeMap::new();
    while let Some((error, index, reentries, count)) = queue.pop_front() {
        let key = (error, index, reentries);
        if visited.get(&key).is_some_and(|&previous| previous >= count) {
            continue;
        }
        visited.insert(key, count);
        let rules: &[Rule] = if error {
            chain.errors.as_deref().unwrap_or_default()
        } else {
            &chain.rules
        };
        let Some(rule) = rules.get(index) else {
            if !error && chain.errors.is_some() {
                queue.push_back((true, 0, reentries, count));
            }
            continue;
        };
        if rule.matcher.is_some() {
            queue.push_back((error, index + 1, reentries, count));
        }
        let mut out = Vec::new();
        paths(&rule.actions, count, &mut out)?;
        for (flow, count) in out {
            match flow {
                Flow::Continue => queue.push_back((error, index + 1, reentries, count)),
                Flow::Reenter if reentries < 4 => queue.push_back((error, 0, reentries + 1, count)),
                Flow::Error if !error && chain.errors.is_some() => {
                    queue.push_back((true, 0, reentries, count))
                }
                _ => (),
            }
        }
    }
    Ok(())
}
