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

fn validate_chain_action(
    action: &mut crate::http_policy::model::Action,
    server: &super::model::Server,
    parameters: &super::model::Parameters,
    routes: &mut Vec<Route>,
) -> Result<()> {
    match action {
        crate::http_policy::model::Action::Route { raw, index } => {
            let action_origin = match &raw.action {
                RawAction::Proxy(v) => v.origin.clone(),
                RawAction::Root(v) => v.origin.clone(),
            };
            let route_action = match &raw.action {
                RawAction::Proxy(proxy) => {
                    Action::Proxy(address::upstream(&proxy.value, server, &proxy.origin)?)
                }
                RawAction::Root(root) => Action::Static {
                    root: root.clone(),
                    index: raw
                        .index
                        .as_ref()
                        .map_or_else(|| "index.html".to_owned(), |value| value.value.clone()),
                    spa: raw.spa.as_ref().is_some_and(|v| v.value),
                    cache_control: raw.cache_control.as_ref().map(|v| v.value.clone()),
                    precompressed: raw.precompressed.as_ref().is_some_and(|v| v.value),
                    browse: raw.browse.as_ref().is_some_and(|v| v.value),
                    error_pages: raw.error_pages.clone(),
                },
            };
            if let Action::Proxy(upstream) = &route_action {
                match raw.proxy_protocol {
                    super::model::UpstreamProtocol::H2c if upstream.tls => {
                        return Err(ConfigError::new(
                            "POLICY_UPSTREAM_PROTOCOL",
                            "protocol",
                            "h2c requires HTTP",
                            &action_origin,
                        ));
                    }
                    super::model::UpstreamProtocol::TlsH2 if !upstream.tls => {
                        return Err(ConfigError::new(
                            "POLICY_UPSTREAM_PROTOCOL",
                            "protocol",
                            "tls-h2 requires HTTPS",
                            &action_origin,
                        ));
                    }
                    _ => {}
                }
            }
            *index = Some(routes.len());
            routes.push(Route {
                proxy_protocol: raw.proxy_protocol,
                proxy_host: raw.proxy_host.as_ref().map(|h| h.value.clone()),
                pattern: raw.pattern.clone(),
                action_origin,
                action: route_action,
                parameters: parameters.clone(),
            });
        }
        crate::http_policy::model::Action::Branch(branches) => {
            for branch in branches {
                for action in &mut branch.actions {
                    validate_chain_action(action, server, parameters, routes)?;
                }
            }
        }
        crate::http_policy::model::Action::BasicAuth(_)
        | crate::http_policy::model::Action::StaticToken(_)
        | crate::http_policy::model::Action::Header(_)
        | crate::http_policy::model::Action::Rewrite(_)
        | crate::http_policy::model::Action::StripPrefix(_)
        | crate::http_policy::model::Action::TryFiles { .. }
        | crate::http_policy::model::Action::DynamicProxy(_)
        | crate::http_policy::model::Action::Continue
        | crate::http_policy::model::Action::Reenter
        | crate::http_policy::model::Action::Respond { .. } => (),
    }
    Ok(())
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
                    if route.spa.is_some()
                        || route.cache_control.is_some()
                        || route.precompressed.is_some()
                        || route.browse.is_some()
                        || !route.error_pages.is_empty()
                    {
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
                    index: "index.html".to_owned(),
                    spa: route.spa.is_some_and(|value| value.value),
                    cache_control: route.cache_control.map(|value| value.value),
                    precompressed: route.precompressed.is_some_and(|value| value.value),
                    browse: route.browse.is_some_and(|value| value.value),
                    error_pages: route.error_pages,
                },
            };
            routes.push(Route {
                proxy_protocol: super::model::UpstreamProtocol::Http1,
                proxy_host: None,
                pattern: route.pattern,
                action_origin,
                action,
                parameters: route.parameters,
            });
        }
        routes.sort_by(|left, right| left.pattern.value.cmp(&right.pattern.value));
        let mut chain = site.chain;
        if let Some(chain) = &mut chain {
            for rule in chain
                .rules
                .iter_mut()
                .chain(chain.errors.iter_mut().flatten())
            {
                for action in &mut rule.actions {
                    validate_chain_action(action, &server, &site.parameters, &mut routes)?;
                }
            }
        }
        sites.push(Site {
            chain,
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
