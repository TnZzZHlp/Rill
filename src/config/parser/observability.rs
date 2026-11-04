use std::collections::BTreeSet;
use std::net::SocketAddr;

use kdl::KdlNode;

use super::values::{at, boolean, once, shape, string, unknown};
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

fn loopback_address(value: &str) -> Option<SocketAddr> {
    value
        .parse::<SocketAddr>()
        .ok()
        .filter(|address| address.ip().is_loopback() && address.port() != 0)
}

fn endpoint(value: &str) -> bool {
    if value.len() > 256
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    value
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix("/v1/traces"))
        .and_then(loopback_address)
        .is_some()
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
            "metrics" => {
                let value = string(child, &origin, "metrics")?;
                result.startup.metrics = Some(loopback_address(&value.value).ok_or_else(|| {
                    ConfigError::new(
                        "CONFIG_TYPE",
                        "metrics",
                        "expected a loopback IP socket address with nonzero port",
                        &origin,
                    )
                })?);
            }
            "tracing" => result.policy.tracing = boolean(child, &origin, "tracing")?.value,
            "trace-sample-per-mille" => {
                result.policy.trace_sample_per_mille =
                    sample(child, &origin, "trace-sample-per-mille")?
            }
            "otlp-endpoint" => {
                let value = string(child, &origin, "otlp-endpoint")?;
                if !endpoint(&value.value) {
                    return Err(ConfigError::new(
                        "CONFIG_TYPE",
                        "otlp-endpoint",
                        "expected a loopback HTTP endpoint with nonzero port and /v1/traces path",
                        &origin,
                    ));
                }
                result.startup.otlp_endpoint = value.value;
            }
            _ => return Err(unknown(child, &origin)),
        }
    }
    Ok(result)
}
