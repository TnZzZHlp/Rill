use super::metrics::{Diagnostic, Identity, Method, Metrics};
use serde::{Serialize, ser::SerializeMap};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct HexId<const N: usize>([u8; N]);
impl<const N: usize> HexId<N> {
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != N
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut value = [0; N];
        value.copy_from_slice(s.as_bytes());
        Some(Self(value))
    }
}
impl<const N: usize> Serialize for HexId<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(std::str::from_utf8(&self.0).map_err(serde::ser::Error::custom)?)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum OpaqueRoute {
    Unknown,
    Overflow,
    Identity(Identity),
}
impl Serialize for OpaqueRoute {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => serializer.serialize_str("unknown"),
            Self::Overflow => serializer.serialize_str("overflow"),
            Self::Identity(id) => serializer.serialize_str(&id.hex()),
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Error,
    Cancelled,
    Timeout,
}
#[derive(Clone, Debug, Serialize)]
pub struct AccessRecord {
    event: &'static str,
    pub timestamp_ms: u64,
    pub request_id: Option<HexId<32>>,
    pub trace_id: Option<HexId<32>>,
    pub span_id: Option<HexId<16>>,
    pub generation: u64,
    pub site: OpaqueRoute,
    pub route: OpaqueRoute,
    pub method: Method,
    pub status: Option<u16>,
    pub body_bytes: Option<u64>,
    pub duration_ns: u64,
    pub outcome: Outcome,
}
impl AccessRecord {
    pub fn new(
        timestamp_ms: u64,
        generation: u64,
        route: OpaqueRoute,
        method: Method,
        duration_ns: u64,
        outcome: Outcome,
    ) -> Self {
        Self {
            event: "access",
            timestamp_ms,
            request_id: None,
            trace_id: None,
            span_id: None,
            generation,
            site: OpaqueRoute::Unknown,
            route,
            method,
            status: None,
            body_bytes: None,
            duration_ns,
            outcome,
        }
    }
}
struct RuntimeRecord {
    timestamp_ms: u128,
    level: &'static str,
    event: &'static str,
    fields: Vec<(String, String)>,
}
impl Serialize for RuntimeRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3 + self.fields.len()))?;
        map.serialize_entry("time_ms", &self.timestamp_ms)?;
        map.serialize_entry("level", self.level)?;
        map.serialize_entry("event", self.event)?;
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

enum LogRecord {
    Access(AccessRecord),
    Runtime(RuntimeRecord),
}

fn bounded_field(value: &str, limit: usize, budget: &mut usize) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        let raw = ch.len_utf8();
        let encoded = match ch {
            '"' | '\\' => 2,
            ch if ch.is_control() && u32::from(ch) <= 0x1f => 6,
            _ => raw,
        };
        if out.len() + raw > limit || encoded > *budget {
            break;
        }
        out.push(ch);
        *budget -= encoded;
    }
    out
}

struct Inner {
    sender: Mutex<Option<mpsc::SyncSender<LogRecord>>>,
    stop: Arc<AtomicBool>,
    sequence: AtomicU64,
    metrics: Arc<Metrics>,
    worker: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<mpsc::Receiver<()>>,
}
#[derive(Clone)]
pub struct AccessSink {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for AccessSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessSink").finish_non_exhaustive()
    }
}
impl AccessSink {
    pub fn new(metrics: Arc<Metrics>) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<LogRecord>(4096);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_metrics = Arc::clone(&metrics);
        let worker = thread::Builder::new()
            .name("rill-access".into())
            .spawn(move || {
                loop {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let next = receiver.recv_timeout(Duration::from_millis(50));
                    match next {
                        Ok(record) => {
                            let encoded = match record {
                                LogRecord::Access(record) => serde_json::to_vec(&record),
                                LogRecord::Runtime(record) => serde_json::to_vec(&record),
                            };
                            let result = encoded.map_err(io::Error::other).and_then(|mut bytes| {
                                if bytes.len() >= 4096 {
                                    return Err(io::Error::other("log record exceeds limit"));
                                }
                                bytes.push(b'\n');
                                io::stderr().lock().write_all(&bytes)
                            });
                            if result.is_err() {
                                worker_metrics.diagnostic(Diagnostic::LogExportFailure);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                for _ in 0..4096 {
                    if receiver.try_recv().is_err() {
                        break;
                    }
                    worker_metrics.diagnostic(Diagnostic::LogQueueDrop);
                }
                let _ = finished_tx.try_send(());
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                sender: Mutex::new(Some(sender)),
                stop,
                sequence: AtomicU64::new(0),
                metrics,
                worker: Mutex::new(Some(worker)),
                finished: Mutex::new(finished),
            }),
        })
    }
    pub fn should_sample(&self, per_thousand: u16) -> bool {
        per_thousand != 0
            && self.inner.sequence.fetch_add(1, Ordering::Relaxed) % 1000
                < u64::from(per_thousand.min(1000))
    }
    pub fn submit_selected(&self, record: AccessRecord) {
        self.submit_record(LogRecord::Access(record));
    }
    fn submit_record(&self, record: LogRecord) {
        let sent = match self.inner.sender.try_lock() {
            Ok(sender) => sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(record).is_ok()),
            Err(_) => false,
        };
        if !sent {
            self.inner.metrics.diagnostic(Diagnostic::LogQueueDrop);
        }
    }
    pub fn submit_runtime(
        &self,
        timestamp_ms: u128,
        level: &'static str,
        event: &'static str,
        fields: &[(&str, &str)],
    ) {
        if event.len() > 128
            || !event
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            self.inner.metrics.diagnostic(Diagnostic::LogQueueDrop);
            return;
        }
        let mut budget = 2800;
        let mut bounded = Vec::with_capacity(fields.len().min(16));
        for (key, value) in fields.iter().take(16) {
            bounded.push((
                bounded_field(key, 64, &mut budget),
                bounded_field(value, 256, &mut budget),
            ));
        }
        if fields.len() > 16 {
            self.inner.metrics.diagnostic(Diagnostic::LogQueueDrop);
        }
        self.submit_record(LogRecord::Runtime(RuntimeRecord {
            timestamp_ms,
            level,
            event,
            fields: bounded,
        }));
    }
    pub fn submit(&self, record: AccessRecord, per_thousand: u16) {
        if self.should_sample(per_thousand) {
            self.submit_selected(record);
        }
    }
    pub fn shutdown(&self, timeout: Duration) {
        {
            let mut sender = self
                .inner
                .sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sender.take();
        }
        let handle = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let done = self
                .inner
                .finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv_timeout(timeout.min(Duration::from_secs(1)))
                .is_ok();
            if done {
                let _ = handle.join();
            } else {
                self.inner.stop.store(true, Ordering::Release);
            }
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_record_preserves_schema_and_bounded_utf8() {
        let mut budget = 2800;
        let input = "\u{0001}\u{20000}".repeat(300);
        let fields: Vec<_> = (0..16)
            .map(|_| {
                (
                    bounded_field("field", 64, &mut budget),
                    bounded_field(&input, 256, &mut budget),
                )
            })
            .collect();
        let record = RuntimeRecord {
            timestamp_ms: 123,
            level: "warn",
            event: "request_timeout",
            fields,
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(bytes.len() < 4096);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["time_ms"], 123);
        assert_eq!(value["level"], "warn");
        assert_eq!(value["event"], "request_timeout");
    }
    #[test]
    fn controlled_record_has_no_raw_inputs() {
        assert!(HexId::<32>::parse("host.example/secret?token=123456").is_none());
        let mut record = AccessRecord::new(
            1,
            2,
            OpaqueRoute::Identity(Identity::from_config("host.example", "/secret?token")),
            Method::Get,
            3,
            Outcome::Completed,
        );
        record.site = OpaqueRoute::Identity(Identity::from_config("host.example", ""));
        let text = serde_json::to_string(&record).unwrap();
        assert!(!text.contains("host.example"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("token"));
        assert!(text.contains("\"body_bytes\":null"));
    }
}
