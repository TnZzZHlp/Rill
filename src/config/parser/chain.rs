use super::chain_actions::sequence;
use super::values::{argument, at, shape as value_shape, string as value_string, unknown};
use crate::config::{
    error::{ConfigError, Result},
    source::Origin,
};
use crate::http_policy::model::{Chain, Matcher, MatcherKind, Rule};
use kdl::KdlNode;
use std::collections::BTreeSet;

pub(super) fn invalid(code: &'static str, item: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new(code, item, "invalid policy configuration", origin)
}
pub(super) fn shape(
    node: &KdlNode,
    origin: &Origin,
    item: &'static str,
    count: std::ops::RangeInclusive<usize>,
    children: bool,
) -> Result<()> {
    if node.ty().is_some()
        || node
            .entries()
            .iter()
            .any(|entry| entry.ty().is_some() || entry.name().is_some())
    {
        return Err(invalid("CONFIG_UNKNOWN", item, origin));
    }
    value_shape(node, origin, item, count, children)
}
pub(super) fn string(
    node: &KdlNode,
    origin: &Origin,
    item: &'static str,
) -> Result<crate::config::source::Located<String>> {
    shape(node, origin, item, 1..=1, false)?;
    value_string(node, origin, item)
}
pub(super) fn children(node: &KdlNode) -> impl Iterator<Item = &KdlNode> {
    node.children().into_iter().flat_map(|d| d.nodes())
}
fn matcher(
    node: &KdlNode,
    origin: &Origin,
    depth: usize,
    count: &mut usize,
    error_chain: bool,
) -> Result<Matcher> {
    if depth > 16 {
        return Err(invalid("POLICY_LIMIT", "match", origin));
    }
    *count += 1;
    if *count > 256 {
        return Err(invalid("POLICY_LIMIT", "match", origin));
    }
    let kind = match node.name().value() {
        "all" | "any" | "not" => {
            shape(node, origin, "match", 0..=0, true)?;
            let list = children(node)
                .map(|n| matcher(n, &at(n, origin), depth + 1, count, error_chain))
                .collect::<Result<Vec<_>>>()?;
            match node.name().value() {
                "all" if !list.is_empty() => MatcherKind::All(list),
                "any" if !list.is_empty() => MatcherKind::Any(list),
                "not" if list.len() == 1 => MatcherKind::Not(Box::new(
                    list.into_iter()
                        .next()
                        .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "not", origin))?,
                )),
                _ => return Err(invalid("POLICY_INVALID_MATCHER", "match", origin)),
            }
        }
        "method" => {
            let method = string(node, origin, "method")?.value;
            method
                .parse::<http::Method>()
                .map_err(|_| invalid("POLICY_INVALID_MATCHER", "method", origin))?;
            MatcherKind::Method(method)
        }
        "path-glob" => MatcherKind::PathGlob(string(node, origin, "path-glob")?.value),
        "path-regex" => MatcherKind::PathRegex(string(node, origin, "path-regex")?.value),
        "header-equal" | "header-prefix" => {
            shape(node, origin, "header", 2..=2, false)?;
            let name = argument(node, 0, origin, "header")?.value;
            let value = node.entries()[1]
                .value()
                .as_string()
                .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "header", origin))?
                .to_owned();
            if name.parse::<http::header::HeaderName>().is_err()
                || http::HeaderValue::from_str(&value).is_err()
            {
                return Err(invalid("POLICY_INVALID_MATCHER", "header", origin));
            }
            if node.name().value() == "header-equal" {
                MatcherKind::HeaderEqual(name, value)
            } else {
                MatcherKind::HeaderPrefix(name, value)
            }
        }
        "client-cidr" => {
            let value = string(node, origin, "client-cidr")?.value;
            let (ip, prefix) = value
                .split_once('/')
                .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "client-cidr", origin))?;
            let address: std::net::IpAddr = ip
                .parse()
                .map_err(|_| invalid("POLICY_INVALID_MATCHER", "client-cidr", origin))?;
            if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("POLICY_INVALID_MATCHER", "client-cidr", origin));
            }
            let prefix = prefix
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= if address.is_ipv4() { 32 } else { 128 })
                .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "client-cidr", origin))?;
            let (address, prefix) = match address {
                std::net::IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some() => {
                    let address = ip
                        .to_ipv4_mapped()
                        .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "client-cidr", origin))?;
                    let prefix = prefix
                        .checked_sub(96)
                        .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "client-cidr", origin))?;
                    (std::net::IpAddr::V4(address), prefix)
                }
                _ => (address, prefix),
            };
            MatcherKind::ClientCidr(crate::config::model::Network { address, prefix })
        }
        "error-status" => {
            if !error_chain
                || node.ty().is_some()
                || node.children().is_some()
                || node.entries().len() != 1
                || node.entries()[0].name().is_some()
                || node.entries()[0].ty().is_some()
            {
                return Err(invalid("POLICY_INVALID_MATCHER", "error-status", origin));
            }
            let status = node.entries()[0]
                .value()
                .as_integer()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| (400..=599).contains(n))
                .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "error-status", origin))?;
            MatcherKind::ErrorStatus(status)
        }
        _ => return Err(unknown(node, origin)),
    };
    Ok(Matcher {
        kind,
        origin: origin.clone(),
    })
}
pub(super) fn match_block(
    node: &KdlNode,
    origin: &Origin,
    depth: usize,
    count: &mut usize,
    error_chain: bool,
) -> Result<Matcher> {
    shape(node, origin, "match", 0..=0, true)?;
    let mut nodes = children(node);
    let first = nodes
        .next()
        .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "match", origin))?;
    if nodes.next().is_some() {
        return Err(invalid("POLICY_INVALID_MATCHER", "match", origin));
    }
    matcher(first, &at(first, origin), depth, count, error_chain)
}
pub(super) fn parse_rules<'a>(
    nodes: impl IntoIterator<Item = (&'a KdlNode, &'a Origin)>,
    origin: &Origin,
    error_chain: bool,
    total: &mut usize,
) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    let mut ids = BTreeSet::new();
    let mut count = 0;
    for (n, node_origin) in nodes {
        let loc = node_origin.clone();
        if n.name().value() == "handle-errors" {
            return Err(invalid("POLICY_INVALID_CHAIN", "handle-errors", &loc));
        }
        if n.name().value() != "rule" {
            return Err(unknown(n, &loc));
        }
        *total = total
            .checked_add(1)
            .ok_or_else(|| invalid("POLICY_LIMIT", "rule", &loc))?;
        if *total > 4096 {
            return Err(invalid("POLICY_LIMIT", "rule", &loc));
        }
        shape(n, &loc, "rule", 0..=1, true)?;
        let id = if n.entries().is_empty() {
            None
        } else {
            Some(argument(n, 0, &loc, "rule")?.value)
        };
        if let Some(id) = &id
            && !ids.insert(id.clone())
        {
            return Err(invalid("POLICY_INVALID_CHAIN", "rule", &loc));
        }
        let mut iter = children(n);
        let first = iter
            .next()
            .ok_or_else(|| invalid("POLICY_INVALID_CHAIN", "rule", &loc))?;
        let (matcher, first_action) = if first.name().value() == "match" {
            (
                Some(match_block(
                    first,
                    &at(first, &loc),
                    1,
                    &mut count,
                    error_chain,
                )?),
                iter.next()
                    .ok_or_else(|| invalid("POLICY_INVALID_CHAIN", "rule", &loc))?,
            )
        } else {
            (None, first)
        };
        let actions = sequence(
            std::iter::once(first_action).chain(iter),
            &loc,
            "rule",
            1,
            &mut count,
            error_chain,
        )?;
        rules.push(Rule {
            id,
            matcher,
            actions,
            origin: loc,
        });
    }
    if rules.is_empty() {
        return Err(invalid("POLICY_INVALID_CHAIN", "chain", origin));
    }
    Ok(rules)
}
pub fn parse(
    node: &KdlNode,
    origin: &Origin,
    snippets: &mut super::chain_snippets::Snippets<'_>,
) -> Result<Chain> {
    shape(node, origin, "chain", 0..=0, true)?;
    let nodes: Vec<_> = children(node).collect();
    let mut total = 0;
    let position = nodes
        .iter()
        .position(|n| n.name().value() == "handle-errors");
    let (main, error_node) = if let Some(i) = position {
        if i + 1 != nodes.len() {
            return Err(invalid(
                "POLICY_INVALID_CHAIN",
                "handle-errors",
                &at(nodes[i], origin),
            ));
        }
        (&nodes[..i], Some(nodes[i]))
    } else {
        (&nodes[..], None)
    };
    let expanded = snippets.expand(main.iter().copied(), origin)?;
    let rules = parse_rules(
        expanded.iter().map(|n| (&n.node, &n.origin)),
        origin,
        false,
        &mut total,
    )?;
    let errors = if let Some(node) = error_node {
        let loc = at(node, origin);
        shape(node, &loc, "handle-errors", 0..=0, true)?;
        let expanded = snippets.expand(children(node), &loc)?;
        Some(parse_rules(
            expanded.iter().map(|n| (&n.node, &n.origin)),
            &loc,
            true,
            &mut total,
        )?)
    } else {
        None
    };
    Ok(Chain {
        rules,
        errors,
        origin: origin.clone(),
    })
}
