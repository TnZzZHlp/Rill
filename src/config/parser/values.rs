use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use kdl::KdlNode;

use crate::config::error::{ConfigError, Result};
use crate::config::model::Parameters;
use crate::config::source::{Located, Origin};

pub fn at(node: &KdlNode, parent: &Origin) -> Origin {
    Origin {
        offset: node.span().offset(),
        length: node.span().len(),
        ..parent.clone()
    }
}

pub fn shape(
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
            .any(|entry| entry.name().is_some() || entry.ty().is_some())
    {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            item,
            "properties and type annotations are not supported",
            origin,
        ));
    }
    if !count.contains(&node.entries().len()) || node.children().is_some() != children {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            item,
            "incorrect argument count or child block",
            origin,
        ));
    }
    Ok(())
}

pub fn string(node: &KdlNode, origin: &Origin, item: &'static str) -> Result<Located<String>> {
    shape(node, origin, item, 1..=1, false)?;
    let value = node.entries()[0]
        .value()
        .as_string()
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| {
            ConfigError::new(
                "CONFIG_TYPE",
                item,
                "expected a nonempty string without control characters",
                origin,
            )
        })?;
    Ok(Located::new(value.to_string(), origin.clone()))
}

pub fn argument(
    node: &KdlNode,
    index: usize,
    origin: &Origin,
    item: &'static str,
) -> Result<Located<String>> {
    let value = node
        .entries()
        .get(index)
        .and_then(|entry| entry.value().as_string())
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| {
            ConfigError::new(
                "CONFIG_TYPE",
                item,
                "expected a nonempty string argument",
                origin,
            )
        })?;
    Ok(Located::new(value.to_string(), origin.clone()))
}

pub fn boolean(node: &KdlNode, origin: &Origin, item: &'static str) -> Result<Located<bool>> {
    shape(node, origin, item, 1..=1, false)?;
    let value = node.entries()[0]
        .value()
        .as_bool()
        .ok_or_else(|| ConfigError::new("CONFIG_TYPE", item, "expected #true or #false", origin))?;
    Ok(Located::new(value, origin.clone()))
}

pub fn path(node: &KdlNode, origin: &Origin, item: &'static str) -> Result<Located<PathBuf>> {
    let value = string(node, origin, item)?;
    Ok(Located::new(
        origin.relative_path(&value.value),
        origin.clone(),
    ))
}

pub fn once(
    seen: &mut BTreeSet<String>,
    node: &KdlNode,
    origin: &Origin,
    item: &'static str,
) -> Result<()> {
    if !seen.insert(node.name().value().to_string()) {
        return Err(ConfigError::new(
            "DUPLICATE",
            item,
            "scalar configuration field is repeated",
            origin,
        ));
    }
    Ok(())
}

pub fn duration(node: &KdlNode, origin: &Origin, item: &'static str) -> Result<Located<Duration>> {
    let value = string(node, origin, item)?;
    let split = value
        .value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.value.len());
    let (number, unit) = value.value.split_at(split);
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => {
            return Err(ConfigError::new(
                "CONFIG_TYPE",
                item,
                "duration must use ms, s, m or h",
                origin,
            ));
        }
    };
    let milliseconds = number
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            ConfigError::new(
                "CONFIG_TYPE",
                item,
                "duration must be a finite positive integer without overflow",
                origin,
            )
        })?;
    Ok(Located::new(
        Duration::from_millis(milliseconds),
        origin.clone(),
    ))
}

pub fn parameter(node: &KdlNode, origin: &Origin, parameters: &mut Parameters) -> Result<bool> {
    match node.name().value() {
        "connect-timeout" => {
            parameters.connect_timeout = Some(duration(node, origin, "connect-timeout")?)
        }
        "read-timeout" => parameters.read_timeout = Some(duration(node, origin, "read-timeout")?),
        "request-timeout" => {
            parameters.request_timeout = Some(duration(node, origin, "request-timeout")?)
        }
        "idle-timeout" => parameters.idle_timeout = Some(duration(node, origin, "idle-timeout")?),
        "websocket" => parameters.websocket = Some(boolean(node, origin, "websocket")?),
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn references(node: &KdlNode, origin: &Origin) -> Result<Vec<Located<String>>> {
    shape(node, origin, "use", 1..=64, false)?;
    (0..node.entries().len())
        .map(|index| argument(node, index, origin, "use"))
        .collect()
}

pub fn unknown(node: &KdlNode, origin: &Origin) -> ConfigError {
    ConfigError::new(
        "CONFIG_UNKNOWN",
        node.name().value(),
        "unknown or unsupported configuration node",
        origin,
    )
}
