use super::{
    model::{DavError, Result},
    path::DavPath,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

static LOCKS: AtomicUsize = AtomicUsize::new(0);
struct Lease;
impl Lease {
    fn acquire() -> Result<Self> {
        LOCKS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(1).filter(|n| *n <= 4096)
            })
            .map_err(|_| DavError::limit())?;
        Ok(Self)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        LOCKS.fetch_sub(1, Ordering::AcqRel);
    }
}
pub struct Lock {
    pub path: DavPath,
    pub token: String,
    pub infinity: bool,
    pub owner: String,
    pub expires: Instant,
    _lease: Lease,
    _memory: super::budget::Memory,
}
impl Lock {
    pub fn applies(&self, path: &DavPath) -> bool {
        self.path == *path || (self.infinity && self.path.contains(path))
    }
    pub fn seconds(&self) -> u64 {
        self.expires
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1)
    }
}
#[derive(Default)]
pub struct Locks {
    entries: Vec<Lock>,
    slots: super::budget::Memory,
}
impl std::fmt::Debug for Locks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locks")
            .field("count", &self.entries.len())
            .finish()
    }
}
impl Locks {
    pub fn expire(&mut self) {
        let now = Instant::now();
        self.entries.retain(|lock| lock.expires > now);
        self.release_empty();
    }
    fn release_empty(&mut self) {
        if self.entries.is_empty() {
            drop(std::mem::take(&mut self.entries));
            self.slots = super::budget::Memory::default();
        }
    }
    pub fn active(&self) -> impl Iterator<Item = &Lock> {
        self.entries
            .iter()
            .filter(|lock| lock.expires > Instant::now())
    }
    pub fn valid(&self, path: &DavPath, token: &str) -> bool {
        self.active()
            .any(|lock| lock.token == token && lock.applies(path))
    }
    pub fn check(
        &self,
        path: &DavPath,
        descendants: bool,
        parent: bool,
        submitted: &[(DavPath, String)],
    ) -> Result<()> {
        for lock in self.active() {
            if (lock.applies(path)
                || (descendants && path.contains(&lock.path))
                || (parent && lock.applies(&path.parent())))
                && !submitted
                    .iter()
                    .any(|(resource, token)| *token == lock.token && lock.applies(resource))
            {
                return Err(DavError::new(423, "DAV_LOCKED"));
            }
        }
        Ok(())
    }
    pub fn create(
        &mut self,
        path: DavPath,
        infinity: bool,
        owner: String,
        seconds: u64,
    ) -> Result<String> {
        self.expire();
        if self
            .active()
            .any(|lock| lock.applies(&path) || (infinity && path.contains(&lock.path)))
        {
            return Err(DavError::new(423, "DAV_LOCKED"));
        }
        if self.entries.len() >= 1024 {
            return Err(DavError::limit());
        }
        let memory = super::budget::Memory::acquire(
            owner
                .capacity()
                .checked_add(path.key().len())
                .and_then(|n| n.checked_add(256))
                .ok_or_else(DavError::limit)?,
        )?;
        let lease = Lease::acquire()?;
        if self.entries.len() == self.entries.capacity() {
            let old = self.entries.capacity() * std::mem::size_of::<Lock>();
            let next = (self.entries.len() + 1) * std::mem::size_of::<Lock>();
            self.slots.reserve(next)?;
            if self.entries.try_reserve_exact(1).is_err() {
                self.slots.release(next);
                return Err(DavError::limit());
            }
            self.slots.release(old);
        }
        let mut random = [0_u8; 16];
        openssl::rand::rand_bytes(&mut random).map_err(|_| DavError::new(500, "DAV_RANDOM"))?;
        random[6] = (random[6] & 0x0f) | 0x40;
        random[8] = (random[8] & 0x3f) | 0x80;
        let token = format!(
            "urn:uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            random[0],
            random[1],
            random[2],
            random[3],
            random[4],
            random[5],
            random[6],
            random[7],
            random[8],
            random[9],
            random[10],
            random[11],
            random[12],
            random[13],
            random[14],
            random[15]
        );
        self.entries.push(Lock {
            path,
            token: token.clone(),
            infinity,
            owner,
            expires: Instant::now() + Duration::from_secs(seconds.clamp(1, 3600)),
            _lease: lease,
            _memory: memory,
        });
        Ok(token)
    }
    pub fn refresh(
        &mut self,
        path: &DavPath,
        submitted: &[(DavPath, String)],
        seconds: u64,
    ) -> Result<String> {
        self.expire();
        let lock = self
            .entries
            .iter_mut()
            .find(|lock| {
                lock.path == *path
                    && submitted
                        .iter()
                        .any(|(p, token)| *p == *path && *token == lock.token)
            })
            .ok_or_else(|| DavError::new(412, "DAV_LOCK_TOKEN"))?;
        lock.expires = Instant::now() + Duration::from_secs(seconds.clamp(1, 3600));
        Ok(lock.token.clone())
    }
    pub fn unlock(&mut self, path: &DavPath, token: &str) -> Result<()> {
        self.expire();
        let index = self
            .entries
            .iter()
            .position(|lock| lock.path == *path && lock.token == token)
            .ok_or_else(|| DavError::new(409, "DAV_LOCK_TOKEN"))?;
        self.entries.remove(index);
        self.release_empty();
        Ok(())
    }
    pub fn remove(&mut self, path: &DavPath) {
        self.entries.retain(|lock| !path.contains(&lock.path));
        self.release_empty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locks_cover_ancestors_bindings_and_recursive_operations() -> Result<()> {
        let mut locks = Locks::default();
        let root = DavPath::parse("/docs")?;
        let token = locks.create(root.clone(), true, String::new(), 60)?;
        assert!(
            locks
                .check(&DavPath::parse("/docs/a")?, false, false, &[])
                .is_err()
        );
        assert!(
            locks
                .check(&DavPath::parse("/")?, true, false, &[])
                .is_err()
        );
        locks.check(
            &DavPath::parse("/docs/a")?,
            false,
            true,
            &[(root.clone(), token.clone())],
        )?;
        assert!(
            locks
                .create(DavPath::parse("/docs/a")?, false, String::new(), 60)
                .is_err()
        );
        locks.refresh(&root, &[(root.clone(), token.clone())], 1)?;
        locks.unlock(&root, &token)?;
        assert_eq!(locks.active().count(), 0);
        let mut stores = Vec::new();
        for _ in 0..4 {
            let mut storage = Locks::default();
            for index in 0..1024 {
                storage.create(
                    DavPath::parse(&format!("/item-{index}"))?,
                    false,
                    String::new(),
                    60,
                )?;
            }
            assert!(
                storage
                    .create(DavPath::parse("/overflow")?, false, String::new(), 60)
                    .is_err()
            );
            stores.push(storage);
        }
        assert!(
            locks
                .create(
                    DavPath::parse("/global-overflow")?,
                    false,
                    String::new(),
                    60
                )
                .is_err()
        );
        stores[0].entries[0].expires = Instant::now();
        stores[0].expire();
        locks.create(DavPath::parse("/expired-slot")?, false, String::new(), 60)?;
        drop(stores);
        drop(locks);
        assert_eq!(LOCKS.load(Ordering::Acquire), 0);
        Ok(())
    }
}
