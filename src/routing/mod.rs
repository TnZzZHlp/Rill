use std::collections::BTreeMap;

use crate::config::model::ValidatedConfig;

#[derive(Debug)]
struct Paths {
    exact: BTreeMap<String, usize>,
    prefixes: Vec<(String, usize)>,
}

#[derive(Debug)]
pub struct Router {
    exact: BTreeMap<String, usize>,
    wildcard: BTreeMap<String, usize>,
    paths: Vec<Paths>,
}

impl Router {
    pub fn build(configuration: &ValidatedConfig) -> Self {
        let mut result = Self {
            exact: BTreeMap::new(),
            wildcard: BTreeMap::new(),
            paths: Vec::new(),
        };
        for (site_index, site) in configuration.sites.iter().enumerate() {
            for domain in &site.domains {
                if let Some(suffix) = domain.value.strip_prefix("*.") {
                    result.wildcard.insert(suffix.to_string(), site_index);
                } else {
                    result.exact.insert(domain.value.clone(), site_index);
                }
            }
            let mut paths = Paths {
                exact: BTreeMap::new(),
                prefixes: Vec::new(),
            };
            for (route_index, route) in site.routes.iter().enumerate() {
                if let Some(prefix) = route.pattern.value.strip_suffix('*') {
                    paths.prefixes.push((prefix.to_string(), route_index));
                } else {
                    paths.exact.insert(route.pattern.value.clone(), route_index);
                }
            }
            paths
                .prefixes
                .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
            result.paths.push(paths);
        }
        result
    }

    pub fn find(&self, host: &str, path: &str) -> Option<(usize, usize)> {
        let site = self.exact.get(host).copied().or_else(|| {
            let (label, suffix) = host.split_once('.')?;
            if label.is_empty() {
                None
            } else {
                self.wildcard.get(suffix).copied()
            }
        })?;
        let paths = self.paths.get(site)?;
        let route = paths.exact.get(path).copied().or_else(|| {
            paths
                .prefixes
                .iter()
                .find(|(prefix, _)| path.starts_with(prefix))
                .map(|(_, index)| *index)
        })?;
        Some((site, route))
    }
}
