use std::sync::{Arc, OnceLock};

use super::{
    file_config::{FileLogRecord, LogConfig, encode_record},
    log_budget::{RecordCredit, SiteBudget},
};

const SITE_QUEUE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct RotationPolicy {
    pub rotate_size: u64,
    pub retain_files: u8,
    pub retain_days: u8,
}

impl From<&LogConfig> for RotationPolicy {
    fn from(config: &LogConfig) -> Self {
        Self {
            rotate_size: config.rotate_size,
            retain_files: config.retain_files,
            retain_days: config.retain_days,
        }
    }
}

#[derive(Debug)]
pub struct QueuedRecord {
    pub bytes: Vec<u8>,
    pub rotation: RotationPolicy,
    _credit: RecordCredit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogDropped;

#[derive(Clone, Debug)]
pub struct LogQueue {
    site: Arc<SiteBudget>,
    sender: tokio::sync::mpsc::Sender<QueuedRecord>,
    wakeup: Arc<OnceLock<std::thread::Thread>>,
}

impl LogQueue {
    pub fn new(site: Arc<SiteBudget>) -> (Self, tokio::sync::mpsc::Receiver<QueuedRecord>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(SITE_QUEUE_LIMIT);
        (
            Self {
                site,
                sender,
                wakeup: Arc::new(OnceLock::new()),
            },
            receiver,
        )
    }

    pub(crate) fn set_worker(&self, worker: std::thread::Thread) {
        let _ = self.wakeup.set(worker);
    }

    pub fn submit(
        &self,
        record: &FileLogRecord<'_>,
        policy: RotationPolicy,
    ) -> Result<(), LogDropped> {
        let credit = self.site.reserve_record().ok_or(LogDropped)?;
        let permit = self.sender.try_reserve().map_err(|_| LogDropped)?;
        let bytes = encode_record(record).map_err(|_| LogDropped)?;
        permit.send(QueuedRecord {
            bytes,
            rotation: policy,
            _credit: credit,
        });
        if let Some(worker) = self.wakeup.get() {
            worker.unpark();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{file_config::LogFields, log_budget::LogBudget};

    #[test]
    fn full_and_closed_queues_drop_without_losing_credit() {
        let site = LogBudget::process().reserve_site().unwrap();
        let (queue, mut receiver) = LogQueue::new(site);
        let record = FileLogRecord {
            fields: LogFields::default(),
            timestamp: 1,
            site: Some("site"),
            route: None,
            method: "GET",
            status: Some(200),
            duration_ms: 0,
            bytes: None,
            uri: None,
            client_ip: None,
        };
        let policy = RotationPolicy {
            rotate_size: 16384,
            retain_files: 2,
            retain_days: 3,
        };
        for _ in 0..SITE_QUEUE_LIMIT {
            assert_eq!(queue.submit(&record, policy), Ok(()));
        }
        assert_eq!(queue.submit(&record, policy), Err(LogDropped));
        let first = receiver.try_recv().unwrap();
        assert_eq!(first.rotation.retain_files, 2);
        assert_eq!(queue.submit(&record, policy), Err(LogDropped));
        drop(first);
        assert_eq!(queue.submit(&record, policy), Ok(()));
        drop(receiver);
        assert_eq!(queue.submit(&record, policy), Err(LogDropped));
    }
}
