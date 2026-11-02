use std::collections::BTreeMap;
use std::time::Duration;

use super::error::{ConfigError, Result};
use super::model::{NormalizedConfig, Parameters, ParsedConfig, Profile};
use super::source::{Located, Origin};

fn referenced<T: Clone>(value: &Located<T>, reference: Option<&Origin>) -> Located<T> {
    let mut value = value.clone();
    if let Some(reference) = reference {
        value.origin.references.extend(reference.references.clone());
        let mut reference = reference.clone();
        reference.references.clear();
        value.origin.references.push(reference);
    }
    value
}

fn overlay(target: &mut Parameters, source: &Parameters, reference: Option<&Origin>) {
    if let Some(value) = &source.connect_timeout {
        target.connect_timeout = Some(referenced(value, reference));
    }
    if let Some(value) = &source.read_timeout {
        target.read_timeout = Some(referenced(value, reference));
    }
    if let Some(value) = &source.request_timeout {
        target.request_timeout = Some(referenced(value, reference));
    }
    if let Some(value) = &source.idle_timeout {
        target.idle_timeout = Some(referenced(value, reference));
    }
    if let Some(value) = &source.websocket {
        target.websocket = Some(referenced(value, reference));
    }
}

fn resolve(
    name: &Located<String>,
    profiles: &BTreeMap<String, Profile>,
    cache: &mut BTreeMap<String, Parameters>,
    stack: &mut Vec<String>,
) -> Result<Parameters> {
    if stack.contains(&name.value) {
        return Err(ConfigError::new(
            "PROFILE_REF",
            "use",
            "cyclic profile reference",
            &name.origin,
        ));
    }
    if let Some(parameters) = cache.get(&name.value) {
        return Ok(parameters.clone());
    }
    if stack.len() >= 64 {
        return Err(ConfigError::new(
            "CONFIG_LIMIT",
            "use",
            "profile nesting exceeds limit",
            &name.origin,
        ));
    }
    let profile = profiles.get(&name.value).ok_or_else(|| {
        ConfigError::new("PROFILE_REF", "use", "profile does not exist", &name.origin)
    })?;
    stack.push(name.value.clone());
    let mut parameters = Parameters::default();
    for reference in &profile.uses {
        let defaults = resolve(reference, profiles, cache, stack)?;
        overlay(&mut parameters, &defaults, Some(&reference.origin));
    }
    overlay(&mut parameters, &profile.parameters, None);
    stack.pop();
    cache.insert(name.value.clone(), parameters.clone());
    Ok(parameters)
}

fn apply(
    parameters: &mut Parameters,
    uses: &[Located<String>],
    profiles: &BTreeMap<String, Parameters>,
) -> Result<()> {
    for reference in uses {
        let defaults = profiles.get(&reference.value).ok_or_else(|| {
            ConfigError::new(
                "PROFILE_REF",
                "use",
                "profile does not exist",
                &reference.origin,
            )
        })?;
        overlay(parameters, defaults, Some(&reference.origin));
    }
    Ok(())
}

fn defaults(origin: &Origin) -> Parameters {
    Parameters {
        connect_timeout: Some(Located::new(Duration::from_secs(5), origin.clone())),
        read_timeout: Some(Located::new(Duration::from_secs(60), origin.clone())),
        request_timeout: Some(Located::new(Duration::from_secs(300), origin.clone())),
        idle_timeout: Some(Located::new(Duration::from_secs(60), origin.clone())),
        websocket: Some(Located::new(true, origin.clone())),
    }
}

pub fn normalize(mut parsed: ParsedConfig) -> Result<NormalizedConfig> {
    let server = parsed.server.ok_or_else(|| {
        ConfigError::new(
            "CONFIG_TYPE",
            "server",
            "one explicit server block is required",
            &Origin::file(&parsed.entry),
        )
    })?;
    let mut profiles = BTreeMap::new();
    for profile in parsed.profiles.values() {
        resolve(
            &profile.name,
            &parsed.profiles,
            &mut profiles,
            &mut Vec::new(),
        )?;
    }
    for site in &mut parsed.sites {
        let mut parameters = defaults(&site.origin);
        apply(&mut parameters, &site.uses, &profiles)?;
        overlay(&mut parameters, &site.parameters, None);
        for route in &mut site.routes {
            let mut effective = parameters.clone();
            apply(&mut effective, &route.uses, &profiles)?;
            overlay(&mut effective, &route.parameters, None);
            route.parameters = effective;
            route.uses.clear();
        }
        site.parameters = parameters;
        site.uses.clear();
    }
    Ok(NormalizedConfig {
        entry: parsed.entry,
        server,
        certificates: parsed.certificates,
        sites: parsed.sites,
    })
}
