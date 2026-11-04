use super::access::{AccessSink, HexId};
use super::metrics::Metrics;
use super::trace::{TraceError, TraceSink};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

pub struct Observability {
    pub metrics: Arc<Metrics>,
    pub access: AccessSink,
    pub trace: TraceSink,
    pub metrics_enabled: bool,
    namespace: u64,
    sequence: AtomicU64,
}
impl std::fmt::Debug for Observability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observability").finish_non_exhaustive()
    }
}
impl Observability {
    pub fn new(metrics_enabled: bool, endpoint: &str) -> Result<Arc<Self>, TraceError> {
        let metrics = Arc::new(Metrics::new());
        let trace = TraceSink::new(endpoint, metrics.clone())?;
        let access = AccessSink::new(metrics.clone()).map_err(|_| TraceError::Startup)?;
        let mut bytes = [0u8; 8];
        openssl::rand::rand_bytes(&mut bytes).map_err(|_| TraceError::Startup)?;
        Ok(Arc::new(Self {
            metrics,
            access,
            trace,
            metrics_enabled,
            namespace: u64::from_be_bytes(bytes),
            sequence: AtomicU64::new(0),
        }))
    }
    pub fn request_id(&self) -> Option<HexId<32>> {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        HexId::parse(&format!("{:016x}{sequence:016x}", self.namespace))
    }
    pub fn shutdown(&self) {
        self.trace.shutdown(Duration::from_secs(3));
        self.access.shutdown(Duration::from_secs(1));
    }
}

pub struct GaugeGuard {
    metrics: Option<Arc<Metrics>>,
    gauge: super::metrics::Gauge,
}
impl GaugeGuard {
    pub fn new(owner: &Observability, gauge: super::metrics::Gauge) -> Self {
        let metrics = owner.metrics_enabled.then(|| owner.metrics.clone());
        if let Some(metrics) = &metrics {
            metrics.gauge_inc(gauge);
        }
        Self { metrics, gauge }
    }
}
impl std::fmt::Debug for GaugeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaugeGuard").finish_non_exhaustive()
    }
}
impl Drop for GaugeGuard {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            metrics.gauge_dec(self.gauge);
        }
    }
}
