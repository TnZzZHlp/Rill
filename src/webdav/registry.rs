use super::{
    locks::Locks,
    model::{DavError, Result, StorageIdentity, WebDav},
    namespace,
    properties::{self, ActivatedProperties, Properties},
};
use crate::{
    http_policy::compile::{ExternalBudget, PreparedPolicy},
    static_files::{PreparedStatic, root::Root},
};
use std::{
    collections::BTreeMap,
    fs::File,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

const CANDIDATE_LIMIT: usize = 8 * 1024 * 1024;
#[derive(Debug)]
pub struct StorageHandle {
    pub identity: StorageIdentity,
    pub root: Arc<Root>,
    pub read: Arc<PreparedStatic>,
    pub namespace: Arc<tokio::sync::RwLock<()>>,
    pub locks: Mutex<Locks>,
    pub properties: Arc<Properties>,
    _budget: ExternalBudget,
}
struct Entry {
    handle: Arc<StorageHandle>,
    published: bool,
}
#[derive(Default)]
struct RegistryInner {
    entries: Mutex<BTreeMap<String, Entry>>,
    activation: Mutex<()>,
}
#[derive(Clone, Default)]
pub struct StorageRegistry {
    inner: Arc<RegistryInner>,
}
impl std::fmt::Debug for StorageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageRegistry").finish_non_exhaustive()
    }
}
static REGISTRY: OnceLock<Arc<StorageRegistry>> = OnceLock::new();
static ACTIVATION: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
pub(crate) fn activation_permit() -> Result<tokio::sync::OwnedSemaphorePermit> {
    ACTIVATION
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| DavError::new(503, "BUSY"))
}
pub fn registry() -> Arc<StorageRegistry> {
    REGISTRY
        .get_or_init(|| Arc::new(StorageRegistry::default()))
        .clone()
}
#[derive(Debug)]
pub struct StorageCandidate {
    registry: StorageRegistry,
    identity: StorageIdentity,
    root: Arc<Root>,
    read: Arc<PreparedStatic>,
    budget: ExternalBudget,
    metadata_bytes: usize,
    limit: usize,
}
#[derive(Debug)]
pub(crate) struct PreparedStorage {
    candidate: StorageCandidate,
    origin: crate::config::source::Origin,
}
impl StorageRegistry {
    pub fn collect_expired(&self) {
        let entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in entries.values() {
            if let Ok(mut locks) = entry.handle.locks.try_lock() {
                locks.expire();
            }
        }
    }
    fn compatible(
        &self,
        identity: &StorageIdentity,
        root: &Root,
        published: bool,
    ) -> Result<Option<Arc<StorageHandle>>> {
        let entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, entry) in entries.iter() {
            if id == identity.storage_id.as_ref() {
                if entry.handle.identity != *identity {
                    return Err(DavError::new(409, "RESTART_REQUIRED"));
                }
                if published && !entry.published {
                    return Err(DavError::new(503, "BUSY"));
                }
                return Ok(Some(entry.handle.clone()));
            }
            if namespace::overlaps(&entry.handle.root, root)? {
                return Err(DavError::new(403, "DAV_ROOT_OVERLAP"));
            }
        }
        Ok(None)
    }
    pub fn prepare(&self, identity: StorageIdentity, root: Arc<File>) -> Result<StorageCandidate> {
        if identity.storage_id.is_empty()
            || namespace::identity(&root)? != (identity.device, identity.inode)
        {
            return Err(DavError::new(409, "RESOURCE_CHANGED"));
        }
        let root = Arc::new(Root::from_prepared_fd(root.try_clone()?)?);
        namespace::check_writable_directory(root.capability())?;
        self.compatible(&identity, &root, false)?;
        let mut budget = ExternalBudget::default();
        let base = 4096_usize
            .checked_add(
                identity
                    .storage_id
                    .len()
                    .checked_mul(4)
                    .ok_or_else(DavError::limit)?,
            )
            .filter(|n| *n <= CANDIDATE_LIMIT)
            .ok_or_else(DavError::limit)?;
        if !budget.reserve_process(base) {
            return Err(DavError::limit());
        }
        let metadata_bytes = scan(&identity, &root, CANDIDATE_LIMIT - base)?;
        let read = Arc::new(PreparedStatic {
            root: Root::from_prepared_fd(root.capability().try_clone()?)?,
            index: "index.html".into(),
            cache: None,
            spa: false,
            precompressed: false,
            browse: false,
            error_pages: BTreeMap::new(),
            cache_control: None,
            capacity: crate::config::prepare::static_capacity(),
            request: Duration::from_secs(300),
            idle: Duration::from_secs(60),
        });
        Ok(StorageCandidate {
            registry: self.clone(),
            identity,
            root,
            read,
            budget,
            metadata_bytes,
            limit: CANDIDATE_LIMIT,
        })
    }
    pub(crate) fn prepare_config(
        &self,
        config: &WebDav,
        policy: &PreparedPolicy,
    ) -> crate::config::Result<PreparedStorage> {
        let root = Root::prepare(&config.root)?;
        let (device, inode) = namespace::identity(root.capability())
            .map_err(|error| config_error(error, &config.origin))?;
        let identity = StorageIdentity {
            storage_id: config.storage_id.clone().into_boxed_str(),
            device,
            inode,
        };
        let file = Arc::new(
            root.capability()
                .try_clone()
                .map_err(|error| config_error(error.into(), &config.origin))?,
        );
        let mut candidate = self
            .prepare(identity, file)
            .map_err(|error| config_error(error, &config.origin))?;
        candidate.limit = candidate.required();
        if candidate.required() > policy.remaining_bytes() {
            return Err(config_error(DavError::limit(), &config.origin));
        }
        Ok(PreparedStorage {
            candidate,
            origin: config.origin.clone(),
        })
    }
}
fn scan(identity: &StorageIdentity, root: &Root, limit: usize) -> Result<usize> {
    let mut memory = super::budget::Memory::default();
    let mut total = 0_usize;
    properties::validate(identity, root, |bytes| {
        total = total
            .checked_add(bytes)
            .filter(|n| *n <= limit)
            .ok_or_else(DavError::limit)?;
        memory.reserve(bytes)
    })?;
    Ok(total)
}
impl StorageCandidate {
    fn required(&self) -> usize {
        self.budget.bytes().saturating_add(self.metadata_bytes)
    }
    pub fn activate(self) -> Result<ActivatedStorage> {
        let registry = self.registry;
        let gate = registry.inner.clone();
        let _activation = gate
            .activation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = registry.compatible(&self.identity, &self.root, true)? {
            return Ok(ActivatedStorage {
                registry,
                handle,
                properties: None,
                inserted: false,
                committed: false,
            });
        }
        scan(
            &self.identity,
            &self.root,
            self.limit
                .checked_sub(self.budget.bytes())
                .ok_or_else(DavError::limit)?,
        )?;
        super::private_file::probe(&self.root)?;
        let properties = ActivatedProperties::activate(&self.identity, self.root.clone())?;
        let handle = Arc::new(StorageHandle {
            identity: self.identity,
            root: self.root,
            read: self.read,
            namespace: Arc::new(tokio::sync::RwLock::new(())),
            locks: Mutex::new(Locks::default()),
            properties: properties.properties.clone(),
            _budget: self.budget,
        });
        registry
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                handle.identity.storage_id.to_string(),
                Entry {
                    handle: handle.clone(),
                    published: false,
                },
            );
        Ok(ActivatedStorage {
            registry,
            handle,
            properties: Some(properties),
            inserted: true,
            committed: false,
        })
    }
}
impl PreparedStorage {
    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.candidate.identity
    }
    pub(crate) fn root(&self) -> &Root {
        &self.candidate.root
    }
    pub(crate) fn origin(&self) -> &crate::config::source::Origin {
        &self.origin
    }
    pub(crate) fn required(&self) -> usize {
        self.candidate.required()
    }
    pub(crate) fn limit(&mut self, limit: usize) -> crate::config::Result<()> {
        if self.required() > limit {
            return Err(config_error(DavError::limit(), &self.origin));
        }
        self.candidate.limit = self.required();
        Ok(())
    }
    pub(crate) fn activate(self) -> crate::config::Result<ActivatedStorage> {
        self.candidate
            .activate()
            .map_err(|error| config_error(error, &self.origin))
    }
}
#[derive(Debug)]
pub struct ActivatedStorage {
    registry: StorageRegistry,
    handle: Arc<StorageHandle>,
    properties: Option<ActivatedProperties>,
    inserted: bool,
    committed: bool,
}
impl ActivatedStorage {
    pub fn handle(&self) -> &Arc<StorageHandle> {
        &self.handle
    }
    pub fn commit(mut self) {
        self.committed = true;
        if let Some(entry) = self
            .registry
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(self.handle.identity.storage_id.as_ref())
        {
            entry.published = true;
        }
        if let Some(properties) = self.properties.take() {
            properties.commit();
        }
    }
}
impl Drop for ActivatedStorage {
    fn drop(&mut self) {
        if self.inserted && !self.committed {
            let mut entries = self
                .registry
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entries
                .get(self.handle.identity.storage_id.as_ref())
                .is_some_and(|entry| !entry.published && Arc::ptr_eq(&entry.handle, &self.handle))
            {
                entries.remove(self.handle.identity.storage_id.as_ref());
            }
        }
    }
}
fn config_error(
    error: DavError,
    origin: &crate::config::source::Origin,
) -> crate::config::ConfigError {
    crate::config::ConfigError::new(
        error.code,
        "webdav",
        "cannot securely prepare DAV storage",
        origin,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepared_fd_candidate_keeps_its_registry_owner_and_rolls_back_only_unpublished_entries()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "rill-dav-registry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&path)?;
        let file = Arc::new(File::open(&path)?);
        let (device, inode) = namespace::identity(&file)?;
        let identity = StorageIdentity {
            storage_id: "owner-test".into(),
            device,
            inode,
        };
        let registry = StorageRegistry::default();
        let independent = StorageRegistry::default();
        let first = registry.prepare(identity.clone(), file.clone())?;
        assert_eq!(std::fs::read_dir(&path)?.count(), 0);
        let mut wrong = identity.clone();
        wrong.inode = wrong.inode.checked_add(1).ok_or("inode limit")?;
        assert!(registry.prepare(wrong, file.clone()).is_err());
        let duplicate = registry.prepare(identity.clone(), file.clone())?;
        let activated = first.activate()?;
        assert!(Arc::ptr_eq(&activated.registry.inner, &registry.inner));
        assert_eq!(
            registry.inner.entries.lock().map_err(|_| "poisoned")?.len(),
            1
        );
        assert!(
            independent
                .inner
                .entries
                .lock()
                .map_err(|_| "poisoned")?
                .is_empty()
        );
        assert_eq!(duplicate.activate().unwrap_err().code, "BUSY");
        drop(activated);
        assert!(!path.join(".rill-dav").exists());
        assert!(
            registry
                .inner
                .entries
                .lock()
                .map_err(|_| "poisoned")?
                .is_empty()
        );
        let activated = registry
            .prepare(identity.clone(), file.clone())?
            .activate()?;
        let handle = activated.handle().clone();
        activated.commit();
        let reused = registry.prepare(identity, file)?.activate()?;
        assert!(Arc::ptr_eq(&handle, reused.handle()));
        drop(reused);
        assert!(path.join(".rill-dav/.owner").exists());
        drop(handle);
        drop(registry);
        drop(independent);
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
