use std::sync::Arc;

use crate::config::model::ValidatedConfig;
use crate::config::prepare::{ConfigurationOnly, PreparedSnapshot, ResourcePreparer};
use crate::config::{ConfigError, Result};
use crate::observability::logging::Logger;
use crate::proxy::upstream::PreparedPeer;
use crate::routing::Router;
use arc_swap::ArcSwap;

#[derive(Debug)]
pub struct HttpResources {
    pub peers: Vec<Vec<PreparedPeer>>,
}

pub struct HttpPreparer;

impl ResourcePreparer for HttpPreparer {
    type Resources = HttpResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<HttpResources> {
        ConfigurationOnly.prepare(configuration)?;
        if !configuration.server.https.is_empty() || configuration.server.redirect_https {
            return Err(ConfigError::new(
                "CONFIG_UNSUPPORTED",
                "https",
                "HTTPS service is unavailable until stage 004",
                &configuration.server.origin,
            ));
        }
        let peers = configuration
            .sites
            .iter()
            .map(|site| {
                site.routes
                    .iter()
                    .map(|route| PreparedPeer::prepare(route, &configuration.server))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(HttpResources { peers })
    }
}

#[derive(Debug)]
pub struct RuntimeSnapshot {
    pub generation: u64,
    pub configuration: ValidatedConfig,
    pub router: Router,
    pub peers: Vec<Vec<PreparedPeer>>,
    pub logger: Logger,
}

#[derive(Debug)]
pub struct RuntimeState {
    active: ArcSwap<RuntimeSnapshot>,
}

impl RuntimeState {
    pub fn new(prepared: PreparedSnapshot<HttpResources>) -> Arc<Self> {
        let snapshot = RuntimeSnapshot {
            generation: 1,
            router: Router::build(&prepared.configuration),
            logger: Logger::new(&prepared.configuration.server.log_level),
            configuration: prepared.configuration,
            peers: prepared.resources.peers,
        };
        Arc::new(Self {
            active: ArcSwap::from_pointee(snapshot),
        })
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.active.load_full()
    }
}
