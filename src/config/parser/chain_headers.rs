use super::chain::{children, invalid, shape};
use super::values::{at, unknown};
use crate::config::{error::Result, source::Origin};
use crate::http_policy::{
    headers::{HeaderPolicy, Operation, Side, protected},
    template::Template,
};
use http::{HeaderName, HeaderValue};
use kdl::KdlNode;

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<HeaderPolicy> {
    if node.ty().is_some() || node.children().is_none() {
        return Err(invalid("CONFIG_UNKNOWN", "header", origin));
    }
    let mut entries = node.entries().iter();
    let first = entries
        .next()
        .ok_or_else(|| invalid("CONFIG_TYPE", "header", origin))?;
    if first.name().is_some() || first.ty().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", "header", origin));
    }
    let side = match first.value().as_string() {
        Some("request") => Side::Request,
        Some("response") => Side::Response,
        _ => return Err(invalid("CONFIG_TYPE", "header", origin)),
    };
    let mut content_type = None;
    let mut status = None;
    let mut real_client_ip = false;
    for entry in entries {
        if entry.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", "header", origin));
        }
        let v = entry
            .value()
            .as_string()
            .ok_or_else(|| invalid("CONFIG_TYPE", "header", origin))?;
        match entry.name().map(kdl::KdlIdentifier::value) {
            Some("when-content-type") if side == Side::Response && content_type.is_none() => {
                let (major, minor) = v
                    .split_once('/')
                    .ok_or_else(|| invalid("POLICY_INVALID_RESPONSE", "header", origin))?;
                if major.is_empty()
                    || minor.is_empty()
                    || major.parse::<HeaderName>().is_err()
                    || minor.parse::<HeaderName>().is_err()
                {
                    return Err(invalid("POLICY_INVALID_RESPONSE", "header", origin));
                }
                content_type = Some(v.to_owned());
            }
            Some("when-status") if side == Side::Response && status.is_none() => {
                let (a, b) = v
                    .split_once("..")
                    .ok_or_else(|| invalid("POLICY_INVALID_RESPONSE", "header", origin))?;
                if a.len() != 3
                    || b.len() != 3
                    || !a.bytes().all(|c| c.is_ascii_digit())
                    || !b.bytes().all(|c| c.is_ascii_digit())
                {
                    return Err(invalid("POLICY_INVALID_RESPONSE", "header", origin));
                }
                let a = a
                    .parse::<u16>()
                    .map_err(|_| invalid("POLICY_INVALID_RESPONSE", "header", origin))?;
                let b = b
                    .parse::<u16>()
                    .map_err(|_| invalid("POLICY_INVALID_RESPONSE", "header", origin))?;
                if !(200..=599).contains(&a) || !(200..=599).contains(&b) || a > b {
                    return Err(invalid("POLICY_INVALID_RESPONSE", "header", origin));
                }
                status = Some((a, b));
            }
            Some("real-client-ip") if side == Side::Request && !real_client_ip => {
                if !v.eq_ignore_ascii_case("x-real-ip") {
                    return Err(invalid("POLICY_PROTECTED_HEADER", "header", origin));
                }
                real_client_ip = true;
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "header", origin)),
        }
    }
    let mut operations = Vec::new();
    for child in children(node) {
        let loc = at(child, origin);
        let op = child.name().value();
        let count = match op {
            "set" | "add" => 2,
            "delete" | "vary" => 1,
            "replace" => 3,
            _ => return Err(unknown(child, &loc)),
        };
        shape(child, &loc, "header", count..=count, false)?;
        let args = child
            .entries()
            .iter()
            .map(|e| {
                e.value()
                    .as_string()
                    .ok_or_else(|| invalid("CONFIG_TYPE", "header", &loc))
            })
            .collect::<Result<Vec<_>>>()?;
        if op == "vary" {
            if side != Side::Response || args[0].parse::<HeaderName>().is_err() {
                return Err(invalid("POLICY_INVALID_RESPONSE", "header", &loc));
            }
            operations.push(Operation::Vary(args[0].to_owned()));
            continue;
        }
        if args[0].eq_ignore_ascii_case(":authority") {
            return Err(invalid("POLICY_PROTECTED_HEADER", "header", &loc));
        }
        let name = args[0]
            .parse::<HeaderName>()
            .map_err(|_| invalid("CONFIG_TYPE", "header", &loc))?;
        if protected(&name, &[]) {
            return Err(invalid("POLICY_PROTECTED_HEADER", "header", &loc));
        }
        let template = |v: &str| {
            Template::compile(v).map_err(|_| invalid("POLICY_INVALID_TEMPLATE", "header", &loc))
        };
        operations.push(match op {
            "set" => Operation::Set(name, template(args[1])?),
            "add" => Operation::Add(name, template(args[1])?),
            "delete" => Operation::Delete(name),
            "replace" => {
                HeaderValue::from_str(args[1])
                    .map_err(|_| invalid("POLICY_INVALID_TEMPLATE", "header", &loc))?;
                Operation::Replace(name, args[1].to_owned(), template(args[2])?)
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "header", &loc)),
        });
    }
    Ok(HeaderPolicy {
        side,
        content_type,
        status,
        real_client_ip,
        operations,
    })
}
