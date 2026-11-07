use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::{
    log_budget::{Credit, LogBudget, SiteBudget},
    log_queue::{LogQueue, QueuedRecord},
};

const CANDIDATE: u8 = 0;
const RUNNING: u8 = 1;
const STOPPING: u8 = 2;
const ABORTED: u8 = 3;

pub struct LogWorker {
    state: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
    finished: mpsc::Receiver<()>,
    timed_out: bool,
}

impl LogWorker {
    pub fn start(
        budget: &LogBudget,
        site: Arc<SiteBudget>,
        write: impl FnMut(QueuedRecord) + Send + 'static,
    ) -> io::Result<(Self, LogQueue)> {
        let credit = budget
            .reserve_worker()
            .ok_or_else(|| io::Error::other("LOG_CAPACITY"))?;
        Self::start_reserved(credit, site, write)
    }

    pub fn start_reserved(
        credit: Credit,
        site: Arc<SiteBudget>,
        write: impl FnMut(QueuedRecord) + Send + 'static,
    ) -> io::Result<(Self, LogQueue)> {
        Self::start_reserved_with_maintenance(
            credit,
            site,
            write,
            || {},
            Duration::from_secs(60),
            (),
        )
    }

    pub fn start_reserved_with_maintenance(
        credit: Credit,
        site: Arc<SiteBudget>,
        mut write: impl FnMut(QueuedRecord) + Send + 'static,
        mut maintenance: impl FnMut() + Send + 'static,
        interval: Duration,
        lifetime: impl Send + 'static,
    ) -> io::Result<(Self, LogQueue)> {
        let (queue, mut receiver) = LogQueue::new(Arc::clone(&site));
        let state = Arc::new(AtomicU8::new(CANDIDATE));
        let worker_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("rill-file-log".into())
            .spawn(move || {
                let _ = ready_tx.send(());
                let mut last_maintenance = Instant::now();
                loop {
                    let phase = worker_state.load(Ordering::Acquire);
                    if phase == ABORTED {
                        break;
                    }
                    if phase == CANDIDATE {
                        thread::park_timeout(Duration::from_millis(50));
                        continue;
                    }
                    if phase == RUNNING && last_maintenance.elapsed() >= interval {
                        maintenance();
                        last_maintenance = Instant::now();
                    }
                    match receiver.try_recv() {
                        Ok(record) => write(record),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) if phase != STOPPING => {
                            thread::park_timeout(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
                drop(receiver);
                drop(write);
                drop(maintenance);
                drop(lifetime);
                drop(site);
                drop(credit);
                let _ = done_tx.send(());
            })?;
        queue.set_worker(handle.thread().clone());
        ready_rx.recv().map_err(io::Error::other)?;
        Ok((
            Self {
                state,
                thread: Some(handle),
                finished,
                timed_out: false,
            },
            queue,
        ))
    }

    pub fn commit(&self) {
        if self
            .state
            .compare_exchange(CANDIDATE, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.wake();
        }
    }

    fn wake(&self) {
        if let Some(handle) = &self.thread {
            handle.thread().unpark();
        }
    }

    pub fn shutdown(&mut self, timeout: Duration) -> bool {
        self.request_stop();
        let Some(handle) = self.thread.take() else {
            return !self.timed_out;
        };
        if self
            .finished
            .recv_timeout(timeout.min(Duration::from_secs(5)))
            .is_ok()
        {
            let _ = handle.join();
            true
        } else {
            self.timed_out = true;
            false
        }
    }

    pub fn request_stop(&self) {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let next = match current {
                CANDIDATE => ABORTED,
                RUNNING => STOPPING,
                _ => break,
            };
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.wake();
    }
}

impl Drop for LogWorker {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{
        file_config::{FileLogRecord, LogFields},
        log_queue::RotationPolicy,
    };

    fn record() -> FileLogRecord<'static> {
        FileLogRecord {
            fields: LogFields::default(),
            timestamp: 0,
            site: None,
            route: None,
            method: "GET",
            status: None,
            duration_ms: 0,
            bytes: None,
            uri: None,
            client_ip: None,
        }
    }
    fn policy() -> RotationPolicy {
        RotationPolicy {
            rotate_size: 16384,
            retain_files: 1,
            retain_days: 1,
        }
    }

    #[test]
    fn candidate_gate_commit_and_abort() {
        let budget = LogBudget::process();
        let (tx, rx) = mpsc::channel();
        let (mut worker, queue) =
            LogWorker::start(budget, budget.reserve_site().unwrap(), move |_| {
                tx.send(()).unwrap();
            })
            .unwrap();
        queue.submit(&record(), policy()).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(70)).is_err());
        worker.commit();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(worker.shutdown(Duration::from_secs(2)));

        let (tx, rx) = mpsc::channel();
        let (mut worker, queue) =
            LogWorker::start(budget, budget.reserve_site().unwrap(), move |_| {
                tx.send(()).unwrap();
            })
            .unwrap();
        queue.submit(&record(), policy()).unwrap();
        assert!(worker.shutdown(Duration::from_secs(2)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn shutdown_does_not_wait_for_blocked_writer() {
        let budget = LogBudget::process();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (mut worker, queue) =
            LogWorker::start(budget, budget.reserve_site().unwrap(), move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        worker.commit();
        queue.submit(&record(), policy()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!worker.shutdown(Duration::from_millis(20)));
        assert!(!worker.shutdown(Duration::from_millis(20)));
        release_tx.send(()).unwrap();
    }
}
