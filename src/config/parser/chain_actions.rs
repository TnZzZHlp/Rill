use super::chain::{children, invalid, match_block, shape, string};
use super::values::{at, unknown};
use crate::config::{error::Result, source::Origin};
use crate::http_policy::model::{Action, Branch};
use kdl::KdlNode;
use std::collections::BTreeSet;

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
        text: text.into(),
        content_type,
    })
}
pub(super) fn sequence<'a>(
    nodes: impl IntoIterator<Item = &'a KdlNode>,
    origin: &Origin,
    item: &'static str,
    depth: usize,
    count: &mut usize,
    error_chain: bool,
) -> Result<Vec<Action>> {
    let mut actions = Vec::new();
    let mut terminated = false;
    for node in nodes {
        if terminated {
            return Err(invalid("POLICY_INVALID_CHAIN", item, origin));
        }
        let act = action(node, &at(node, origin), depth, count, error_chain)?;
        terminated = match &act {
            Action::BasicAuth(_)
            | Action::StaticToken(_)
            | Action::BodyReplace(_)
            | Action::Encode(_)
            | Action::Header(_)
            | Action::Rewrite(_)
            | Action::StripPrefix(_)
            | Action::TryFiles { .. } => false,
            Action::WebDav(_)
            | Action::DynamicProxy(_)
            | Action::Continue
            | Action::Reenter
            | Action::Route { .. }
            | Action::Respond { .. }
            | Action::Branch(_) => true,
        };
        actions.push(act);
    }
    if !terminated {
        return Err(invalid("POLICY_INVALID_CHAIN", item, origin));
    }
    Ok(actions)
}

fn action(
    node: &KdlNode,
    origin: &Origin,
    depth: usize,
    count: &mut usize,
    error_chain: bool,
) -> Result<Action> {
    if depth > 16 {
        return Err(invalid("POLICY_LIMIT", "branch", origin));
    }
    match node.name().value() {
        "handle-errors" => Err(invalid("POLICY_INVALID_CHAIN", "handle-errors", origin)),
        "continue" => {
            shape(node, origin, "continue", 0..=0, false)?;
            Ok(Action::Continue)
        }
        "reenter" => {
            shape(node, origin, "reenter", 0..=0, false)?;
            Ok(Action::Reenter)
        }
        "respond" => {
            let action = respond(node, origin)?;
            if error_chain && matches!(&action, Action::Respond { status, .. } if *status < 400) {
                return Err(invalid("POLICY_INVALID_RESPONSE", "respond", origin));
            }
            Ok(action)
        }
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
            let protocol = match protocol.as_deref() {
                None | Some("http1") => crate::config::model::UpstreamProtocol::Http1,
                Some("h2c") => crate::config::model::UpstreamProtocol::H2c,
                Some("tls-h2") => crate::config::model::UpstreamProtocol::TlsH2,
                Some(_) => return Err(invalid("CONFIG_UNSUPPORTED", "protocol", origin)),
            };
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
                    proxy_protocol: protocol,
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
                proxy_protocol: Default::default(),
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
                    Some(match_block(
                        child,
                        &at(child, &loc),
                        depth + 1,
                        count,
                        error_chain,
                    )?)
                } else {
                    None
                };
                let actions = sequence(iter, &loc, "branch", depth + 1, count, error_chain)?;
                branches.push(Branch {
                    matcher: m,
                    actions,
                    origin: loc,
                });
            }
            if branches.is_empty() {
                return Err(invalid("POLICY_INVALID_BRANCH", "branch", origin));
            }
            Ok(Action::Branch(branches))
        }
        "header" => Ok(Action::Header(std::sync::Arc::new(
            super::chain_headers::parse(node, origin)?,
        ))),
        "rewrite" | "strip-prefix" | "try-files" => super::chain_paths::parse(node, origin),
        "basic-auth" | "static-token" => super::chain_auth::parse(node, origin),
        "dynamic-proxy" => super::chain_dynamic::parse(node, origin),
        "body-replace" | "encode" => super::chain_body::parse(node, origin),
        "webdav" => super::chain_webdav::parse(node, origin),
        _ => Err(unknown(node, origin)),
    }
}
