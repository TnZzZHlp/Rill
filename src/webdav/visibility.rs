use super::{
    budget::Memory,
    model::{DavError, Result},
};
use std::{
    collections::BTreeMap,
    fs::File,
    os::unix::fs::MetadataExt,
    sync::{
        Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

type Identity = (u64, u64);
pub(crate) struct Lease {
    file: File,
    references: AtomicUsize,
    published: AtomicBool,
    _memory: Memory,
    disk: Mutex<Option<super::limits::DiskBudget>>,
}
pub(crate) const fn entry_bytes() -> usize {
    512 + std::mem::size_of::<Lease>()
}
pub(crate) type Entries = BTreeMap<Identity, Arc<Lease>>;
static PRIVATE: OnceLock<RwLock<Entries>> = OnceLock::new();
static CURSOR: Mutex<Option<Identity>> = Mutex::new(None);
fn registry() -> &'static RwLock<Entries> {
    PRIVATE.get_or_init(RwLock::default)
}
fn collect(entries: &mut Entries) {
    let mut cursor = CURSOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut retired = [None; 64];
    let mut count = 0;
    let mut last = None;
    for (identity, lease) in entries
        .range((
            cursor.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
            std::ops::Bound::Unbounded,
        ))
        .take(64)
    {
        last = Some(*identity);
        if lease.references.load(Ordering::Acquire) == 0
            && (lease.published.load(Ordering::Acquire)
                || lease
                    .file
                    .metadata()
                    .is_ok_and(|metadata| metadata.nlink() == 0))
        {
            retired[count] = Some(*identity);
            count += 1;
        }
    }
    *cursor = last;
    for identity in retired.into_iter().flatten() {
        entries.remove(&identity);
    }
}
pub(crate) fn read() -> RwLockReadGuard<'static, Entries> {
    if let Ok(mut entries) = registry().try_write() {
        collect(&mut entries);
    }
    registry()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub(crate) fn write() -> RwLockWriteGuard<'static, Entries> {
    let mut entries = registry()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    collect(&mut entries);
    entries
}
pub(crate) fn reserved(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes == b".rill-dav" || bytes.starts_with(b".rill-dav-put-")
}
pub(crate) fn denied(file: &File, entries: &Entries) -> std::io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(entries
        .get(&(metadata.dev(), metadata.ino()))
        .is_some_and(|lease| !lease.published.load(Ordering::Acquire)))
}
pub struct PrivateEntry {
    lease: Arc<Lease>,
}
impl std::fmt::Debug for PrivateEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateEntry").finish_non_exhaustive()
    }
}
impl PrivateEntry {
    pub fn register(file: &File, entries: &mut Entries) -> Result<Self> {
        let metadata = file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        if let Some(lease) = entries.get(&identity) {
            if lease.published.load(Ordering::Acquire) {
                return Err(DavError::new(409, "DAV_PRIVATE_IDENTITY"));
            }
            lease
                .references
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_add(1)
                })
                .map_err(|_| DavError::limit())?;
            return Ok(Self {
                lease: lease.clone(),
            });
        }
        let memory = Memory::acquire(entry_bytes())?;
        let lease = Arc::new(Lease {
            file: file.try_clone()?,
            references: AtomicUsize::new(1),
            published: AtomicBool::new(false),
            _memory: memory,
            disk: Mutex::new(None),
        });
        entries.insert(identity, lease.clone());
        Ok(Self { lease })
    }
    pub fn retain_disk(&self, budget: super::limits::DiskBudget) {
        *self
            .lease
            .disk
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(budget);
    }
    pub fn published(&self) -> bool {
        self.lease.published.load(Ordering::Acquire)
    }
    pub fn publish(&self) {
        self.lease.published.store(true, Ordering::Release);
    }
}
impl Drop for PrivateEntry {
    fn drop(&mut self) {
        self.lease.references.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retired_private_inode_keeps_its_fd_and_budget_until_last_link_disappears()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "rill-retired-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&directory)?;
        let private = directory.join("private");
        let alias = directory.join("alias");
        std::fs::write(&private, b"secret fixture")?;
        let file = File::open(&private)?;
        let identity = (file.metadata()?.dev(), file.metadata()?.ino());
        let entry = PrivateEntry::register(&file, &mut write())?;
        std::fs::hard_link(&private, &alias)?;
        std::fs::remove_file(&private)?;
        drop(entry);
        {
            let entries = read();
            assert!(denied(&File::open(&alias)?, &entries)?);
            assert!(entries.contains_key(&identity));
        }
        std::fs::remove_file(alias)?;
        for _ in 0..4 {
            drop(write());
        }
        assert!(!read().contains_key(&identity));
        drop(file);
        std::fs::remove_dir(directory)?;
        Ok(())
    }
}
