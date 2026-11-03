use std::path::Path;
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

use super::error::Result;
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

static STATIC_CAPACITY: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub struct ConfigurationOnly;

#[derive(Debug)]
pub enum CheckedRoute {
    Proxy,
    Static(Arc<crate::static_files::PreparedStatic>),
}

#[derive(Debug)]
pub struct CheckResources {
    pub tls: crate::tls::cert::PreparedTls,
    pub routes: Vec<Vec<CheckedRoute>>,
}

impl ResourcePreparer for ConfigurationOnly {
    type Resources = CheckResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources> {
        let tls = crate::tls::cert::PreparedTls::prepare(configuration)?;
        let capacity = STATIC_CAPACITY
            .get_or_init(|| Arc::new(Semaphore::new(128)))
            .clone();
        let routes = configuration
            .sites
            .iter()
            .map(|site| {
                site.routes
                    .iter()
                    .map(|route| match &route.action {
                        Action::Proxy(_) => Ok(CheckedRoute::Proxy),
                        Action::Static { .. } => Ok(CheckedRoute::Static(Arc::new(
                            crate::static_files::PreparedStatic::prepare(route, capacity.clone())?,
                        ))),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(CheckResources { tls, routes })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::source::{Located, Origin};

    #[tokio::test]
    async fn prepared_generations_share_static_io_limit()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let entry = Path::new("examples/config.kdl");
        let mut configuration = validate(normalize(parse(load(entry)?)?)?)?;
        configuration.sites[0].routes[0].action = Action::Static {
            root: Located::new(std::env::temp_dir(), Origin::file(entry)),
            spa: false,
            cache_control: None,
        };
        let old = ConfigurationOnly.prepare(&configuration)?;
        let CheckedRoute::Static(old) = &old.routes[0][0] else {
            panic!("static route expected");
        };
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            old.capacity.clone().acquire_many_owned(128),
        )
        .await??;
        let new = ConfigurationOnly.prepare(&configuration)?;
        let CheckedRoute::Static(new) = &new.routes[0][0] else {
            panic!("static route expected");
        };
        assert!(new.capacity.clone().try_acquire_owned().is_err());
        drop(permit);
        assert!(new.capacity.clone().try_acquire_owned().is_ok());
        Ok(())
    }
}
