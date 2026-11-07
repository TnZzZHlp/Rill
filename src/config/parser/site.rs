use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kdl::KdlNode;

use super::values::{
    argument, at, boolean, once, parameter, path, references, shape, string, unknown,
};
use crate::config::error::{ConfigError, Result};
use crate::config::model::{Parameters, RawAction, RawRoute, RawSite, RouteItem};
use crate::config::source::{Located, Origin};

struct Scope {
    uses: Vec<Located<String>>,
    parameters: Parameters,
    action: Option<RawAction>,
    spa: Option<Located<bool>>,
    cache_control: Option<Located<String>>,
    precompressed: Option<Located<bool>>,
    browse: Option<Located<bool>>,
    error_pages: BTreeMap<u16, Located<PathBuf>>,
    tls: Option<Located<String>>,
    items: Vec<RouteItem>,
    chain: Option<crate::http_policy::model::Chain>,
    site_label: Option<Located<String>>,
    access_log: Option<crate::observability::file_config::LogConfig>,
}

fn scope(
    node: &KdlNode,
    origin: &Origin,
    site: bool,
    snippets: &mut super::chain_snippets::Snippets<'_>,
) -> Result<Scope> {
    let mut result = Scope {
        uses: Vec::new(),
        parameters: Parameters::default(),
        action: None,
        spa: None,
        cache_control: None,
        precompressed: None,
        browse: None,
        error_pages: BTreeMap::new(),
        tls: None,
        items: Vec::new(),
        chain: None,
        site_label: None,
        access_log: None,
    };
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        match child.name().value() {
            "site-label" if site => {
                if result.site_label.is_some() {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "site-label",
                        "duplicate site label",
                        &origin,
                    ));
                }
                result.site_label = Some(crate::observability::file_config::parse_site_label(
                    child, &origin,
                )?);
                continue;
            }
            "access-log" if site => {
                if result.access_log.is_some() {
                    return Err(ConfigError::new(
                        "LOG_CAPACITY",
                        "access-log",
                        "duplicate site log",
                        &origin,
                    ));
                }
                result.access_log = Some(crate::observability::file_config::parse_access_log(
                    child, &origin,
                )?);
                continue;
            }
            "chain" if site => {
                if result.chain.is_some() {
                    return Err(ConfigError::new(
                        "POLICY_INVALID_CHAIN",
                        "chain",
                        "duplicate chain",
                        &origin,
                    ));
                }
                result.chain = Some(super::chain::parse(child, &origin, snippets)?);
                continue;
            }
            "error-page" => {
                shape(child, &origin, "error-page", 2..=2, false)?;
                let status = child.entries()[0]
                    .value()
                    .as_integer()
                    .and_then(|value| u16::try_from(value).ok())
                    .filter(|value| matches!(value, 403 | 404 | 405 | 406 | 416 | 500 | 503))
                    .ok_or_else(|| {
                        ConfigError::new("CONFIG_TYPE", "error-page", "unsupported status", &origin)
                    })?;
                let path = argument(child, 1, &origin, "error-page")?;
                let path = Located::new(origin.relative_path(&path.value), origin.clone());
                if let Some(first) = result.error_pages.insert(status, path) {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "error-page",
                        "status is repeated",
                        &origin,
                    )
                    .related(&first.origin));
                }
                continue;
            }
            "use" => {
                result.uses.extend(references(child, &origin)?);
                continue;
            }
            "route" if site => {
                result
                    .items
                    .push(RouteItem::Route(Box::new(route(child, &origin)?)));
                continue;
            }
            "use-snippet" if site => {
                result
                    .items
                    .push(RouteItem::Snippet(string(child, &origin, "use-snippet")?));
                continue;
            }
            _ => {}
        }
        once(&mut seen, child, &origin, "site/route")?;
        if parameter(child, &origin, &mut result.parameters)? {
            continue;
        }
        match child.name().value() {
            "tls" if site => result.tls = Some(string(child, &origin, "tls")?),
            "proxy" | "root" => {
                if result.action.is_some() {
                    return Err(ConfigError::new(
                        "CONFIG_TYPE",
                        "action",
                        "proxy and root actions are mutually exclusive",
                        &origin,
                    ));
                }
                result.action = Some(if child.name().value() == "proxy" {
                    RawAction::Proxy(string(child, &origin, "proxy")?)
                } else {
                    RawAction::Root(path(child, &origin, "root")?)
                });
            }
            "precompressed" => {
                result.precompressed = Some(boolean(child, &origin, "precompressed")?)
            }
            "browse" => result.browse = Some(boolean(child, &origin, "browse")?),
            "spa" => result.spa = Some(boolean(child, &origin, "spa")?),
            "cache-control" => {
                result.cache_control = Some(string(child, &origin, "cache-control")?)
            }
            _ => return Err(unknown(child, &origin)),
        }
    }
    Ok(result)
}

pub fn route(node: &KdlNode, origin: &Origin) -> Result<RawRoute> {
    shape(node, origin, "route", 1..=1, true)?;
    let pattern = argument(node, 0, origin, "route")?;
    let scope = scope(
        node,
        origin,
        false,
        &mut super::chain_snippets::Snippets::default(),
    )?;
    let action = scope.action.ok_or_else(|| {
        ConfigError::new(
            "CONFIG_TYPE",
            "route",
            "route requires exactly one proxy or root action",
            origin,
        )
    })?;
    Ok(RawRoute {
        pattern,
        index: None,
        proxy_host: None,
        proxy_protocol: Default::default(),
        uses: scope.uses,
        parameters: scope.parameters,
        action,
        spa: scope.spa,
        cache_control: scope.cache_control,
        precompressed: scope.precompressed,
        browse: scope.browse,
        error_pages: scope.error_pages,
    })
}

pub fn parse(
    node: &KdlNode,
    origin: &Origin,
    snippets: &mut super::chain_snippets::Snippets<'_>,
) -> Result<RawSite> {
    shape(node, origin, "site", 1..=128, true)?;
    let domains = (0..node.entries().len())
        .map(|index| argument(node, index, origin, "site"))
        .collect::<Result<Vec<_>>>()?;
    let mut chain_origin = None;
    let mut legacy_origin = None;
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        match child.name().value() {
            "chain" => chain_origin = Some(at(child, origin)),
            "route" | "use-snippet" | "proxy" | "root" => legacy_origin = Some(at(child, origin)),
            _ => (),
        }
    }
    if let (Some(chain), Some(legacy)) = (chain_origin, legacy_origin) {
        return Err(ConfigError::new(
            "POLICY_MODE_CONFLICT",
            "site",
            "chain and legacy routes are mutually exclusive",
            &chain,
        )
        .related(&legacy));
    }
    let mut scope = scope(node, origin, true, snippets)?;
    if scope.chain.is_some() && (scope.action.is_some() || !scope.items.is_empty()) {
        return Err(ConfigError::new(
            "POLICY_MODE_CONFLICT",
            "site",
            "chain and legacy routes are mutually exclusive",
            origin,
        ));
    }
    if let Some(action) = scope.action {
        scope.items.push(RouteItem::Route(Box::new(RawRoute {
            pattern: Located::new(
                "/*".into(),
                match &action {
                    RawAction::Proxy(value) => value.origin.clone(),
                    RawAction::Root(value) => value.origin.clone(),
                },
            ),
            index: None,
            proxy_host: None,
            proxy_protocol: Default::default(),
            uses: Vec::new(),
            parameters: Parameters::default(),
            action,
            spa: scope.spa,
            cache_control: scope.cache_control,
            precompressed: scope.precompressed,
            browse: scope.browse,
            error_pages: scope.error_pages,
        })));
    } else if scope.spa.is_some()
        || scope.cache_control.is_some()
        || scope.precompressed.is_some()
        || scope.browse.is_some()
        || !scope.error_pages.is_empty()
    {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "site",
            "static fields require a site-level root action",
            origin,
        ));
    }
    if scope.items.is_empty() && scope.chain.is_none() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "site",
            "site requires at least one route or shorthand action",
            origin,
        ));
    }
    Ok(RawSite {
        domains,
        site_label: scope.site_label,
        access_log: scope.access_log,
        chain: scope.chain,
        tls: scope.tls,
        uses: scope.uses,
        parameters: scope.parameters,
        routes: Vec::new(),
        items: scope.items,
        origin: origin.clone(),
    })
}
