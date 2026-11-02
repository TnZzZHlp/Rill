use std::sync::Arc;

use crate::config::Result;
use crate::config::model::ValidatedConfig;
use crate::config::prepare::{ConfigurationOnly, PreparedSnapshot, ResourcePreparer};
use crate::observability::logging::Logger;
use crate::proxy::upstream::PreparedPeer;
use crate::routing::Router;
use crate::tls::cert::PreparedTls;
use arc_swap::ArcSwap;

#[derive(Debug)]
pub struct HttpResources {
    pub tls: PreparedTls,
    pub peers: Vec<Vec<PreparedPeer>>,
}

pub struct HttpPreparer;

impl ResourcePreparer for HttpPreparer {
    type Resources = HttpResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<HttpResources> {
        let tls = ConfigurationOnly.prepare(configuration)?;
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
        Ok(HttpResources { peers, tls })
    }
}

#[derive(Debug)]
pub struct RuntimeSnapshot {
    pub generation: u64,
    pub configuration: ValidatedConfig,
    pub router: Router,
    pub peers: Vec<Vec<PreparedPeer>>,
    pub tls: PreparedTls,
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
            tls: prepared.resources.tls,
        };
        Arc::new(Self {
            active: ArcSwap::from_pointee(snapshot),
        })
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.active.load_full()
    }
}
