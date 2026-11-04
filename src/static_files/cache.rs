use std::collections::BTreeMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use bytes::Bytes;

const MAX_BODY: usize = 64 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_PATH: usize = 4096;
const MAX_BUDGET: usize = 256 * 1024 * 1024;
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    budget: Weak::new(),
    pool: Weak::new(),
});
static NAMESPACE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_namespace() -> Option<u64> {
    let mut current = NAMESPACE.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match NAMESPACE.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Binding {
    pub pool: Arc<CachePool>,
    pub namespace: u64,
    pub generation: u64,
    pub route: (usize, usize),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct FileVersion {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}
impl From<&Metadata> for FileVersion {
    fn from(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Key {
    pub namespace: u64,
    pub generation: u64,
    pub route: (usize, usize),
    pub root: (u64, u64),
    pub path: PathBuf,
    pub encoding: &'static str,
    pub version: FileVersion,
}

#[derive(Debug)]
struct Budget {
    cap: usize,
    used: AtomicUsize,
}
impl Budget {
    fn reserve(self: &Arc<Self>, len: usize) -> Option<BudgetPermit> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(len).filter(|&n| n <= self.cap)?;
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(BudgetPermit {
                        budget: self.clone(),
                        len,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}
#[derive(Debug)]
pub(crate) struct BudgetPermit {
    budget: Arc<Budget>,
    len: usize,
}
impl Drop for BudgetPermit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.len, Ordering::AcqRel);
    }
}
#[derive(Debug)]
struct BodyOwner {
    body: Box<[u8]>,
    _permit: BudgetPermit,
}
impl AsRef<[u8]> for BodyOwner {
    fn as_ref(&self) -> &[u8] {
        &self.body
    }
}

pub(crate) struct ReservedBody {
    body: Vec<u8>,
    permit: BudgetPermit,
}
impl BudgetPermit {
    pub(crate) fn allocate(self) -> ReservedBody {
        let body = vec![0; self.len];
        assert_eq!(body.capacity(), self.len);
        ReservedBody { body, permit: self }
    }
}
impl ReservedBody {
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.body
    }
    fn finish(self) -> Bytes {
        Bytes::from_owner(BodyOwner {
            body: self.body.into_boxed_slice(),
            _permit: self.permit,
        })
    }
}

#[derive(Debug)]
enum Entry {
    Loading(u64),
    Ready { bytes: Bytes, used: u64 },
}
#[derive(Debug, Default)]
struct State {
    entries: BTreeMap<Key, Entry>,
    clock: u64,
    hits: u64,
    misses: u64,
    loads: u64,
    fallback: u64,
}
#[derive(Debug)]
pub(crate) struct CachePool {
    budget: Arc<Budget>,
    state: Mutex<State>,
}
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub cap: usize,
    pub used: usize,
    pub index_entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub loads: u64,
    pub fallback: u64,
}

#[derive(Debug, Default)]
struct Registry {
    budget: Weak<Budget>,
    pool: Weak<CachePool>,
}
impl Registry {
    fn register(&mut self, cap: usize) -> Result<Option<Arc<CachePool>>, &'static str> {
        if cap == 0 {
            return Ok(None);
        }
        if cap > MAX_BUDGET {
            return Err("static cache budget exceeds limit");
        }
        let budget = if let Some(budget) = self.budget.upgrade() {
            if budget.cap != cap {
                return Err("static cache budget conflicts with a live pool");
            }
            if let Some(pool) = self.pool.upgrade() {
                return Ok(Some(pool));
            }
            budget
        } else {
            Arc::new(Budget {
                cap,
                used: AtomicUsize::new(0),
            })
        };
        let pool = Arc::new(CachePool {
            budget: budget.clone(),
            state: Mutex::new(State::default()),
        });
        self.budget = Arc::downgrade(&budget);
        self.pool = Arc::downgrade(&pool);
        Ok(Some(pool))
    }
}
pub(crate) fn register(cap: usize) -> Result<Option<Arc<CachePool>>, &'static str> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(cap)
}

pub(crate) enum Lookup {
    Hit(Bytes),
    Load(LoadingGuard, BudgetPermit),
    Stream,
}
pub(crate) struct LoadingGuard {
    pool: Arc<CachePool>,
    key: Key,
    token: u64,
}
impl LoadingGuard {
    pub(crate) fn publish(self, body: ReservedBody) -> Option<Bytes> {
        if !Arc::ptr_eq(&self.pool.budget, &body.permit.budget)
            || u64::try_from(body.permit.len).ok() != Some(self.key.version.size)
        {
            return None;
        }
        let bytes = body.finish();
        let mut state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.entries.get(&self.key), Some(Entry::Loading(token)) if *token == self.token)
        {
            state.entries.insert(
                self.key.clone(),
                Entry::Ready {
                    bytes: bytes.clone(),
                    used: self.token,
                },
            );
        }
        drop(state);
        Some(bytes)
    }
}
impl Drop for LoadingGuard {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(state.entries.get(&self.key), Some(Entry::Loading(token)) if *token == self.token)
        {
            state.entries.remove(&self.key);
        }
    }
}
impl CachePool {
    pub(crate) fn stats(&self) -> Stats {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Stats {
            cap: self.budget.cap,
            used: self.budget.used.load(Ordering::Acquire),
            index_entries: state.entries.len(),
            hits: state.hits,
            misses: state.misses,
            loads: state.loads,
            fallback: state.fallback,
        }
    }
    pub(crate) fn lookup_or_begin(self: &Arc<Self>, key: Key) -> Lookup {
        use std::os::unix::ffi::OsStrExt;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(clock) = state.clock.checked_add(1) else {
            state.fallback = state.fallback.saturating_add(1);
            return Lookup::Stream;
        };
        state.clock = clock;
        if let Some(Entry::Ready { bytes, used }) = state.entries.get_mut(&key) {
            *used = clock;
            let bytes = bytes.clone();
            state.hits = state.hits.saturating_add(1);
            return Lookup::Hit(bytes);
        }
        state.misses = state.misses.saturating_add(1);
        let len = usize::try_from(key.version.size).ok();
        if state.entries.contains_key(&key)
            || key.path.as_os_str().as_bytes().len() > MAX_PATH
            || !matches!(len, Some(1..=MAX_BODY))
        {
            state.fallback = state.fallback.saturating_add(1);
            return Lookup::Stream;
        }
        let len = len.unwrap_or(0);
        loop {
            if state.entries.len() < MAX_ENTRIES
                && let Some(permit) = self.budget.reserve(len)
            {
                state.entries.insert(key.clone(), Entry::Loading(clock));
                state.loads = state.loads.saturating_add(1);
                return Lookup::Load(
                    LoadingGuard {
                        pool: self.clone(),
                        key,
                        token: clock,
                    },
                    permit,
                );
            }
            let victim = state
                .entries
                .iter()
                .filter_map(|(key, entry)| match entry {
                    Entry::Ready { used, .. } => Some((key, used)),
                    Entry::Loading(_) => None,
                })
                .min_by_key(|(_, used)| **used)
                .map(|(key, _)| key.clone());
            if let Some(victim) = victim {
                state.entries.remove(&victim);
            } else {
                state.fallback = state.fallback.saturating_add(1);
                return Lookup::Stream;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(cap: usize) -> Arc<CachePool> {
        Registry::default().register(cap).unwrap().unwrap()
    }
    fn key(id: u64) -> Key {
        Key {
            namespace: 1,
            generation: 1,
            route: (0, 0),
            root: (1, 1),
            path: PathBuf::from(format!("{id}")),
            encoding: "identity",
            version: FileVersion {
                dev: 1,
                ino: id,
                size: 1,
                mtime: 1,
                mtime_nsec: 0,
                ctime: 1,
                ctime_nsec: 0,
                mode: 0o644,
                uid: 0,
                gid: 0,
            },
        }
    }
    fn load(pool: &Arc<CachePool>, key: Key) -> Bytes {
        let Lookup::Load(guard, permit) = pool.lookup_or_begin(key) else {
            panic!("expected load")
        };
        let mut body = permit.allocate();
        body.as_mut_slice()[0] = 42;
        guard.publish(body).unwrap()
    }
    #[test]
    fn evicted_body_retains_budget() {
        let pool = pool(1);
        let body = load(&pool, key(0));
        assert!(matches!(pool.lookup_or_begin(key(1)), Lookup::Stream));
        assert_eq!(pool.stats().used, 1);
        assert_eq!(pool.stats().index_entries, 0);
        drop(body);
        assert_eq!(pool.stats().used, 0);
        assert!(matches!(pool.lookup_or_begin(key(1)), Lookup::Load(_, _)));
    }
    #[test]
    fn registry_retains_budget_after_pool_drop() {
        let mut registry = Registry::default();
        let pool = registry.register(1).unwrap().unwrap();
        let body = load(&pool, key(0));
        drop(pool);
        assert!(registry.register(2).is_err());
        assert!(registry.register(0).unwrap().is_none());
        let next = registry.register(1).unwrap().unwrap();
        assert!(matches!(next.lookup_or_begin(key(1)), Lookup::Stream));
        drop(body);
        assert!(matches!(next.lookup_or_begin(key(1)), Lookup::Load(_, _)));
        drop(next);
        assert!(registry.register(2).is_ok());
    }
    #[test]
    fn cancelled_loading_releases_slot_and_permit() {
        let pool = pool(1);
        let Lookup::Load(guard, permit) = pool.lookup_or_begin(key(0)) else {
            panic!("expected load")
        };
        assert_eq!(pool.stats().used, 1);
        drop(guard);
        drop(permit);
        assert_eq!(pool.stats().used, 0);
        assert_eq!(pool.stats().index_entries, 0);
        assert!(matches!(pool.lookup_or_begin(key(0)), Lookup::Load(_, _)));
    }
    #[test]
    fn loading_is_single_key_nonblocking() {
        let pool = pool(2);
        let Lookup::Load(guard, permit) = pool.lookup_or_begin(key(0)) else {
            panic!("expected load")
        };
        assert!(matches!(pool.lookup_or_begin(key(0)), Lookup::Stream));
        let body = guard.publish(permit.allocate()).unwrap();
        assert!(matches!(pool.lookup_or_begin(key(0)), Lookup::Hit(_)));
        assert_eq!(pool.stats().loads, 1);
        drop(body);
    }
    #[test]
    fn inflight_slots_are_bounded() {
        let pool = pool(MAX_ENTRIES + 1);
        let mut loads = Vec::new();
        for id in 0..MAX_ENTRIES as u64 {
            let Lookup::Load(guard, permit) = pool.lookup_or_begin(key(id)) else {
                panic!("expected load")
            };
            loads.push((guard, permit));
        }
        assert_eq!(pool.stats().index_entries, MAX_ENTRIES);
        assert!(matches!(
            pool.lookup_or_begin(key(MAX_ENTRIES as u64)),
            Lookup::Stream
        ));
        drop(loads);
        assert_eq!(pool.stats().index_entries, 0);
    }
    #[test]
    fn version_encoding_and_namespace_are_isolated() {
        let pool = pool(8);
        let original = key(0);
        let _body = load(&pool, original.clone());
        assert!(matches!(
            pool.lookup_or_begin(original.clone()),
            Lookup::Hit(_)
        ));
        let mut version = original.clone();
        version.version.ctime_nsec += 1;
        assert!(matches!(pool.lookup_or_begin(version), Lookup::Load(_, _)));
        let mut encoding = original.clone();
        encoding.encoding = "gzip";
        assert!(matches!(pool.lookup_or_begin(encoding), Lookup::Load(_, _)));
        let mut namespace = original;
        namespace.namespace += 1;
        assert!(matches!(
            pool.lookup_or_begin(namespace),
            Lookup::Load(_, _)
        ));
    }
}
