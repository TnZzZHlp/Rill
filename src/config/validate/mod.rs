mod address;

use std::collections::{BTreeMap, BTreeSet};

use super::error::{ConfigError, Result};
use super::model::{Action, NormalizedConfig, RawAction, Route, Site, ValidatedConfig};
use super::source::Origin;

pub use address::{check_ip, domain as normalize_domain, normalized_ip};

fn pattern(value: &str, origin: &Origin) -> Result<String> {
    let invalid = || {
        ConfigError::new(
            "CONFIG_TYPE",
            "route",
            "expected an absolute exact path or terminal /* prefix without query, fragment or invalid encoding",
            origin,
        )
    };
    if !value.starts_with('/')
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['?', '#', '\\'])
    {
        return Err(invalid());
    }
    let base = value.strip_suffix("/*").unwrap_or(value);
    if base.contains('*') {
        return Err(invalid());
    }
    let mut bytes = value.as_bytes().to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(invalid());
            }
            bytes[index + 1].make_ascii_uppercase();
            bytes[index + 2].make_ascii_uppercase();
            if &bytes[index + 1..index + 3] == b"00" || &bytes[index + 1..index + 3] == b"5C" {
                return Err(invalid());
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| invalid())
}

pub fn validate(normalized: NormalizedConfig) -> Result<ValidatedConfig> {
    let mut server = normalized.server;
    if server.http.is_empty() && server.https.is_empty() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "server",
            "at least one HTTP or HTTPS listener is required",
            &server.origin,
        ));
    }
    let listeners: Vec<_> = server.http.iter().chain(&server.https).collect();
    for (index, listener) in listeners.iter().enumerate() {
        for previous in &listeners[..index] {
            let a = listener.value;
            let b = previous.value;
            if a.port() == b.port()
                && a.is_ipv4() == b.is_ipv4()
                && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
            {
                return Err(ConfigError::new(
                    "DUPLICATE",
                    "listener",
                    "listener addresses overlap",
                    &listener.origin,
                )
                .related(&previous.origin));
            }
        }
    }
    let ports: BTreeSet<_> = server
        .https
        .iter()
        .map(|address| address.value.port())
        .collect();
    if server.redirect_https {
        if ports.is_empty() || (server.redirect_port.is_none() && ports.len() != 1) {
            return Err(ConfigError::new(
                "CONFIG_TYPE",
                "redirect-https",
                "redirect requires HTTPS and an unambiguous target port",
                &server.origin,
            ));
        }
        if let Some(port) = server.redirect_port {
            if !ports.contains(&port) {
                return Err(ConfigError::new(
                    "CONFIG_TYPE",
                    "redirect-port",
                    "redirect port must name a configured HTTPS listener",
                    &server.origin,
                ));
            }
        } else {
            server.redirect_port = ports.first().copied();
        }
    }
    if normalized.sites.is_empty() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "site",
            "at least one site is required",
            &server.origin,
        ));
    }
    let mut domains = BTreeMap::<String, Origin>::new();
    let mut sites = Vec::new();
    for site in normalized.sites {
        let mut site_domains = Vec::new();
        for mut name in site.domains {
            name.value = address::domain(&name.value, true).ok_or_else(|| {
                ConfigError::new(
                    "CONFIG_TYPE",
                    "site",
                    "invalid ASCII DNS name or single-label wildcard",
                    &name.origin,
                )
            })?;
            if let Some(first) = domains.insert(name.value.clone(), name.origin.clone()) {
                return Err(ConfigError::new(
                    "DUPLICATE",
                    "site",
                    "domain is already bound",
                    &name.origin,
                )
                .related(&first));
            }
            site_domains.push(name);
        }
        if let Some(reference) = &site.tls {
            if !normalized.certificates.contains_key(&reference.value) {
                return Err(ConfigError::new(
                    "TLS_INVALID",
                    "tls",
                    "certificate reference does not exist",
                    &reference.origin,
                ));
            }
        } else if server.redirect_https {
            return Err(ConfigError::new(
                "TLS_INVALID",
                "site",
                "redirect sites require a TLS reference",
                &site.origin,
            ));
        }
        let mut patterns = BTreeMap::<String, Origin>::new();
        let mut routes = Vec::new();
        for mut route in site.routes {
            route.pattern.value = pattern(&route.pattern.value, &route.pattern.origin)?;
            if let Some(first) =
                patterns.insert(route.pattern.value.clone(), route.pattern.origin.clone())
            {
                return Err(ConfigError::new(
                    "ROUTE_CONFLICT",
                    "route",
                    "route pattern conflicts with an existing or shorthand route",
                    &route.pattern.origin,
                )
                .related(&first));
            }
            let action_origin = match &route.action {
                RawAction::Proxy(value) => value.origin.clone(),
                RawAction::Root(value) => value.origin.clone(),
            };
            let action = match route.action {
                RawAction::Proxy(proxy) => {
                    if route.spa.is_some() || route.cache_control.is_some() {
                        return Err(ConfigError::new(
                            "CONFIG_TYPE",
                            "proxy",
                            "static response fields cannot be used with proxy",
                            &proxy.origin,
                        ));
                    }
                    Action::Proxy(address::upstream(&proxy.value, &server, &proxy.origin)?)
                }
                RawAction::Root(root) => Action::Static {
                    root,
                    spa: route.spa.is_some_and(|value| value.value),
                    cache_control: route.cache_control.map(|value| value.value),
                },
            };
            routes.push(Route {
                pattern: route.pattern,
                action_origin,
                action,
                parameters: route.parameters,
            });
        }
        routes.sort_by(|left, right| left.pattern.value.cmp(&right.pattern.value));
        sites.push(Site {
            domains: site_domains,
            tls: site.tls,
            routes,
            origin: site.origin,
        });
    }
    if !server.https.is_empty() && !sites.iter().any(|site| site.tls.is_some()) {
        return Err(ConfigError::new(
            "TLS_INVALID",
            "https",
            "HTTPS listener requires at least one TLS-enabled site",
            &server.origin,
        ));
    }
    Ok(ValidatedConfig {
        entry: normalized.entry,
        server,
        certificates: normalized.certificates,
        sites,
    })
}
