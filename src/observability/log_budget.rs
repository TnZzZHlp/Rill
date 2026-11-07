use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

const MAX_SITES: usize = 64;
const MAX_SINKS: usize = 64;
const MAX_WORKERS: usize = 64;
// Each admitted disk target reserves the full six files and 60 MiB.
const MAX_DISK_TARGETS: usize = 64;
const MAX_FDS: usize = 256;
const MAX_RECORDS: usize = 16_384;
const MAX_SITE_RECORDS: usize = 1_024;

#[derive(Debug)]
struct Counter {
    used: AtomicUsize,
    limit: usize,
}
impl Counter {
    const fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }
    fn acquire(self: &Arc<Self>) -> Option<Credit> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            if used >= self.limit {
                return None;
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => used = current,
            }
        }
        Some(Credit(Arc::clone(self)))
    }
}

#[derive(Debug)]
pub struct Credit(Arc<Counter>);
impl Drop for Credit {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub struct LogBudget {
    sites: Arc<Counter>,
    sinks: Arc<Counter>,
    workers: Arc<Counter>,
    fds: Arc<Counter>,
    disks: Arc<Counter>,
    records: Arc<Counter>,
}
impl LogBudget {
    fn new() -> Self {
        Self {
            sites: Arc::new(Counter::new(MAX_SITES)),
            sinks: Arc::new(Counter::new(MAX_SINKS)),
            workers: Arc::new(Counter::new(MAX_WORKERS)),
            fds: Arc::new(Counter::new(MAX_FDS)),
            disks: Arc::new(Counter::new(MAX_DISK_TARGETS)),
            records: Arc::new(Counter::new(MAX_RECORDS)),
        }
    }
    pub fn process() -> &'static Self {
        static BUDGET: OnceLock<LogBudget> = OnceLock::new();
        BUDGET.get_or_init(Self::new)
    }
    pub fn reserve_site(&self) -> Option<Arc<SiteBudget>> {
        Some(Arc::new(SiteBudget {
            _site: self.sites.acquire()?,
            records: Arc::new(Counter::new(MAX_SITE_RECORDS)),
            global_records: Arc::clone(&self.records),
        }))
    }
    pub fn reserve_sink(&self) -> Option<Credit> {
        self.sinks.acquire()
    }
    pub fn reserve_worker(&self) -> Option<Credit> {
        self.workers.acquire()
    }
    pub fn reserve_disk_target(&self) -> Option<Credit> {
        self.disks.acquire()
    }
    pub fn reserve_fd(&self) -> Option<Credit> {
        self.fds.acquire()
    }
}

#[derive(Debug)]
pub struct SiteBudget {
    _site: Credit,
    records: Arc<Counter>,
    global_records: Arc<Counter>,
}
impl SiteBudget {
    pub fn reserve_record(self: &Arc<Self>) -> Option<RecordCredit> {
        let site = self.records.acquire()?;
        let global = self.global_records.acquire()?;
        Some(RecordCredit {
            _site: site,
            _global: global,
            _owner: Arc::clone(self),
        })
    }
}

#[derive(Debug)]
pub struct RecordCredit {
    _site: Credit,
    _global: Credit,
    _owner: Arc<SiteBudget>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_credits_cover_live_generations_and_are_reusable() {
        let budget = LogBudget::new();
        let credits: Vec<_> = (0..MAX_FDS).map(|_| budget.reserve_fd().unwrap()).collect();
        assert!(budget.reserve_fd().is_none());
        drop(credits);
        assert!(budget.reserve_fd().is_some());
        let sites: Vec<_> = (0..MAX_SITES)
            .map(|_| budget.reserve_site().unwrap())
            .collect();
        assert!(budget.reserve_site().is_none());
        let record = sites[0].reserve_record().unwrap();
        drop(sites);
        let sites: Vec<_> = (0..MAX_SITES - 1)
            .map(|_| budget.reserve_site().unwrap())
            .collect();
        assert!(budget.reserve_site().is_none());
        drop(record);
        assert!(budget.reserve_site().is_some());
        drop(sites);
    }

    #[test]
    fn queue_limits_are_shared_and_rollback_site_reservation() {
        let budget = LogBudget::new();
        let sites: Vec<_> = (0..17).map(|_| budget.reserve_site().unwrap()).collect();
        let credits: Vec<_> = sites[..16]
            .iter()
            .flat_map(|site| (0..MAX_SITE_RECORDS).map(move |_| site.reserve_record().unwrap()))
            .collect();
        assert!(sites[0].reserve_record().is_none());
        assert!(sites[16].reserve_record().is_none());
        drop(credits);
        let again: Vec<_> = (0..MAX_SITE_RECORDS)
            .map(|_| sites[16].reserve_record().unwrap())
            .collect();
        assert!(sites[16].reserve_record().is_none());
        drop(again);
    }
}
