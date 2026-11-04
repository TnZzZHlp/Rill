use super::super::metrics::{Diagnostic, Metrics};
use super::exporter;
use opentelemetry::Context;
use opentelemetry_sdk::{
    Resource,
    error::{OTelSdkError, OTelSdkResult},
    trace::{Span, SpanData, SpanExporter, SpanProcessor},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CAPACITY: usize = 2048;
const BATCH: usize = 128;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(2);
struct Budget(AtomicUsize);
impl Budget {
    fn reserve(self: &Arc<Self>) -> Option<Permit> {
        let mut count = self.0.load(Ordering::Acquire);
        loop {
            if count >= CAPACITY {
                return None;
            }
            match self.0.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Permit(self.clone())),
                Err(actual) => count = actual,
            }
        }
    }
}
struct Permit(Arc<Budget>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Envelope {
    span: SpanData,
    permit: Permit,
}
struct Shared {
    stop: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    budget: Arc<Budget>,
    metrics: Arc<Metrics>,
}
pub(super) struct Processor {
    tx: Mutex<Option<mpsc::SyncSender<Envelope>>>,
    shared: Arc<Shared>,
    resource: mpsc::SyncSender<Resource>,
    worker: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<mpsc::Receiver<()>>,
}
impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TraceProcessor")
    }
}
struct Done(mpsc::SyncSender<()>);
impl Drop for Done {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn export_batch(
    exporter: &opentelemetry_otlp::SpanExporter,
    batch: Vec<Envelope>,
    metrics: &Metrics,
) {
    let mut spans = Vec::with_capacity(batch.len());
    let mut permits = Vec::with_capacity(batch.len());
    for Envelope { span, permit } in batch {
        spans.push(span);
        permits.push(permit);
    }
    if exporter::block_on(exporter.export(spans)).is_err() {
        metrics.diagnostic(Diagnostic::TraceExportFailure);
    }
    drop(permits);
}
fn drain(rx: &mpsc::Receiver<Envelope>, metrics: &Metrics) {
    while let Ok(envelope) = rx.try_recv() {
        metrics.diagnostic(Diagnostic::TraceQueueDrop);
        drop(envelope);
    }
}
fn remaining(state: &Shared) -> Duration {
    state
        .deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
}
impl Processor {
    pub(super) fn new(endpoint: &str, metrics: Arc<Metrics>) -> Result<Self, ()> {
        let (tx, rx) = mpsc::sync_channel::<Envelope>(CAPACITY);
        let (resource, resource_rx) = mpsc::sync_channel::<Resource>(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, finished) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            deadline: Mutex::new(None),
            budget: Arc::new(Budget(AtomicUsize::new(0))),
            metrics,
        });
        let state = shared.clone();
        let endpoint = endpoint.to_owned();
        let worker = thread::Builder::new()
            .name("rill-trace".into())
            .spawn(move || {
                let _done = Done(done_tx);
                let mut exporter = match exporter::build(&endpoint) {
                    Ok(e) => {
                        let _ = ready_tx.try_send(true);
                        e
                    }
                    Err(()) => {
                        let _ = ready_tx.try_send(false);
                        return;
                    }
                };
                let resource = loop {
                    if state.stop.load(Ordering::Acquire) {
                        drain(&rx, &state.metrics);
                        return;
                    }
                    match resource_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(r) => break r,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            drain(&rx, &state.metrics);
                            return;
                        }
                    }
                };
                exporter.set_resource(&resource);
                loop {
                    let stopped = state.stop.load(Ordering::Acquire);
                    let first = if stopped {
                        rx.try_recv().ok()
                    } else {
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(s) => Some(s),
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => None,
                        }
                    };
                    let Some(first) = first else {
                        break;
                    };
                    let mut batch = Vec::with_capacity(BATCH);
                    batch.push(first);
                    while batch.len() < BATCH {
                        match rx.try_recv() {
                            Ok(s) => batch.push(s),
                            Err(_) => break,
                        }
                    }
                    if state.stop.load(Ordering::Acquire) {
                        if remaining(&state) >= EXPORT_TIMEOUT {
                            export_batch(&exporter, batch, &state.metrics);
                        } else {
                            for envelope in batch {
                                state.metrics.diagnostic(Diagnostic::TraceQueueDrop);
                                drop(envelope);
                            }
                        }
                        break;
                    }
                    export_batch(&exporter, batch, &state.metrics);
                }
                drain(&rx, &state.metrics);
            })
            .map_err(|_| ())?;
        if ready_rx.recv_timeout(Duration::from_secs(3)).ok() != Some(true) {
            drop(tx);
            shared.stop.store(true, Ordering::Release);
            if finished.try_recv().is_ok() {
                let _ = worker.join();
            }
            return Err(());
        }
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            shared,
            resource,
            worker: Mutex::new(Some(worker)),
            finished: Mutex::new(finished),
        })
    }
    fn close(&self, deadline: Instant) {
        let mut tx = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *self
            .shared
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(deadline);
        self.shared.stop.store(true, Ordering::Release);
        tx.take();
    }
}
impl SpanProcessor for Processor {
    fn on_start(&self, _: &mut Span, _: &Context) {}
    fn on_end(&self, span: SpanData) {
        let Ok(tx) = self.tx.try_lock() else {
            self.shared.metrics.diagnostic(Diagnostic::TraceQueueDrop);
            return;
        };
        let Some(tx) = tx.as_ref() else {
            self.shared.metrics.diagnostic(Diagnostic::TraceQueueDrop);
            return;
        };
        let Some(permit) = self.shared.budget.reserve() else {
            self.shared.metrics.diagnostic(Diagnostic::TraceQueueDrop);
            return;
        };
        if tx.try_send(Envelope { span, permit }).is_err() {
            self.shared.metrics.diagnostic(Diagnostic::TraceQueueDrop);
        }
    }
    fn force_flush(&self) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure("flush unavailable".into()))
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let timeout = timeout.min(Duration::from_secs(3));
        let deadline = Instant::now() + timeout;
        self.close(deadline);
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = worker.take() {
            if self
                .finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_ok()
            {
                let _ = handle.join();
            } else {
                return Err(OTelSdkError::Timeout(timeout));
            }
        }
        Ok(())
    }
    fn set_resource(&mut self, resource: &Resource) {
        let _ = self.resource.try_send(resource.clone());
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        self.close(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_counts_inflight_and_returns_permit_on_drop() {
        let budget = Arc::new(Budget(AtomicUsize::new(0)));
        let mut held: Vec<_> = (0..CAPACITY).map(|_| budget.reserve().unwrap()).collect();
        assert!(budget.reserve().is_none());
        drop(held.pop());
        let replacement = budget.reserve().unwrap();
        assert!(budget.reserve().is_none());
        drop(replacement);
        drop(held);
        assert_eq!(budget.0.load(Ordering::Acquire), 0);
    }
}
