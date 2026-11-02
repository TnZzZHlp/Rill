use std::path::Path;

use crate::config::prepare::{ConfigurationOnly, build};

pub fn check(path: &Path) -> u8 {
    match build(path, &ConfigurationOnly) {
        Ok(snapshot) => {
            println!(
                "configuration valid: {} sites, {} routes (scope: configuration and literal upstream policy; DNS, connectivity, TLS and static resources are not validated)",
                snapshot.configuration.sites.len(),
                snapshot
                    .configuration
                    .sites
                    .iter()
                    .map(|site| site.routes.len())
                    .sum::<usize>()
            );
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
