use super::access::{AccessSink, HexId};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

pub struct Observability {
    pub access: AccessSink,
    namespace: u64,
    sequence: AtomicU64,
}
impl std::fmt::Debug for Observability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observability").finish_non_exhaustive()
    }
}
impl Observability {
    pub fn new() -> std::io::Result<Arc<Self>> {
        let mut bytes = [0u8; 8];
        openssl::rand::rand_bytes(&mut bytes).map_err(std::io::Error::other)?;
        let access = AccessSink::new()?;
        Ok(Arc::new(Self {
            access,
            namespace: u64::from_be_bytes(bytes),
            sequence: AtomicU64::new(0),
        }))
    }
    pub fn request_id(&self) -> Option<HexId<32>> {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        HexId::parse(&format!("{:016x}{sequence:016x}", self.namespace))
    }
    pub fn shutdown(&self) {
        let start = Instant::now();
        let file_wait = Duration::from_millis(4750).saturating_sub(start.elapsed());
        if !super::log_registry::LogRegistry::process().shutdown(file_wait) {
            self.access.report_log_timeout();
        }
        let remaining = Duration::from_secs(5).saturating_sub(start.elapsed());
        self.access.shutdown(remaining.min(Duration::from_secs(1)));
    }
}
