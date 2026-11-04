use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use kdl::KdlNode;

use super::values::{at, boolean, once, path, shape, string, unknown};
use crate::config::error::{ConfigError, Result};
use crate::config::model::{Network, Server};
use crate::config::source::{Located, Origin};

pub fn parse(node: &KdlNode, origin: &Origin) -> Result<Server> {
    shape(node, origin, "server", 0..=0, true)?;
    let mut result = Server {
        origin: origin.clone(),
        http: Vec::new(),
        https: Vec::new(),
        watch: false,
        static_cache_bytes: 0,
        observability: Default::default(),
        redirect_https: false,
        redirect_port: None,
        control_socket: "/run/rill/control.sock".into(),
        log_level: "info".into(),
        trusted_proxy: Vec::new(),
        upstream_allow: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        match child.name().value() {
            "http" | "https" => {
                let name = if child.name().value() == "http" {
                    "http"
                } else {
                    "https"
                };
                let value = string(child, &origin, name)?;
                let address = value
                    .value
                    .parse::<SocketAddr>()
                    .ok()
                    .filter(|address| address.port() != 0)
                    .ok_or_else(|| {
                        ConfigError::new(
                            "CONFIG_TYPE",
                            name,
                            "expected an IP socket address with nonzero port",
                            &origin,
                        )
                    })?;
                let addresses = if name == "http" {
                    &mut result.http
                } else {
                    &mut result.https
                };
                addresses.push(Located::new(address, origin));
            }
            "trusted-proxy" | "upstream-allow" => {
                let value = string(child, &origin, "network")?;
                let (address, prefix) = value.value.split_once('/').ok_or_else(|| {
                    ConfigError::new(
                        "CONFIG_TYPE",
                        "network",
                        "expected CIDR address/prefix",
                        &origin,
                    )
                })?;
                let address = address.parse::<IpAddr>().map_err(|_| {
                    ConfigError::new("CONFIG_TYPE", "network", "invalid CIDR address", &origin)
                })?;
                if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(ConfigError::new(
                        "CONFIG_TYPE",
                        "network",
                        "CIDR prefix must contain only decimal digits",
                        &origin,
                    ));
                }
                let prefix = prefix
                    .parse::<u8>()
                    .ok()
                    .filter(|prefix| *prefix <= if address.is_ipv4() { 32 } else { 128 })
                    .ok_or_else(|| {
                        ConfigError::new("CONFIG_TYPE", "network", "invalid CIDR prefix", &origin)
                    })?;
                let (address, prefix) = match address {
                    IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some() => {
                        let prefix = prefix.checked_sub(96).ok_or_else(|| {
                            ConfigError::new(
                                "CONFIG_TYPE",
                                "network",
                                "mapped IPv6 CIDR prefix must be at least 96",
                                &origin,
                            )
                        })?;
                        (
                            IpAddr::V4(ip.to_ipv4_mapped().ok_or_else(|| {
                                ConfigError::new(
                                    "CONFIG_TYPE",
                                    "network",
                                    "invalid mapped CIDR",
                                    &origin,
                                )
                            })?),
                            prefix,
                        )
                    }
                    address => (address, prefix),
                };
                let networks = if child.name().value() == "trusted-proxy" {
                    &mut result.trusted_proxy
                } else {
                    &mut result.upstream_allow
                };
                networks.push(Located::new(Network { address, prefix }, origin));
            }
            name => {
                once(&mut seen, child, &origin, "server")?;
                match name {
                    "observability" => {
                        result.observability = super::observability::parse(child, &origin)?;
                    }
                    "static-cache-bytes" => {
                        shape(child, &origin, "static-cache-bytes", 1..=1, false)?;
                        result.static_cache_bytes = child.entries()[0]
                            .value()
                            .as_integer()
                            .and_then(|value| usize::try_from(value).ok())
                            .filter(|value| *value <= 268_435_456)
                            .ok_or_else(|| {
                                ConfigError::new(
                                    "CONFIG_TYPE",
                                    "static-cache-bytes",
                                    "expected integer bytes from 0 to 268435456",
                                    &origin,
                                )
                            })?;
                    }
                    "watch" => result.watch = boolean(child, &origin, "watch")?.value,
                    "redirect-https" => {
                        result.redirect_https = boolean(child, &origin, "redirect-https")?.value
                    }
                    "redirect-port" => {
                        shape(child, &origin, "redirect-port", 1..=1, false)?;
                        let port = child.entries()[0]
                            .value()
                            .as_integer()
                            .and_then(|value| u16::try_from(value).ok())
                            .filter(|value| *value > 0)
                            .ok_or_else(|| {
                                ConfigError::new(
                                    "CONFIG_TYPE",
                                    "redirect-port",
                                    "expected integer port from 1 to 65535",
                                    &origin,
                                )
                            })?;
                        result.redirect_port = Some(port);
                    }
                    "control-socket" => {
                        result.control_socket = path(child, &origin, "control-socket")?.value
                    }
                    "log-level" => {
                        let level = string(child, &origin, "log-level")?;
                        if !matches!(
                            level.value.as_str(),
                            "trace" | "debug" | "info" | "warn" | "error" | "off"
                        ) {
                            return Err(ConfigError::new(
                                "CONFIG_TYPE",
                                "log-level",
                                "unsupported log level",
                                &origin,
                            ));
                        }
                        result.log_level = level.value;
                    }
                    _ => return Err(unknown(child, &origin)),
                }
            }
        }
    }
    Ok(result)
}
