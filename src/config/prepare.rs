use std::path::Path;
use std::sync::Arc;

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
        let capacity = Arc::new(Semaphore::new(128));
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
