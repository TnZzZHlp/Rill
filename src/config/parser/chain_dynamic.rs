use super::chain::{children, invalid, shape};
use super::values::at;
use crate::config::{error::Result, source::Origin};
use crate::http_policy::model::{Action, DynamicProxy};
use kdl::KdlNode;

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<Action> {
    if node.ty().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", "dynamic-proxy", origin));
    }
    let mut prefix = None;
    let mut anonymous = None;
    for entry in node.entries() {
        if entry.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", "dynamic-proxy", origin));
        }
        match entry.name().map(kdl::KdlIdentifier::value) {
            Some("prefix") if prefix.is_none() => {
                prefix = Some(
                    entry
                        .value()
                        .as_string()
                        .ok_or_else(|| invalid("CONFIG_TYPE", "prefix", origin))?
                        .to_owned(),
                )
            }
            Some("anonymous") if anonymous.is_none() => {
                anonymous = Some(
                    entry
                        .value()
                        .as_bool()
                        .ok_or_else(|| invalid("CONFIG_TYPE", "anonymous", origin))?,
                )
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "dynamic-proxy", origin)),
        }
    }
    let prefix = prefix
        .filter(|p| {
            p.starts_with('/')
                && p.ends_with('/')
                && !p.contains(['?', '#', '\\'])
                && !p.bytes().any(|b| b.is_ascii_control())
        })
        .ok_or_else(|| invalid("POLICY_INVALID_PATH", "prefix", origin))?;
    let mut ports = Vec::new();
    for child in children(node) {
        let loc = at(child, origin);
        if child.name().value() != "allow-port" {
            return Err(invalid("CONFIG_UNKNOWN", "dynamic-proxy", &loc));
        }
        shape(child, &loc, "allow-port", 1..=1, false)?;
        let port = child.entries()[0]
            .value()
            .as_integer()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n != 0)
            .ok_or_else(|| invalid("CONFIG_TYPE", "allow-port", &loc))?;
        if ports.contains(&port) {
            return Err(invalid("CONFIG_DUPLICATE", "allow-port", &loc));
        }
        ports.push(port);
    }
    Ok(Action::DynamicProxy(DynamicProxy {
        prefix,
        anonymous: anonymous.unwrap_or(false),
        ports,
        origin: origin.clone(),
    }))
}
