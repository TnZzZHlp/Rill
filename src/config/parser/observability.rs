use kdl::KdlNode;
use std::collections::BTreeSet;

use super::values::{at, boolean, once, shape, unknown};
use crate::config::error::{ConfigError, Result};
use crate::config::model::ObservabilityConfig;
use crate::config::source::Origin;

fn sample(node: &KdlNode, origin: &Origin, name: &'static str) -> Result<u16> {
    shape(node, origin, name, 1..=1, false)?;
    node.entries()[0]
        .value()
        .as_integer()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value <= 1000)
        .ok_or_else(|| {
            ConfigError::new(
                "CONFIG_TYPE",
                name,
                "expected integer from 0 to 1000",
                origin,
            )
        })
}

pub fn parse(node: &KdlNode, origin: &Origin) -> Result<ObservabilityConfig> {
    shape(node, origin, "observability", 0..=0, true)?;
    let mut result = ObservabilityConfig::default();
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        once(&mut seen, child, &origin, "observability")?;
        match child.name().value() {
            "access-log" => result.policy.access_log = boolean(child, &origin, "access-log")?.value,
            "access-sample-per-mille" => {
                result.policy.access_sample_per_mille =
                    sample(child, &origin, "access-sample-per-mille")?
            }
            _ => return Err(unknown(child, &origin)),
        }
    }
    Ok(result)
}
