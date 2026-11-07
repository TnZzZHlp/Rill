use std::collections::BTreeMap;

use kdl::KdlNode;

use super::values::{at, shape, string, unknown};
use crate::config::error::{ConfigError, Result};
use crate::config::source::{Located, Origin};

pub(super) struct DomainBindings {
    pub domains: Vec<Located<String>>,
    pub certificates: BTreeMap<String, Located<String>>,
    pub origin: Origin,
}

fn certificate(node: &KdlNode, origin: &Origin) -> Result<Located<String>> {
    let mut reference: Option<Located<String>> = None;
    for child in super::chain::children(node) {
        let location = at(child, origin);
        if child.name().value() != "tls" {
            return Err(unknown(child, &location));
        }
        if let Some(first) = &reference {
            return Err(ConfigError::new(
                "DUPLICATE",
                "tls",
                "duplicate domain certificate binding",
                &location,
            )
            .related(&first.origin));
        }
        reference = Some(string(child, &location, "tls")?);
    }
    reference.ok_or_else(|| {
        ConfigError::new(
            "CONFIG_TYPE",
            "tls",
            "domain object requires a TLS reference",
            origin,
        )
    })
}

pub(super) fn parse(node: &KdlNode, origin: &Origin) -> Result<DomainBindings> {
    shape(node, origin, "domains", 0..=0, true)?;
    let mut result = DomainBindings {
        domains: Vec::new(),
        certificates: BTreeMap::new(),
        origin: origin.clone(),
    };
    for child in super::chain::children(node) {
        let location = at(child, origin);
        if result.domains.len() >= 128 {
            return Err(ConfigError::new(
                "CONFIG_LIMIT",
                "domains",
                "a site may declare at most 128 domains",
                &location,
            ));
        }
        shape(child, &location, "domains", 0..=0, true)?;
        let domain =
            crate::config::normalize_domain(child.name().value(), true).ok_or_else(|| {
                ConfigError::new(
                    "CONFIG_TYPE",
                    "domains",
                    "invalid ASCII DNS name or single-label wildcard",
                    &location,
                )
            })?;
        let certificate = certificate(child, &location)?;
        if let Some(first) = result.certificates.insert(domain.clone(), certificate) {
            return Err(ConfigError::new(
                "DUPLICATE",
                "domains",
                "domain is already bound",
                &location,
            )
            .related(&first.origin));
        }
        result.domains.push(Located::new(domain, location));
    }
    if result.domains.is_empty() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "domains",
            "domain mapping must not be empty",
            origin,
        ));
    }
    Ok(result)
}
