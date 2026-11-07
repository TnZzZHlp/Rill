use std::collections::BTreeMap;
use std::mem::size_of;

use super::error::{ConfigError, Result};
use super::model::{ParsedConfig, RawAction, RawRoute, RouteItem, Snippet};
use super::source::{Located, Origin};

const MAX_DEPTH: usize = 64;
const MAX_ROUTES: usize = 4096;
const MAX_STEPS: usize = 16384;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Budget {
    steps: usize,
    routes: usize,
    bytes: usize,
}

impl Budget {
    fn step(&mut self, origin: &Origin) -> Result<()> {
        self.steps = self
            .steps
            .checked_add(1)
            .ok_or_else(|| step_limit(origin))?;
        if self.steps > MAX_STEPS {
            return Err(step_limit(origin));
        }
        Ok(())
    }
}

fn limit(origin: &Origin) -> ConfigError {
    ConfigError::new(
        "CONFIG_LIMIT",
        "use-snippet",
        "snippet expansion limit exceeded",
        origin,
    )
}

fn step_limit(origin: &Origin) -> ConfigError {
    ConfigError::new(
        "CONFIG_LIMIT",
        "use-snippet",
        "snippet step limit exceeded",
        origin,
    )
}

fn with_chain(mut error: ConfigError, chain: &[Origin]) -> ConfigError {
    add_chain(&mut error.origin, chain);
    error
}

fn check(
    name: &Located<String>,
    snippets: &BTreeMap<String, Snippet>,
    states: &mut BTreeMap<String, Option<Vec<Origin>>>,
    budget: &mut Budget,
    chain: &mut Vec<Origin>,
    depth: usize,
) -> Result<Vec<Origin>> {
    budget
        .step(&name.origin)
        .map_err(|e| with_chain(e, chain))?;
    if depth > MAX_DEPTH {
        return Err(with_chain(limit(&name.origin), chain));
    }
    if let Some(state) = states.get(&name.value) {
        return match state {
            None => Err(with_chain(
                ConfigError::new(
                    "SNIPPET_REF",
                    "use-snippet",
                    "cyclic snippet reference",
                    &name.origin,
                ),
                chain,
            )),
            Some(path) if depth + path.len() > MAX_DEPTH => {
                let index = MAX_DEPTH - depth;
                let mut full = chain.clone();
                full.push(name.origin.clone());
                full.extend(path[..index].iter().cloned());
                Err(with_chain(limit(&path[index]), &full))
            }
            Some(path) => Ok(path.clone()),
        };
    }
    let snippet = snippets.get(&name.value).ok_or_else(|| {
        with_chain(
            ConfigError::new(
                "SNIPPET_REF",
                "use-snippet",
                "snippet does not exist",
                &name.origin,
            ),
            chain,
        )
    })?;
    states.insert(name.value.clone(), None);
    let mut longest = Vec::new();
    for item in &snippet.items {
        budget
            .step(&snippet.name.origin)
            .map_err(|e| with_chain(e, chain))?;
        if let RouteItem::Snippet(reference) = item {
            if depth > 1 {
                chain.push(name.origin.clone());
            }
            let result = check(reference, snippets, states, budget, chain, depth + 1);
            if depth > 1 {
                chain.pop();
            }
            let path = result?;
            if path.len() + 1 > longest.len() {
                longest.clear();
                longest.push(reference.origin.clone());
                longest.extend(path);
            }
        }
    }
    states.insert(name.value.clone(), Some(longest.clone()));
    Ok(longest)
}

fn flat(origin: &Origin) -> Origin {
    Origin {
        source: origin.source.clone(),
        offset: origin.offset,
        length: origin.length,
        references: Vec::new(),
    }
}

fn origin_cost(origin: &Origin, chain: &[Origin]) -> Option<usize> {
    let mut seen = std::collections::BTreeSet::new();
    seen.insert((&origin.source.path, origin.offset, origin.length));
    let mut cost = size_of::<Origin>().checked_add(origin.source.path.as_os_str().len())?;
    for item in origin
        .references
        .iter()
        .chain(chain.iter().flat_map(|reference| {
            reference
                .references
                .iter()
                .chain(std::iter::once(reference))
        }))
    {
        if seen.insert((&item.source.path, item.offset, item.length)) {
            cost = cost
                .checked_add(size_of::<Origin>())?
                .checked_add(item.source.path.as_os_str().len())?;
        }
    }
    Some(cost)
}

fn located_cost<T>(value: &Located<T>, chain: &[Origin]) -> Option<usize> {
    origin_cost(&value.origin, chain)?.checked_add(size_of::<T>())
}

fn route_cost(route: &RawRoute, chain: &[Origin]) -> Option<usize> {
    let mut cost = size_of::<RawRoute>()
        .checked_sub(size_of::<Option<Located<bool>>>() * 2)?
        .checked_sub(size_of::<BTreeMap<u16, Located<std::path::PathBuf>>>())?;
    cost = cost
        .checked_add(located_cost(&route.pattern, chain)?)?
        .checked_add(route.pattern.value.len())?;
    for reference in &route.uses {
        cost = cost
            .checked_add(located_cost(reference, chain)?)?
            .checked_add(reference.value.len())?;
    }
    let p = &route.parameters;
    if let Some(v) = &p.connect_timeout {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    if let Some(v) = &p.read_timeout {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    if let Some(v) = &p.request_timeout {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    if let Some(v) = &p.idle_timeout {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    if let Some(v) = &p.websocket {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    cost = match &route.action {
        RawAction::Proxy(v) => cost
            .checked_add(located_cost(v, chain)?)?
            .checked_add(v.value.len())?,
        RawAction::Root(v) => cost
            .checked_add(located_cost(v, chain)?)?
            .checked_add(v.value.as_os_str().len())?,
    };
    if let Some(v) = &route.spa {
        cost = cost.checked_add(located_cost(v, chain)?)?;
    }
    if let Some(v) = &route.cache_control {
        cost = cost
            .checked_add(located_cost(v, chain)?)?
            .checked_add(v.value.len())?;
    }
    for value in [&route.precompressed, &route.browse].into_iter().flatten() {
        cost = cost.checked_add(located_cost(value, chain)?)?;
    }
    for value in route.error_pages.values() {
        cost = cost
            .checked_add(size_of::<u16>())?
            .checked_add(located_cost(value, chain)?)?
            .checked_add(value.value.as_os_str().len())?;
    }
    Some(cost)
}

pub(crate) fn add_chain(origin: &mut Origin, chain: &[Origin]) {
    let mut seen = std::collections::BTreeSet::new();
    seen.insert((origin.source.path.clone(), origin.offset, origin.length));
    origin.references.retain(|reference| {
        seen.insert((
            reference.source.path.clone(),
            reference.offset,
            reference.length,
        ))
    });
    for reference in chain {
        for item in reference
            .references
            .iter()
            .chain(std::iter::once(reference))
        {
            if seen.insert((item.source.path.clone(), item.offset, item.length)) {
                origin.references.push(flat(item));
            }
        }
    }
}

fn attach(route: &mut RawRoute, chain: &[Origin]) {
    add_chain(&mut route.pattern.origin, chain);
    for reference in &mut route.uses {
        add_chain(&mut reference.origin, chain);
    }
    let p = &mut route.parameters;
    if let Some(v) = &mut p.connect_timeout {
        add_chain(&mut v.origin, chain);
    }
    if let Some(v) = &mut p.read_timeout {
        add_chain(&mut v.origin, chain);
    }
    if let Some(v) = &mut p.request_timeout {
        add_chain(&mut v.origin, chain);
    }
    if let Some(v) = &mut p.idle_timeout {
        add_chain(&mut v.origin, chain);
    }
    if let Some(v) = &mut p.websocket {
        add_chain(&mut v.origin, chain);
    }
    match &mut route.action {
        RawAction::Proxy(v) => add_chain(&mut v.origin, chain),
        RawAction::Root(v) => add_chain(&mut v.origin, chain),
    }
    if let Some(v) = &mut route.spa {
        add_chain(&mut v.origin, chain);
    }
    if let Some(v) = &mut route.cache_control {
        add_chain(&mut v.origin, chain);
    }
    for value in [&mut route.precompressed, &mut route.browse]
        .into_iter()
        .flatten()
    {
        add_chain(&mut value.origin, chain);
    }
    for value in route.error_pages.values_mut() {
        add_chain(&mut value.origin, chain);
    }
}

fn expand_items(
    items: &[RouteItem],
    snippets: &BTreeMap<String, Snippet>,
    chain: &mut Vec<Origin>,
    output: &mut Vec<RawRoute>,
    budget: &mut Budget,
) -> Result<()> {
    for item in items {
        match item {
            RouteItem::Route(route) => {
                if !chain.is_empty() {
                    budget
                        .step(&route.pattern.origin)
                        .map_err(|e| with_chain(e, chain))?;
                    budget.routes = budget
                        .routes
                        .checked_add(1)
                        .ok_or_else(|| with_chain(limit(&route.pattern.origin), chain))?;
                    let cost = route_cost(route, chain)
                        .ok_or_else(|| with_chain(limit(&route.pattern.origin), chain))?;
                    budget.bytes = budget
                        .bytes
                        .checked_add(cost)
                        .ok_or_else(|| with_chain(limit(&route.pattern.origin), chain))?;
                    if budget.routes > MAX_ROUTES || budget.bytes > MAX_BYTES {
                        return Err(with_chain(limit(&route.pattern.origin), chain));
                    }
                }
                let mut route = (**route).clone();
                attach(&mut route, chain);
                output.push(route);
            }
            RouteItem::Snippet(reference) => {
                budget
                    .step(&reference.origin)
                    .map_err(|e| with_chain(e, chain))?;
                if chain.len() >= MAX_DEPTH {
                    return Err(with_chain(limit(&reference.origin), chain));
                }
                let snippet = snippets.get(&reference.value).ok_or_else(|| {
                    with_chain(
                        ConfigError::new(
                            "SNIPPET_REF",
                            "use-snippet",
                            "snippet does not exist",
                            &reference.origin,
                        ),
                        chain,
                    )
                })?;
                chain.push(reference.origin.clone());
                expand_items(&snippet.items, snippets, chain, output, budget)?;
                chain.pop();
            }
        }
    }
    Ok(())
}

pub fn expand(parsed: &mut ParsedConfig) -> Result<()> {
    let mut budget = Budget::default();
    let mut states = BTreeMap::new();
    for snippet in parsed.snippets.values() {
        check(
            &snippet.name,
            &parsed.snippets,
            &mut states,
            &mut budget,
            &mut Vec::new(),
            1,
        )?;
    }
    for site in &mut parsed.sites {
        expand_items(
            &site.items,
            &parsed.snippets,
            &mut Vec::new(),
            &mut site.routes,
            &mut budget,
        )?;
        site.items.clear();
        if site.routes.is_empty() && site.chain.is_none() {
            return Err(ConfigError::new(
                "CONFIG_TYPE",
                "site",
                "site requires at least one route or shorthand action",
                &site.origin,
            ));
        }
    }
    Ok(())
}
