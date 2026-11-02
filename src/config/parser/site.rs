use std::collections::BTreeSet;

use kdl::KdlNode;

use super::values::{
    argument, at, boolean, once, parameter, path, references, shape, string, unknown,
};
use crate::config::error::{ConfigError, Result};
use crate::config::model::{Parameters, RawAction, RawRoute, RawSite};
use crate::config::source::{Located, Origin};

struct Scope {
    uses: Vec<Located<String>>,
    parameters: Parameters,
    action: Option<RawAction>,
    spa: Option<Located<bool>>,
    cache_control: Option<Located<String>>,
    tls: Option<Located<String>>,
    routes: Vec<RawRoute>,
}

fn scope(node: &KdlNode, origin: &Origin, site: bool) -> Result<Scope> {
    let mut result = Scope {
        uses: Vec::new(),
        parameters: Parameters::default(),
        action: None,
        spa: None,
        cache_control: None,
        tls: None,
        routes: Vec::new(),
    };
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        match child.name().value() {
            "use" => {
                result.uses.extend(references(child, &origin)?);
                continue;
            }
            "route" if site => {
                shape(child, &origin, "route", 1..=1, true)?;
                let pattern = argument(child, 0, &origin, "route")?;
                let route = scope(child, &origin, false)?;
                let action = route.action.ok_or_else(|| {
                    ConfigError::new(
                        "CONFIG_TYPE",
                        "route",
                        "route requires exactly one proxy or root action",
                        &origin,
                    )
                })?;
                result.routes.push(RawRoute {
                    pattern,
                    uses: route.uses,
                    parameters: route.parameters,
                    action,
                    spa: route.spa,
                    cache_control: route.cache_control,
                });
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
            "spa" => result.spa = Some(boolean(child, &origin, "spa")?),
            "cache-control" => {
                result.cache_control = Some(string(child, &origin, "cache-control")?)
            }
            _ => return Err(unknown(child, &origin)),
        }
    }
    Ok(result)
}

pub fn parse(node: &KdlNode, origin: &Origin) -> Result<RawSite> {
    shape(node, origin, "site", 1..=128, true)?;
    let domains = (0..node.entries().len())
        .map(|index| argument(node, index, origin, "site"))
        .collect::<Result<Vec<_>>>()?;
    let mut scope = scope(node, origin, true)?;
    if let Some(action) = scope.action {
        scope.routes.push(RawRoute {
            pattern: Located::new(
                "/*".into(),
                match &action {
                    RawAction::Proxy(value) => value.origin.clone(),
                    RawAction::Root(value) => value.origin.clone(),
                },
            ),
            uses: Vec::new(),
            parameters: Parameters::default(),
            action,
            spa: scope.spa,
            cache_control: scope.cache_control,
        });
    } else if scope.spa.is_some() || scope.cache_control.is_some() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "site",
            "static fields require a site-level root action",
            origin,
        ));
    }
    if scope.routes.is_empty() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "site",
            "site requires at least one route or shorthand action",
            origin,
        ));
    }
    Ok(RawSite {
        domains,
        tls: scope.tls,
        uses: scope.uses,
        parameters: scope.parameters,
        routes: scope.routes,
        origin: origin.clone(),
    })
}
