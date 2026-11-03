use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::model::ValidatedConfig;
use crate::config::prepare::{CheckedRoute, ConfigurationOnly, PreparedSnapshot, ResourcePreparer};
use crate::config::source::Origin;
use crate::config::{ConfigError, Result};
use crate::observability::logging::Logger;
use crate::proxy::upstream::PreparedPeer;
use crate::routing::Router;
use crate::static_files::PreparedStatic;
use crate::tls::cert::PreparedTls;
use arc_swap::ArcSwap;
use tokio::sync::watch;

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

#[derive(Debug, PartialEq, Eq)]
pub struct StartupSettings {
    pub http: BTreeSet<SocketAddr>,
    pub https: BTreeSet<SocketAddr>,
    pub control_socket: PathBuf,
}

impl StartupSettings {
    fn from_configuration(configuration: &ValidatedConfig) -> Self {
        Self {
            http: configuration
                .server
                .http
                .iter()
                .map(|address| address.value)
                .collect(),
            https: configuration
                .server
                .https
                .iter()
                .map(|address| address.value)
                .collect(),
            control_socket: configuration.server.control_socket.clone(),
        }
    }
}

impl RuntimeSnapshot {
    fn from_prepared(prepared: PreparedSnapshot<HttpResources>, generation: u64) -> Self {
        Self {
            generation,
            router: Router::build(&prepared.configuration),
            logger: Logger::new(&prepared.configuration.server.log_level),
            configuration: prepared.configuration,
            routes: prepared.resources.routes,
            tls: prepared.resources.tls,
        }
    }
}

#[derive(Debug)]
pub struct RuntimeState {
    active: ArcSwap<RuntimeSnapshot>,
    entry: PathBuf,
    startup: StartupSettings,
    closed: Mutex<bool>,
    shutdown: watch::Sender<bool>,
}

impl RuntimeState {
    pub fn new(prepared: PreparedSnapshot<HttpResources>) -> Arc<Self> {
        let entry = prepared.configuration.entry.clone();
        let startup = StartupSettings::from_configuration(&prepared.configuration);
        let snapshot = RuntimeSnapshot::from_prepared(prepared, 1);
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self {
            active: ArcSwap::from_pointee(snapshot),
            entry,
            startup,
            closed: Mutex::new(false),
            shutdown,
        })
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.active.load_full()
    }

    pub fn close(&self) {
        let mut closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *closed = true;
        self.shutdown.send_replace(true);
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn entry(&self) -> &std::path::Path {
        &self.entry
    }

    pub(crate) fn error(&self, code: &'static str, reason: &'static str) -> ConfigError {
        ConfigError::new(code, "reload", reason, &Origin::file(&self.entry))
    }

    pub(crate) fn publish(
        &self,
        prepared: PreparedSnapshot<HttpResources>,
        deadline: Instant,
    ) -> Result<u64> {
        let startup = StartupSettings::from_configuration(&prepared.configuration);
        let mut candidate = RuntimeSnapshot::from_prepared(prepared, 0);
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *closed {
            return Err(self.error("SHUTTING_DOWN", "runtime is shutting down"));
        }
        if Instant::now() >= deadline {
            return Err(self.error("PREPARE_TIMEOUT", "reload preparation timed out"));
        }
        if startup != self.startup {
            return Err(self.error("RESTART_REQUIRED", "startup settings changed"));
        }
        let generation = self
            .active
            .load()
            .generation
            .checked_add(1)
            .ok_or_else(|| self.error("CONFIG_LIMIT", "generation limit exceeded"))?;
        candidate.generation = generation;
        self.active.store(Arc::new(candidate));
        Ok(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::prepare::build;
    use std::path::Path;

    #[test]
    fn publish_replaces_snapshot_and_close_rejects_candidate()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("examples/config.kdl");
        let state = RuntimeState::new(build(path, &HttpPreparer)?);
        let old = state.snapshot();
        let weak = Arc::downgrade(&old);
        let mut candidate = build(path, &HttpPreparer)?;
        candidate.configuration.server.log_level = "debug".into();
        assert_eq!(
            state.publish(candidate, Instant::now() + Duration::from_secs(30))?,
            2
        );
        assert_eq!(old.generation, 1);
        drop(old);
        assert!(weak.upgrade().is_none());
        state.close();
        let error = state
            .publish(
                build(path, &HttpPreparer)?,
                Instant::now() + Duration::from_secs(30),
            )
            .expect_err("closed state must reject publish");
        assert_eq!(error.code, "SHUTTING_DOWN");
        assert_eq!(state.snapshot().generation, 2);
        Ok(())
    }

    #[test]
    fn expired_deadline_rejects_publish() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("examples/config.kdl");
        let state = RuntimeState::new(build(path, &HttpPreparer)?);
        let old = state.snapshot();
        let error = state
            .publish(build(path, &HttpPreparer)?, Instant::now())
            .expect_err("expired deadline must reject publish");
        assert_eq!(error.code, "PREPARE_TIMEOUT");
        assert_eq!(state.snapshot().generation, 1);
        assert!(Arc::ptr_eq(&old, &state.snapshot()));
        Ok(())
    }

    #[test]
    fn changed_http_address_requires_restart() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let path = Path::new("examples/config.kdl");
        let state = RuntimeState::new(build(path, &HttpPreparer)?);
        let old = state.snapshot();
        let mut candidate = build(path, &HttpPreparer)?;
        candidate.configuration.server.http[0].value = "127.0.0.1:8081".parse()?;
        let error = state
            .publish(candidate, Instant::now() + Duration::from_secs(30))
            .expect_err("changed listener must require restart");
        assert_eq!(error.code, "RESTART_REQUIRED");
        assert_eq!(state.snapshot().generation, 1);
        assert!(Arc::ptr_eq(&old, &state.snapshot()));
        Ok(())
    }

    #[test]
    fn changed_control_socket_requires_restart()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("examples/config.kdl");
        let state = RuntimeState::new(build(path, &HttpPreparer)?);
        let old = state.snapshot();
        let mut candidate = build(path, &HttpPreparer)?;
        candidate
            .configuration
            .server
            .control_socket
            .push("different");
        let error = state
            .publish(candidate, Instant::now() + Duration::from_secs(30))
            .expect_err("changed control socket must require restart");
        assert_eq!(error.code, "RESTART_REQUIRED");
        assert_eq!(state.snapshot().generation, 1);
        assert!(Arc::ptr_eq(&old, &state.snapshot()));
        Ok(())
    }

    #[test]
    fn generation_overflow_rejects_publish() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let path = Path::new("examples/config.kdl");
        let state = RuntimeState::new(build(path, &HttpPreparer)?);
        state.active.store(Arc::new(RuntimeSnapshot::from_prepared(
            build(path, &HttpPreparer)?,
            u64::MAX,
        )));
        let old = state.snapshot();
        let error = state
            .publish(
                build(path, &HttpPreparer)?,
                Instant::now() + Duration::from_secs(30),
            )
            .expect_err("generation overflow must reject publish");
        assert_eq!(error.code, "CONFIG_LIMIT");
        assert_eq!(state.snapshot().generation, u64::MAX);
        assert!(Arc::ptr_eq(&old, &state.snapshot()));
        Ok(())
    }
}
