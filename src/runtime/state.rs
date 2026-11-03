use std::sync::Arc;
use std::time::Duration;

use crate::config::Result;
use crate::config::model::ValidatedConfig;
use crate::config::prepare::{CheckedRoute, ConfigurationOnly, PreparedSnapshot, ResourcePreparer};
use crate::observability::logging::Logger;
use crate::proxy::upstream::PreparedPeer;
use crate::routing::Router;
use crate::static_files::PreparedStatic;
use crate::tls::cert::PreparedTls;
use arc_swap::ArcSwap;

#[derive(Debug)]
pub enum PreparedRoute {
    Proxy(Box<PreparedPeer>),
    Static(Arc<PreparedStatic>),
}

impl PreparedRoute {
    pub fn request(&self) -> Duration {
        match self {
            Self::Proxy(peer) => peer.request,
            Self::Static(route) => route.request,
        }
    }
    pub fn idle(&self) -> Duration {
        match self {
            Self::Proxy(peer) => peer.idle,
            Self::Static(route) => route.idle,
        }
    }
    pub fn as_proxy(&self) -> Option<&PreparedPeer> {
        match self {
            Self::Proxy(peer) => Some(peer),
            Self::Static(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct HttpResources {
    pub tls: PreparedTls,
    pub routes: Vec<Vec<PreparedRoute>>,
}

pub struct HttpPreparer;

impl ResourcePreparer for HttpPreparer {
    type Resources = HttpResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<HttpResources> {
        let checked = ConfigurationOnly.prepare(configuration)?;
        let routes = checked
            .routes
            .into_iter()
            .zip(&configuration.sites)
            .map(|(routes, site)| {
                routes
                    .into_iter()
                    .zip(&site.routes)
                    .map(|(checked, route)| match checked {
                        CheckedRoute::Proxy => PreparedPeer::prepare(route, &configuration.server)
                            .map(|peer| PreparedRoute::Proxy(Box::new(peer))),
                        CheckedRoute::Static(prepared) => Ok(PreparedRoute::Static(prepared)),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(HttpResources {
            routes,
            tls: checked.tls,
        })
    }
}

#[derive(Debug)]
pub struct RuntimeSnapshot {
    pub generation: u64,
    pub configuration: ValidatedConfig,
    pub router: Router,
    pub routes: Vec<Vec<PreparedRoute>>,
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
            routes: prepared.resources.routes,
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
