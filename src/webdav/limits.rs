use super::model::{DavError, Result};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU8, AtomicU64, Ordering},
};
use tokio::sync::Semaphore;

pub const PUT_FILE: u64 = 256 * 1024 * 1024;
pub const BLOCK: usize = 64 * 1024;
static PUT_BYTES: AtomicU64 = AtomicU64::new(0);
static PUTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
pub fn puts() -> Arc<Semaphore> {
    PUTS.get_or_init(|| Arc::new(Semaphore::new(16))).clone()
}
#[derive(Default)]
pub struct DiskBudget {
    bytes: u64,
}
impl DiskBudget {
    pub fn add(&mut self, bytes: usize) -> Result<()> {
        let bytes = u64::try_from(bytes).map_err(|_| DavError::limit())?;
        let total = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= PUT_FILE)
            .ok_or_else(|| DavError::new(413, "DAV_LIMIT"))?;
        PUT_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(bytes).filter(|n| *n <= 1024 * 1024 * 1024)
            })
            .map_err(|_| DavError::limit())?;
        self.bytes = total;
        Ok(())
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
impl Drop for DiskBudget {
    fn drop(&mut self) {
        PUT_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
#[derive(Clone)]
pub struct Cancellation(Arc<AtomicU8>);
impl Default for Cancellation {
    fn default() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
}
impl Cancellation {
    pub fn check(&self, deadline: std::time::Instant) -> Result<()> {
        if self.0.load(Ordering::Acquire) == 1 {
            return Err(DavError::new(499, "DAV_CANCELLED"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(DavError::new(504, "DAV_TIMEOUT"));
        }
        Ok(())
    }
    pub fn committing(&self) -> bool {
        self.0.load(Ordering::Acquire) == 2
    }
    pub fn admit_commit(&self, deadline: std::time::Instant) -> Result<()> {
        self.check(deadline)?;
        self.0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| DavError::new(499, "DAV_CANCELLED"))?;
        Ok(())
    }
}
pub struct CancelOnDrop(pub Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self
            .0
            .0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn put_limits_are_checked_before_growth_and_cancellation_cannot_win_after_commit_admission()
    -> Result<()> {
        let permits = (0..16)
            .map(|_| puts().try_acquire_owned())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| DavError::limit())?;
        assert!(puts().try_acquire_owned().is_err());
        drop(permits);
        let mut budgets = Vec::new();
        for _ in 0..4 {
            let mut budget = DiskBudget::default();
            budget.add(PUT_FILE as usize)?;
            assert_eq!(budget.add(1).unwrap_err().status, 413);
            budgets.push(budget);
        }
        assert_eq!(DiskBudget::default().add(1).unwrap_err().status, 503);
        drop(budgets);
        assert_eq!(PUT_BYTES.load(Ordering::Acquire), 0);
        let cancelled = Cancellation::default();
        drop(CancelOnDrop(cancelled.clone()));
        assert!(
            cancelled
                .admit_commit(std::time::Instant::now() + std::time::Duration::from_secs(1))
                .is_err()
        );
        let committing = Cancellation::default();
        committing.admit_commit(std::time::Instant::now() + std::time::Duration::from_secs(1))?;
        drop(CancelOnDrop(committing.clone()));
        assert!(committing.committing());
        assert!(
            Cancellation::default()
                .admit_commit(std::time::Instant::now())
                .is_err()
        );
        Ok(())
    }
}
