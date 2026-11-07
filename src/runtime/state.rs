use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::watch::resources::WatchResources;
use crate::config::dependencies::DependencyPlan;
use crate::config::model::ValidatedConfig;
use crate::config::prepare::{CheckedRoute, ConfigurationOnly, PreparedSnapshot, ResourcePreparer};
use crate::config::source::Origin;
use crate::config::{ConfigError, Result};
use crate::observability::logging::Logger;
use crate::proxy::upstream::PreparedPeer;
use crate::routing::Router;
use crate::static_files::PreparedStatic;
use crate::static_files::cache::{self, Binding, CachePool};
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
    pub(crate) activated_dav: Vec<crate::webdav::registry::ActivatedStorage>,
    dav_activation: Option<tokio::sync::OwnedSemaphorePermit>,
    pub(crate) _dynamic_trust: Option<Arc<crate::proxy::trust::store::TrustStore>>,
    pub policy: crate::http_policy::compile::PreparedPolicy,
    pub tls: PreparedTls,
    pub routes: Vec<Vec<PreparedRoute>>,
}

impl HttpResources {
    pub(crate) fn activate(&mut self) -> Result<()> {
        if !self.policy.dav_candidates.is_empty() {
            self.dav_activation = Some(crate::webdav::registry::activation_permit().map_err(
                |error| {
                    crate::config::ConfigError::new(
                        error.code,
                        "webdav",
                        "DAV activation is busy",
                        self.policy.dav_candidates[0].origin(),
                    )
                },
            )?);
        }
        for candidate in std::mem::take(&mut self.policy.dav_candidates) {
            if let Some(handle) = self
                .policy
                .dav_handles
                .iter()
                .find(|handle| handle.identity == *candidate.identity())
            {
                self.policy.dav_handles.push(handle.clone());
                continue;
            }
            let activated = candidate.activate()?;
            self.policy.dav_handles.push(activated.handle().clone());
            self.activated_dav.push(activated);
        }
        Ok(())
    }
}

pub struct HttpPreparer;

impl ResourcePreparer for HttpPreparer {
    type Resources = HttpResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<HttpResources> {
        self.prepare_inner(configuration, None)
    }

    fn prepare_tracked(
        &self,
        configuration: &ValidatedConfig,
        dependencies: &mut crate::config::dependencies::DependencyPlan,
    ) -> Result<HttpResources> {
        self.prepare_inner(configuration, Some(dependencies))
    }
}

impl HttpPreparer {
    fn prepare_inner(
        &self,
        configuration: &ValidatedConfig,
        dependencies: Option<&mut crate::config::dependencies::DependencyPlan>,
    ) -> Result<HttpResources> {
        let checked = match dependencies {
            Some(plan) => ConfigurationOnly.prepare_tracked(configuration, plan)?,
            None => ConfigurationOnly.prepare(configuration)?,
        };
        let trust = checked
            .trust
            .map(|material| {
                crate::proxy::trust::store::TrustStore::prepare(
                    material,
                    &checked.policy,
                    &configuration.server.origin,
                )
            })
            .transpose()?;
        let routes = checked
            .routes
            .into_iter()
            .zip(&configuration.sites)
            .map(|(routes, site)| {
                routes
                    .into_iter()
                    .zip(&site.routes)
                    .map(|(checked, route)| match checked {
                        CheckedRoute::Proxy => {
                            PreparedPeer::prepare(route, &configuration.server, trust.clone())
                                .map(|peer| PreparedRoute::Proxy(Box::new(peer)))
                        }
                        CheckedRoute::Static(prepared) => Ok(PreparedRoute::Static(prepared)),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(HttpResources {
            activated_dav: Vec::new(),
            dav_activation: None,
            _dynamic_trust: if checked.policy.dynamic_count > 0 {
                trust
            } else {
                None
            },
            policy: checked.policy,
            routes,
            tls: checked.tls,
        })
    }
}

fn bind_static_routes(
    resources: &mut HttpResources,
    cache: &Option<Arc<CachePool>>,
    namespace: u64,
    generation: u64,
    origin: &Origin,
) -> Result<()> {
    for (site, routes) in resources.routes.iter_mut().enumerate() {
        for (route_index, route) in routes.iter_mut().enumerate() {
            if let PreparedRoute::Static(route) = route {
                let prepared = Arc::get_mut(route).ok_or_else(|| {
                    ConfigError::new(
                        "INTERNAL",
                        "static-cache-bytes",
                        "static route is shared before publication",
                        origin,
                    )
                })?;
                prepared.cache = cache.as_ref().map(|pool| Binding {
                    pool: pool.clone(),
                    namespace,
                    generation,
                    route: (site, route_index),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct RuntimeSnapshot {
    pub(crate) _dynamic_trust: Option<Arc<crate::proxy::trust::store::TrustStore>>,
    pub policy: crate::http_policy::compile::PreparedPolicy,
    pub generation: u64,
    pub configuration: ValidatedConfig,
    pub router: Router,
    pub routes: Vec<Vec<PreparedRoute>>,
    pub tls: PreparedTls,
    pub logger: Logger,
    pub observation_routes: Vec<Vec<crate::observability::request::ObservationRoute>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StartupSettings {
    pub http: BTreeSet<SocketAddr>,
    pub https: BTreeSet<SocketAddr>,
    pub control_socket: PathBuf,
    pub watch: bool,
    pub static_cache_bytes: usize,
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
            watch: configuration.server.watch,
            static_cache_bytes: configuration.server.static_cache_bytes,
        }
    }
}

impl RuntimeSnapshot {
    fn from_prepared(
        prepared: PreparedSnapshot<HttpResources>,
        generation: u64,
        observability: &crate::observability::owner::Observability,
    ) -> Self {
        use crate::observability::{identity::Identity, request::ObservationRoute};
        let identities: Vec<_> = prepared
            .configuration
            .sites
            .iter()
            .flat_map(|site| {
                let name = site
                    .domains
                    .iter()
                    .map(|domain| domain.value.as_str())
                    .min()
                    .unwrap_or("");
                site.routes.iter().map(move |route| {
                    (
                        Identity::from_config(name, ""),
                        Identity::from_config(name, &route.pattern.value),
                    )
                })
            })
            .collect();
        let mut entries = identities
            .into_iter()
            .map(|(site, route)| ObservationRoute { site, route });
        let observation_routes = prepared
            .configuration
            .sites
            .iter()
            .map(|site| {
                (0..site.routes.len())
                    .filter_map(|_| entries.next())
                    .collect()
            })
            .collect();
        Self {
            generation,
            _dynamic_trust: prepared.resources._dynamic_trust,
            policy: prepared.resources.policy,
            observation_routes,
            router: Router::build(&prepared.configuration),
            logger: Logger::new(&prepared.configuration.server.log_level)
                .with_sink(observability.access.clone()),
            configuration: prepared.configuration,
            routes: prepared.resources.routes,
            tls: prepared.resources.tls,
        }
    }
}

#[derive(Debug, Default)]
struct Gate {
    closed: bool,
    watch: Option<WatchResources>,
    active_plan: Option<DependencyPlan>,
    failed_plan: Option<DependencyPlan>,
    dirty: bool,
    recovery: bool,
}

#[derive(Debug)]
pub struct RuntimeState {
    active: ArcSwap<RuntimeSnapshot>,
    pub observability: Arc<crate::observability::owner::Observability>,
    entry: PathBuf,
    startup: StartupSettings,
    cache: Option<Arc<CachePool>>,
    namespace: u64,
    closed: Mutex<Gate>,
    shutdown: watch::Sender<bool>,
}

pub(crate) struct StartupActivation {
    storages: Vec<crate::webdav::registry::ActivatedStorage>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl StartupActivation {
    pub(crate) fn commit(self) {
        for storage in self.storages {
            storage.commit();
        }
    }
}
impl RuntimeState {
    pub fn new(prepared: PreparedSnapshot<HttpResources>) -> Result<Arc<Self>> {
        let (state, activation) = Self::new_deferred(prepared)?;
        activation.commit();
        Ok(state)
    }
    pub(crate) fn new_deferred(
        mut prepared: PreparedSnapshot<HttpResources>,
    ) -> Result<(Arc<Self>, StartupActivation)> {
        let entry = prepared.configuration.entry.clone();
        let startup = StartupSettings::from_configuration(&prepared.configuration);
        let origin = Origin::file(&entry);
        let namespace = cache::next_namespace().ok_or_else(|| {
            ConfigError::new(
                "CONFIG_LIMIT",
                "static-cache-bytes",
                "cache namespace exhausted",
                &origin,
            )
        })?;
        let cache = cache::register(startup.static_cache_bytes).map_err(|reason| {
            ConfigError::new("RESOURCE_IO", "static-cache-bytes", reason, &origin)
        })?;
        bind_static_routes(&mut prepared.resources, &cache, namespace, 1, &origin)?;
        let observability = crate::observability::owner::Observability::new().map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "observability",
                "cannot initialize access logging",
                &origin,
            )
        })?;
        prepared.resources.activate()?;
        let activated = std::mem::take(&mut prepared.resources.activated_dav);
        let activation = prepared.resources.dav_activation.take();
        let snapshot = RuntimeSnapshot::from_prepared(prepared, 1, &observability);
        let (shutdown, _) = watch::channel(false);
        let state = Arc::new(Self {
            active: ArcSwap::from_pointee(snapshot),
            observability,
            entry,
            startup,
            cache,
            namespace,
            closed: Mutex::new(Gate::default()),
            shutdown,
        });
        Ok((
            state,
            StartupActivation {
                storages: activated,
                _permit: activation,
            },
        ))
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.active.load_full()
    }

    pub fn static_cache_stats(&self) -> Option<crate::static_files::StaticCacheStats> {
        self.cache.as_ref().map(|pool| pool.stats())
    }

    pub fn close(&self) {
        let mut closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        closed.closed = true;
        closed.watch.take();
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

    pub(crate) fn watch_enabled(&self) -> bool {
        self.startup.watch
    }

    pub(crate) fn set_initial_plan(&self, plan: DependencyPlan) {
        self.closed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_plan = Some(plan);
    }

    pub(crate) fn initialize_watch(
        &self,
        plan: DependencyPlan,
        watch: WatchResources,
    ) -> Result<()> {
        let mut gate = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if gate.closed {
            return Err(self.error("SHUTTING_DOWN", "runtime is shutting down"));
        }
        gate.active_plan = Some(plan);
        gate.watch = Some(watch);
        Ok(())
    }

    pub(crate) fn active_plan(&self) -> Option<DependencyPlan> {
        self.closed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_plan
            .clone()
    }

    pub(crate) fn poll_watch(&self) -> (bool, bool, Option<&'static str>) {
        let mut gate = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let outcome = gate
            .watch
            .as_mut()
            .map(WatchResources::drain)
            .unwrap_or_default();
        gate.dirty |= outcome.dirty;
        gate.recovery |= outcome.needs_rebuild;
        let dirty = std::mem::take(&mut gate.dirty);
        (dirty, gate.recovery, outcome.code)
    }

    pub(crate) fn install_failure(
        &self,
        plan: DependencyPlan,
        replacement: Option<WatchResources>,
        deadline: Instant,
    ) {
        let mut gate = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if gate.closed || Instant::now() >= deadline {
            return;
        }
        gate.failed_plan = Some(plan);
        if let Some(replacement) = replacement {
            let mut old = gate.watch.replace(replacement);
            if let Some(watch) = gate.watch.as_mut() {
                let outcome = watch.drain();
                gate.dirty |= outcome.dirty;
                gate.recovery = outcome.needs_rebuild;
            }
            if let Some(old) = old.as_mut() {
                let outcome = old.drain();
                gate.dirty |= outcome.dirty || outcome.needs_rebuild;
            }
        } else {
            gate.recovery = true;
        }
    }

    pub(crate) fn publish_tracked(
        &self,
        mut prepared: PreparedSnapshot<HttpResources>,
        plan: DependencyPlan,
        watch: WatchResources,
        deadline: Instant,
    ) -> Result<u64> {
        let startup = StartupSettings::from_configuration(&prepared.configuration);
        let mut gate = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if gate.closed {
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
        bind_static_routes(
            &mut prepared.resources,
            &self.cache,
            self.namespace,
            generation,
            &Origin::file(&self.entry),
        )?;
        let activated = std::mem::take(&mut prepared.resources.activated_dav);
        let activation = prepared.resources.dav_activation.take();
        let candidate = RuntimeSnapshot::from_prepared(prepared, generation, &self.observability);
        self.active.store(Arc::new(candidate));
        for storage in activated {
            storage.commit();
        }
        drop(activation);
        gate.active_plan = Some(plan);
        gate.failed_plan = None;
        let mut old = gate.watch.replace(watch);
        if let Some(watch) = gate.watch.as_mut() {
            let outcome = watch.drain();
            gate.dirty |= outcome.dirty;
            gate.recovery = outcome.needs_rebuild;
        }
        if let Some(old) = old.as_mut() {
            let outcome = old.drain();
            gate.dirty |= outcome.dirty || outcome.needs_rebuild;
        }
        Ok(generation)
    }

    pub(crate) fn publish(
        &self,
        mut prepared: PreparedSnapshot<HttpResources>,
        deadline: Instant,
    ) -> Result<u64> {
        let startup = StartupSettings::from_configuration(&prepared.configuration);
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if closed.closed {
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
        bind_static_routes(
            &mut prepared.resources,
            &self.cache,
            self.namespace,
            generation,
            &Origin::file(&self.entry),
        )?;
        let activated = std::mem::take(&mut prepared.resources.activated_dav);
        let activation = prepared.resources.dav_activation.take();
        let candidate = RuntimeSnapshot::from_prepared(prepared, generation, &self.observability);
        self.active.store(Arc::new(candidate));
        for storage in activated {
            storage.commit();
        }
        drop(activation);
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
        let state = RuntimeState::new(build(path, &HttpPreparer)?)?;
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
        let state = RuntimeState::new(build(path, &HttpPreparer)?)?;
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
        let state = RuntimeState::new(build(path, &HttpPreparer)?)?;
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
        let state = RuntimeState::new(build(path, &HttpPreparer)?)?;
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
        let state = RuntimeState::new(build(path, &HttpPreparer)?)?;
        state.active.store(Arc::new(RuntimeSnapshot::from_prepared(
            build(path, &HttpPreparer)?,
            u64::MAX,
            &state.observability,
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
