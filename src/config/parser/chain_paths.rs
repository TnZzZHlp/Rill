use super::chain::{invalid, shape};
use crate::config::{
    error::Result,
    source::{Located, Origin},
};
use crate::http_policy::{
    model::Action,
    path::{PathError, PathTemplate, validate},
};
use kdl::KdlNode;

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<Action> {
    let name = node.name().value();
    if name != "try-files" {
        shape(node, origin, "path", 1..=1, false)?;
        let value = node.entries()[0]
            .value()
            .as_string()
            .ok_or_else(|| invalid("CONFIG_TYPE", "path", origin))?;
        if name == "strip-prefix" {
            validate(value).map_err(|_| invalid("POLICY_INVALID_PATH", "strip-prefix", origin))?;
            return Ok(Action::StripPrefix(value.to_owned()));
        }
        let template = PathTemplate::compile(value).map_err(|error| {
            invalid(
                match error {
                    PathError::InvalidPath => "POLICY_INVALID_PATH",
                    PathError::InvalidTemplate => "POLICY_INVALID_TEMPLATE",
                },
                "rewrite",
                origin,
            )
        })?;
        return Ok(Action::Rewrite(template));
    }
    if node.ty().is_some() || node.children().is_some() {
        return Err(invalid("CONFIG_UNKNOWN", "try-files", origin));
    }
    let mut root = None;
    let mut candidates = Vec::new();
    for entry in node.entries() {
        if entry.ty().is_some() {
            return Err(invalid("CONFIG_UNKNOWN", "try-files", origin));
        }
        let value = entry
            .value()
            .as_string()
            .ok_or_else(|| invalid("CONFIG_TYPE", "try-files", origin))?;
        match entry.name().map(kdl::KdlIdentifier::value) {
            Some("root") if root.is_none() && !value.is_empty() => {
                root = Some(Located::new(origin.relative_path(value), origin.clone()))
            }
            None => {
                if candidates.len() >= 64 || value.len() > 4096 {
                    return Err(invalid("POLICY_LIMIT", "try-files", origin));
                }
                candidates.push(PathTemplate::compile(value).map_err(|error| {
                    invalid(
                        match error {
                            PathError::InvalidPath => "POLICY_INVALID_PATH",
                            PathError::InvalidTemplate => "POLICY_INVALID_TEMPLATE",
                        },
                        "try-files",
                        origin,
                    )
                })?);
            }
            _ => return Err(invalid("CONFIG_UNKNOWN", "try-files", origin)),
        }
    }
    if candidates.is_empty() {
        return Err(invalid("POLICY_INVALID_PATH", "try-files", origin));
    }
    Ok(Action::TryFiles {
        root: root.ok_or_else(|| invalid("POLICY_INVALID_PATH", "try-files", origin))?,
        candidates,
    })
}
