use super::chain::{children, invalid, shape};
use super::values::at;
use crate::body_transform::{
    model::{Encode, Replace, Rule, RuleKind},
    replacement,
};
use crate::config::{Result, source::Origin};
use crate::http_policy::model::Action;
use kdl::KdlNode;
use std::sync::Arc;

fn mime(value: &str, origin: &Origin) -> Result<String> {
    let Some((major, minor)) = value.split_once('/') else {
        return Err(invalid("POLICY_INVALID_MIME", "mime", origin));
    };
    if major.is_empty()
        || minor.is_empty()
        || value.contains('*')
        || major.parse::<http::HeaderName>().is_err()
        || minor.parse::<http::HeaderName>().is_err()
    {
        return Err(invalid("POLICY_INVALID_MIME", "mime", origin));
    }
    Ok(value.to_ascii_lowercase())
}

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<Action> {
    let item = if node.name().value() == "encode" {
        "encode"
    } else {
        "body-replace"
    };
    if node.ty().is_some()
        || node.entries().len() != 1
        || node.entries()[0].ty().is_some()
        || node.entries()[0].name().map(kdl::KdlIdentifier::value) != Some("mime")
        || node.children().is_none()
    {
        return Err(invalid("CONFIG_UNKNOWN", item, origin));
    }
    let value = node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| invalid("CONFIG_TYPE", item, origin))?;
    let mut mimes = vec![mime(value, origin)?];
    let mut rules = Vec::new();
    let mut gzip = false;
    let mut zstd = false;
    for child in children(node) {
        let loc = at(child, origin);
        match child.name().value() {
            "mime" => {
                shape(child, &loc, "mime", 1..=1, false)?;
                let value = child.entries()[0]
                    .value()
                    .as_string()
                    .ok_or_else(|| invalid("CONFIG_TYPE", "mime", &loc))?;
                let value = mime(value, &loc)?;
                if mimes.contains(&value) {
                    return Err(invalid("DUPLICATE", "mime", &loc));
                }
                mimes.push(value);
            }
            "literal" | "regex" if item == "body-replace" => {
                shape(child, &loc, "body-replace", 2..=2, false)?;
                let pattern = child.entries()[0]
                    .value()
                    .as_string()
                    .ok_or_else(|| invalid("CONFIG_TYPE", item, &loc))?;
                let value = child.entries()[1]
                    .value()
                    .as_string()
                    .ok_or_else(|| invalid("CONFIG_TYPE", item, &loc))?;
                let kind = if child.name().value() == "literal" {
                    RuleKind::Literal(pattern.to_owned(), value.to_owned())
                } else {
                    RuleKind::Regex(
                        pattern.to_owned(),
                        replacement::parse(value)
                            .map_err(|_| invalid("POLICY_INVALID_REPLACEMENT", item, &loc))?,
                    )
                };
                rules.push(Rule {
                    kind,
                    origin: loc.clone(),
                });
                if rules.len() > 16 {
                    return Err(invalid("POLICY_LIMIT", item, &loc));
                }
            }
            "gzip" | "zstd" if item == "encode" => {
                if child.ty().is_some()
                    || child.children().is_some()
                    || child.entries().len() != 1
                    || child.entries()[0].ty().is_some()
                    || child.entries()[0].name().map(kdl::KdlIdentifier::value) != Some("level")
                {
                    return Err(invalid("CONFIG_UNKNOWN", item, &loc));
                }
                let level = child.entries()[0]
                    .value()
                    .as_integer()
                    .ok_or_else(|| invalid("CONFIG_TYPE", item, &loc))?;
                if level != 3 {
                    return Err(invalid("POLICY_LIMIT", item, &loc));
                }
                let seen = if child.name().value() == "gzip" {
                    &mut gzip
                } else {
                    &mut zstd
                };
                if *seen {
                    return Err(invalid("DUPLICATE", item, &loc));
                }
                *seen = true;
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", item, &loc)),
        }
    }
    if item == "encode" {
        if !gzip && !zstd {
            return Err(invalid("POLICY_INVALID_ENCODE", item, origin));
        }
        Ok(Action::Encode(Arc::new(Encode {
            mimes,
            gzip,
            zstd,
            origin: origin.clone(),
        })))
    } else {
        if rules.is_empty() {
            return Err(invalid("POLICY_INVALID_REPLACEMENT", item, origin));
        }
        Ok(Action::BodyReplace(Arc::new(Replace {
            mimes,
            rules,
            origin: origin.clone(),
        })))
    }
}
