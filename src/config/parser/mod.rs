mod observability;
mod server;
mod site;
mod values;

use std::collections::{BTreeMap, BTreeSet};

use kdl::KdlNode;

use super::error::{ConfigError, Result};
use super::loader::LoadedConfig;
use super::model::{Certificate, Parameters, ParsedConfig, Profile, RouteItem, Snippet};
use super::source::Origin;
use values::{argument, at, once, parameter, path, references, shape, unknown};

fn profile(node: &KdlNode, origin: &Origin) -> Result<Profile> {
    shape(node, origin, "profile", 1..=1, true)?;
    let mut result = Profile {
        name: argument(node, 0, origin, "profile")?,
        uses: Vec::new(),
        parameters: Parameters::default(),
    };
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        if child.name().value() == "use" {
            result.uses.extend(references(child, &origin)?);
        } else {
            once(&mut seen, child, &origin, "profile")?;
            if !parameter(child, &origin, &mut result.parameters)? {
                return Err(unknown(child, &origin));
            }
        }
    }
    Ok(result)
}

fn snippet(node: &KdlNode, origin: &Origin) -> Result<Snippet> {
    shape(node, origin, "snippet", 1..=1, true)?;
    let name = argument(node, 0, origin, "snippet")?;
    let mut items = Vec::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        items.push(match child.name().value() {
            "route" => RouteItem::Route(Box::new(site::route(child, &origin)?)),
            "use-snippet" => RouteItem::Snippet(values::string(child, &origin, "use-snippet")?),
            _ => return Err(unknown(child, &origin)),
        });
    }
    if items.is_empty() {
        return Err(ConfigError::new(
            "CONFIG_TYPE",
            "snippet",
            "snippet must not be empty",
            origin,
        ));
    }
    Ok(Snippet { name, items })
}

fn certificate(node: &KdlNode, origin: &Origin) -> Result<Certificate> {
    shape(node, origin, "tls", 1..=1, true)?;
    let name = argument(node, 0, origin, "tls")?;
    let mut cert = None;
    let mut key = None;
    let mut seen = BTreeSet::new();
    for child in node
        .children()
        .into_iter()
        .flat_map(|document| document.nodes())
    {
        let origin = at(child, origin);
        once(&mut seen, child, &origin, "tls")?;
        match child.name().value() {
            "cert" => cert = Some(path(child, &origin, "cert")?),
            "key" => key = Some(path(child, &origin, "key")?),
            _ => return Err(unknown(child, &origin)),
        }
    }
    Ok(Certificate {
        name,
        cert: cert.ok_or_else(|| {
            ConfigError::new("CONFIG_TYPE", "tls", "certificate path is required", origin)
        })?,
        key: key.ok_or_else(|| {
            ConfigError::new("CONFIG_TYPE", "tls", "private key path is required", origin)
        })?,
    })
}

pub fn parse(loaded: LoadedConfig) -> Result<ParsedConfig> {
    let mut result = ParsedConfig {
        entry: loaded.entry,
        server: None,
        profiles: BTreeMap::new(),
        snippets: BTreeMap::new(),
        certificates: BTreeMap::new(),
        sites: Vec::new(),
    };
    for located in loaded.nodes {
        let node = &located.node;
        let origin = &located.origin;
        match node.name().value() {
            "server" => {
                if let Some(first) = &result.server {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "server",
                        "only one server block is allowed",
                        origin,
                    )
                    .related(&first.origin));
                }
                result.server = Some(server::parse(node, origin)?);
            }
            "site" => result.sites.push(site::parse(node, origin)?),
            "profile" => {
                let profile = profile(node, origin)?;
                if let Some(first) = result.profiles.get(&profile.name.value) {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "profile",
                        "profile name already defined",
                        origin,
                    )
                    .related(&first.name.origin));
                }
                result.profiles.insert(profile.name.value.clone(), profile);
            }
            "snippet" => {
                let snippet = snippet(node, origin)?;
                if let Some(first) = result.snippets.get(&snippet.name.value) {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "snippet",
                        "snippet name already defined",
                        origin,
                    )
                    .related(&first.name.origin));
                }
                result.snippets.insert(snippet.name.value.clone(), snippet);
            }
            "tls" => {
                let certificate = certificate(node, origin)?;
                if let Some(first) = result.certificates.get(&certificate.name.value) {
                    return Err(ConfigError::new(
                        "DUPLICATE",
                        "tls",
                        "certificate name already defined",
                        origin,
                    )
                    .related(&first.name.origin));
                }
                result
                    .certificates
                    .insert(certificate.name.value.clone(), certificate);
            }
            _ => return Err(unknown(node, origin)),
        }
    }
    Ok(result)
}
