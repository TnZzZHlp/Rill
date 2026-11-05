use std::path::Path;
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

use super::dependencies::DependencyPlan;
use super::error::{ConfigError, Result};
use super::model::{Action, ValidatedConfig};
use super::{load_tracked, normalize, parse, validate};

#[derive(Debug)]
pub struct PreparedSnapshot<R> {
    pub configuration: ValidatedConfig,
    pub resources: R,
}

pub trait ResourcePreparer {
    type Resources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources>;

    fn prepare_tracked(
        &self,
        configuration: &ValidatedConfig,
        _dependencies: &mut DependencyPlan,
    ) -> Result<Self::Resources> {
        if !configuration.certificates.is_empty() {
            return Err(ConfigError::new(
                "INTERNAL",
                "watch",
                "resource preparer does not support tracked TLS",
                &configuration.server.origin,
            ));
        }
        self.prepare(configuration)
    }
}

static STATIC_CAPACITY: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(crate) fn static_capacity() -> Arc<Semaphore> {
    STATIC_CAPACITY
        .get_or_init(|| Arc::new(Semaphore::new(128)))
        .clone()
}

pub struct ConfigurationOnly;

#[derive(Debug)]
pub enum CheckedRoute {
    Proxy,
    Static(Arc<crate::static_files::PreparedStatic>),
}

#[derive(Debug)]
pub struct CheckResources {
    pub policy: crate::http_policy::compile::PreparedPolicy,
    pub(crate) trust: Option<crate::proxy::trust::material::TrustMaterial>,
    pub tls: crate::tls::cert::PreparedTls,
    pub routes: Vec<Vec<CheckedRoute>>,
}

impl ResourcePreparer for ConfigurationOnly {
    type Resources = CheckResources;

    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources> {
        self.prepare_inner(configuration, None)
    }

    fn prepare_tracked(
        &self,
        configuration: &ValidatedConfig,
        dependencies: &mut DependencyPlan,
    ) -> Result<Self::Resources> {
        self.prepare_inner(configuration, Some(dependencies))
    }
}

impl ConfigurationOnly {
    fn prepare_inner(
        &self,
        configuration: &ValidatedConfig,
        mut dependencies: Option<&mut DependencyPlan>,
    ) -> Result<CheckResources> {
        crate::proxy::pool_policy::registry()
            .collect_expired()
            .map_err(|_| {
                ConfigError::new(
                    "POLICY_LIMIT",
                    "proxy",
                    "cannot collect upstream pool resources",
                    &configuration.server.origin,
                )
            })?;
        let policy =
            crate::http_policy::compile::compile(configuration, dependencies.as_deref_mut())?;
        let has_tls_upstream = configuration
            .sites
            .iter()
            .flat_map(|site| &site.routes)
            .any(|route| matches!(&route.action, Action::Proxy(upstream) if upstream.tls));
        let trust = if has_tls_upstream {
            Some(crate::proxy::trust::material::TrustMaterial::prepare(
                &policy,
                &configuration.server.origin,
            )?)
        } else {
            None
        };
        let tls = match dependencies {
            Some(plan) => crate::tls::cert::PreparedTls::prepare_tracked(configuration, plan)?,
            None => crate::tls::cert::PreparedTls::prepare(configuration)?,
        };
        let capacity = static_capacity();
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
        Ok(CheckResources {
            policy,
            trust,
            tls,
            routes,
        })
    }
}

#[derive(Debug)]
pub struct TrackedSnapshot<R> {
    pub prepared: PreparedSnapshot<R>,
    pub dependencies: DependencyPlan,
}

#[derive(Debug)]
pub struct BuildFailure {
    pub error: ConfigError,
    pub dependencies: DependencyPlan,
}

pub fn build<P: ResourcePreparer>(
    entry: &Path,
    preparer: &P,
) -> Result<PreparedSnapshot<P::Resources>> {
    build_inner(entry, preparer, false)
        .map(|tracked| tracked.prepared)
        .map_err(|failure| failure.error)
}

pub fn build_for_run<P: ResourcePreparer>(
    entry: &Path,
    preparer: &P,
) -> Result<(PreparedSnapshot<P::Resources>, Option<DependencyPlan>)> {
    let configuration = validate(normalize(parse(super::load(entry)?)?)?)?;
    if !configuration.server.watch {
        let resources = preparer.prepare(&configuration)?;
        return Ok((
            PreparedSnapshot {
                configuration,
                resources,
            },
            None,
        ));
    }
    let tracked = build_tracked(entry, preparer).map_err(|failure| failure.error)?;
    let plan = tracked
        .prepared
        .configuration
        .server
        .watch
        .then_some(tracked.dependencies);
    Ok((tracked.prepared, plan))
}

pub fn build_tracked<P: ResourcePreparer>(
    entry: &Path,
    preparer: &P,
) -> std::result::Result<TrackedSnapshot<P::Resources>, Box<BuildFailure>> {
    build_inner(entry, preparer, true)
}

fn build_inner<P: ResourcePreparer>(
    entry: &Path,
    preparer: &P,
    tracked_resources: bool,
) -> std::result::Result<TrackedSnapshot<P::Resources>, Box<BuildFailure>> {
    let mut dependencies = if tracked_resources {
        DependencyPlan::default()
    } else {
        DependencyPlan::disabled()
    };
    let result = (|| {
        let loaded = load_tracked(entry, &mut dependencies)?;
        let parsed = parse(loaded)?;
        let configuration = validate(normalize(parsed)?)?;
        let resources = if tracked_resources {
            preparer.prepare_tracked(&configuration, &mut dependencies)?
        } else {
            preparer.prepare(&configuration)?
        };
        if tracked_resources && !dependencies.unchanged() {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                "watch",
                "dependency changed during preparation",
                &super::source::Origin::file(entry),
            ));
        }
        Ok(PreparedSnapshot {
            configuration,
            resources,
        })
    })();
    match result {
        Ok(prepared) => Ok(TrackedSnapshot {
            prepared,
            dependencies,
        }),
        Err(error) => Err(Box::new(BuildFailure {
            error,
            dependencies,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;
    use crate::config::source::{Located, Origin};

    #[tokio::test]
    async fn prepared_generations_share_static_io_limit()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let entry = Path::new("examples/config.kdl");
        let mut configuration = validate(normalize(parse(load(entry)?)?)?)?;
        configuration.sites[0].routes[0].action = Action::Static {
            root: Located::new(std::env::temp_dir(), Origin::file(entry)),
            index: "index.html".to_owned(),
            spa: false,
            cache_control: None,
            precompressed: false,
            browse: false,
            error_pages: Default::default(),
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
