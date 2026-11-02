use std::path::Path;

use super::error::{ConfigError, Result};
use super::model::{Action, ValidatedConfig};
use super::{load, normalize, parse, validate};

#[derive(Debug)]
pub struct PreparedSnapshot<R> {
    pub configuration: ValidatedConfig,
    pub resources: R,
}

pub trait ResourcePreparer {
    type Resources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources>;
}

pub struct ConfigurationOnly;

impl ResourcePreparer for ConfigurationOnly {
    type Resources = crate::tls::cert::PreparedTls;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources> {
        let tls = crate::tls::cert::PreparedTls::prepare(configuration)?;
        for site in &configuration.sites {
            for route in &site.routes {
                if let Action::Static { root, .. } = &route.action {
                    return Err(ConfigError::new(
                        "CONFIG_UNSUPPORTED",
                        "root",
                        "secure static root preparation is unavailable until stage 005; check cannot report success",
                        &root.origin,
                    ));
                }
            }
        }
        Ok(tls)
    }
}

pub fn build<P: ResourcePreparer>(
    entry: &Path,
    preparer: &P,
) -> Result<PreparedSnapshot<P::Resources>> {
    let configuration = validate(normalize(parse(load(entry)?)?)?)?;
    let resources = preparer.prepare(&configuration)?;
    Ok(PreparedSnapshot {
        configuration,
        resources,
    })
}
