use super::values::{argument, at, shape as value_shape, string as value_string, unknown};
use crate::config::{
    error::{ConfigError, Result},
    source::Origin,
};
use crate::http_policy::model::{Action, Branch, Chain, Matcher, MatcherKind, Rule};
use kdl::KdlNode;
use std::collections::BTreeSet;

fn invalid(code: &'static str, item: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new(code, item, "invalid policy configuration", origin)
}
fn shape(
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
fn string(
    node: &KdlNode,
    origin: &Origin,
    item: &'static str,
) -> Result<crate::config::source::Located<String>> {
    shape(node, origin, item, 1..=1, false)?;
    value_string(node, origin, item)
}
fn children(node: &KdlNode) -> impl Iterator<Item = &KdlNode> {
    node.children().into_iter().flat_map(|d| d.nodes())
}
fn matcher(node: &KdlNode, origin: &Origin, depth: usize, count: &mut usize) -> Result<Matcher> {
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
                .map(|n| matcher(n, &at(n, origin), depth + 1, count))
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
        "error-status" => return Err(invalid("POLICY_INVALID_MATCHER", "error-status", origin)),
        _ => return Err(unknown(node, origin)),
    };
    Ok(Matcher {
        kind,
        origin: origin.clone(),
    })
}
fn match_block(
    node: &KdlNode,
    origin: &Origin,
    depth: usize,
    count: &mut usize,
) -> Result<Matcher> {
    shape(node, origin, "match", 0..=0, true)?;
    let mut nodes = children(node);
    let first = nodes
        .next()
        .ok_or_else(|| invalid("POLICY_INVALID_MATCHER", "match", origin))?;
    if nodes.next().is_some() {
        return Err(invalid("POLICY_INVALID_MATCHER", "match", origin));
    }
    matcher(first, &at(first, origin), depth, count)
}
fn respond(node: &KdlNode, origin: &Origin) -> Result<Action> {
    if node.ty().is_some() || node.children().is_some() {
        return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin));
    }
    let mut status = None;
    let mut text = None;
    let mut content_type = None;
    for e in node.entries() {
        if e.ty().is_some() {
            return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin));
        }
        match e.name().map(kdl::KdlIdentifier::value) {
            None if status.is_none() => {
                status = Some(
                    e.value()
                        .as_integer()
                        .and_then(|v| u16::try_from(v).ok())
                        .ok_or_else(|| invalid("POLICY_INVALID_RESPONSE", "respond", origin))?,
                );
            }
            None => return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin)),
            Some("text") if text.is_none() => text = e.value().as_string().map(str::to_owned),
            Some("content-type") if content_type.is_none() => {
                content_type = e.value().as_string().map(str::to_owned)
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "respond", origin)),
        }
        if e.name().is_some() && e.value().as_string().is_none() {
            return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin));
        }
    }
    let status = status
        .filter(|s| (200..=599).contains(s))
        .ok_or_else(|| invalid("POLICY_INVALID_RESPONSE", "respond", origin))?;
    let text = text.unwrap_or_default();
    let content_type = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".into());
    if (matches!(status, 204 | 304) && !text.is_empty())
        || http::HeaderValue::from_str(&content_type).is_err()
    {
        return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin));
    }
    Ok(Action::Respond {
        status,
        text,
        content_type,
    })
}
fn action(node: &KdlNode, origin: &Origin, depth: usize, count: &mut usize) -> Result<Action> {
    if depth > 16 {
        return Err(invalid("POLICY_LIMIT", "branch", origin));
    }
    match node.name().value() {
        "continue" => {
            shape(node, origin, "continue", 0..=0, false)?;
            Ok(Action::Continue)
        }
        "respond" => respond(node, origin),
        "proxy" => {
            if node.ty().is_some() || node.children().is_some() {
                return Err(invalid("CONFIG_UNKNOWN", "proxy", origin));
            }
            let mut target = None;
            let mut host = None;
            let mut protocol = None;
            for entry in node.entries() {
                if entry.ty().is_some() {
                    return Err(invalid("CONFIG_UNKNOWN", "proxy", origin));
                }
                match entry.name().map(kdl::KdlIdentifier::value) {
                    None if target.is_none() => {
                        target = entry.value().as_string().map(str::to_owned)
                    }
                    Some("host") if host.is_none() => {
                        host = entry.value().as_string().map(str::to_owned)
                    }
                    Some("protocol") if protocol.is_none() => {
                        protocol = entry.value().as_string().map(str::to_owned)
                    }
                    _ => return Err(invalid("CONFIG_UNKNOWN", "proxy", origin)),
                }
                if entry.value().as_string().is_none() {
                    return Err(invalid("CONFIG_UNKNOWN", "proxy", origin));
                }
            }
            if protocol.as_deref().is_some_and(|p| p != "http1") {
                return Err(invalid("CONFIG_UNSUPPORTED", "protocol", origin));
            }
            if host
                .as_ref()
                .is_some_and(|h| h.parse::<http::uri::Authority>().is_err())
            {
                return Err(invalid("POLICY_INVALID_RESPONSE", "proxy", origin));
            }
            let target = crate::config::source::Located::new(
                target
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| invalid("CONFIG_TYPE", "proxy", origin))?,
                origin.clone(),
            );
            Ok(Action::Route {
                raw: Box::new(crate::config::model::RawRoute {
                    pattern: crate::config::source::Located::new("/*".into(), origin.clone()),
                    index: None,
                    proxy_host: host
                        .map(|value| crate::config::source::Located::new(value, origin.clone())),
                    uses: Vec::new(),
                    parameters: Default::default(),
                    action: crate::config::model::RawAction::Proxy(target),
                    spa: None,
                    cache_control: None,
                    precompressed: None,
                    browse: None,
                    error_pages: Default::default(),
                }),
                index: None,
            })
        }
        "serve-files" => {
            if node.ty().is_some()
                || node.entries().len() != 1
                || node.entries()[0].name().map(kdl::KdlIdentifier::value) != Some("root")
                || node.entries()[0].ty().is_some()
            {
                return Err(invalid("CONFIG_UNKNOWN", "serve-files", origin));
            }
            let root = node.entries()[0]
                .value()
                .as_string()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid("POLICY_INVALID_PATH", "serve-files", origin))?;
            let mut raw = crate::config::model::RawRoute {
                pattern: crate::config::source::Located::new("/*".into(), origin.clone()),
                index: None,
                proxy_host: None,
                uses: Vec::new(),
                parameters: Default::default(),
                action: crate::config::model::RawAction::Root(crate::config::source::Located::new(
                    origin.relative_path(root),
                    origin.clone(),
                )),
                spa: None,
                cache_control: None,
                precompressed: None,
                browse: None,
                error_pages: Default::default(),
            };
            let mut seen = BTreeSet::new();
            for child in children(node) {
                let loc = at(child, origin);
                if !seen.insert(child.name().value().to_owned()) {
                    return Err(invalid("POLICY_INVALID_PATH", "serve-files", &loc));
                }
                match child.name().value() {
                    "spa" => raw.spa = Some(super::values::boolean(child, &loc, "spa")?),
                    "browse" => raw.browse = Some(super::values::boolean(child, &loc, "browse")?),
                    "precompressed" => {
                        raw.precompressed =
                            Some(super::values::boolean(child, &loc, "precompressed")?)
                    }
                    "cache-control" => {
                        raw.cache_control = Some(string(child, &loc, "cache-control")?)
                    }
                    "index" => {
                        let index = string(child, &loc, "index")?;
                        if index.value.len() > 1024 {
                            return Err(invalid("POLICY_LIMIT", "index", &loc));
                        }
                        if index.value.contains(['/', '\\'])
                            || matches!(index.value.as_str(), "." | "..")
                        {
                            return Err(invalid("POLICY_INVALID_PATH", "index", &loc));
                        }
                        raw.index = Some(index);
                    }
                    _ => return Err(unknown(child, &loc)),
                }
            }
            Ok(Action::Route {
                raw: Box::new(raw),
                index: None,
            })
        }
        "branch" => {
            shape(node, origin, "branch", 0..=0, true)?;
            let mut branches = Vec::new();
            let mut otherwise = false;
            for n in children(node) {
                let loc = at(n, origin);
                if otherwise {
                    return Err(invalid("POLICY_INVALID_BRANCH", "branch", &loc));
                }
                let is_when = match n.name().value() {
                    "when" => true,
                    "otherwise" => {
                        otherwise = true;
                        false
                    }
                    _ => return Err(invalid("POLICY_INVALID_BRANCH", "branch", &loc)),
                };
                shape(n, &loc, "branch", 0..=0, true)?;
                let mut iter = children(n);
                let m = if is_when {
                    let child = iter
                        .next()
                        .ok_or_else(|| invalid("POLICY_INVALID_BRANCH", "when", &loc))?;
                    if child.name().value() != "match" {
                        return Err(invalid("POLICY_INVALID_BRANCH", "when", &loc));
                    }
                    Some(match_block(child, &at(child, &loc), depth + 1, count)?)
                } else {
                    None
                };
                let a = iter
                    .next()
                    .ok_or_else(|| invalid("POLICY_INVALID_CHAIN", "branch", &loc))?;
                let act = action(a, &at(a, &loc), depth + 1, count)?;
                if iter.next().is_some() {
                    return Err(invalid("POLICY_INVALID_CHAIN", "branch", &loc));
                }
                branches.push(Branch {
                    matcher: m,
                    action: act,
                    origin: loc,
                });
            }
            if branches.is_empty() {
                return Err(invalid("POLICY_INVALID_BRANCH", "branch", origin));
            }
            Ok(Action::Branch(branches))
        }
        "header" | "rewrite" | "strip-prefix" | "try-files" | "reenter" | "basic-auth"
        | "static-token" | "webdav" | "dynamic-proxy" | "body-replace" | "encode" => {
            Err(invalid("CONFIG_UNSUPPORTED", "action", origin))
        }
        _ => Err(unknown(node, origin)),
    }
}
pub fn parse(node: &KdlNode, origin: &Origin) -> Result<Chain> {
    shape(node, origin, "chain", 0..=0, true)?;
    let mut rules = Vec::new();
    let mut ids = BTreeSet::new();
    let mut count = 0;
    for n in children(node) {
        let loc = at(n, origin);
        if n.name().value() == "handle-errors" {
            return Err(invalid("CONFIG_UNSUPPORTED", "handle-errors", &loc));
        }
        if n.name().value() != "rule" {
            return Err(unknown(n, &loc));
        }
        if rules.len() >= 4096 {
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
        let (m, a) = if first.name().value() == "match" {
            (
                Some(match_block(first, &at(first, &loc), 1, &mut count)?),
                iter.next()
                    .ok_or_else(|| invalid("POLICY_INVALID_CHAIN", "rule", &loc))?,
            )
        } else {
            (None, first)
        };
        let act = action(a, &at(a, &loc), 1, &mut count)?;
        if iter.next().is_some() {
            return Err(invalid("POLICY_INVALID_CHAIN", "rule", &loc));
        }
        rules.push(Rule {
            id,
            matcher: m,
            action: act,
            origin: loc,
        });
    }
    if rules.is_empty() {
        return Err(invalid("POLICY_INVALID_CHAIN", "chain", origin));
    }
    Ok(Chain {
        rules,
        origin: origin.clone(),
    })
}
