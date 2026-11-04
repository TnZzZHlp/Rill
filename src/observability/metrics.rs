use std::fmt::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const ROUTE_SLOTS: usize = 130;
pub const MAX_SERIES: usize = 63 + ROUTE_SLOTS * 16 + 1 + 3 + 5 + 5 + 1 + 4;
pub const MAX_RENDER_BYTES: usize = 1024 * 1024;
const BOUNDS_NS: [u64; 12] = [
    1_000_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    30_000_000_000,
];
const BOUNDS_TEXT: [&str; 12] = [
    "0.001", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "30",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity([u8; 16]);
impl Identity {
    pub fn from_config(canonical_site: &str, route_pattern: &str) -> Self {
        let mut hash = openssl::sha::Sha256::new();
        hash.update(&(canonical_site.len() as u64).to_be_bytes());
        hash.update(canonical_site.as_bytes());
        hash.update(&(route_pattern.len() as u64).to_be_bytes());
        hash.update(route_pattern.as_bytes());
        let digest = hash.finish();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        Self(id)
    }
    pub fn hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(32);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        s
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteId(usize);
impl RouteId {
    pub const UNKNOWN: Self = Self(0);
    pub const OVERFLOW: Self = Self(ROUTE_SLOTS - 1);
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Options,
    Patch,
    Connect,
    Other,
}
impl Method {
    fn index(self) -> usize {
        self as usize
    }
}
const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "CONNECT", "OTHER",
];
const STATUSES: [&str; 7] = ["none", "1xx", "2xx", "3xx", "4xx", "5xx", "other"];
fn status_index(status: Option<u16>) -> usize {
    match status {
        None => 0,
        Some(100..=199) => 1,
        Some(200..=299) => 2,
        Some(300..=399) => 3,
        Some(400..=499) => 4,
        Some(500..=599) => 5,
        Some(_) => 6,
    }
}
fn add(a: &AtomicU64, value: u64) {
    let mut old = a.load(Ordering::Relaxed);
    loop {
        let new = old.saturating_add(value);
        match a.compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => old = actual,
        }
    }
}
fn sub(a: &AtomicU64) {
    let mut old = a.load(Ordering::Relaxed);
    loop {
        match a.compare_exchange_weak(
            old,
            old.saturating_sub(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => old = actual,
        }
    }
}
struct RouteMetrics {
    completed: AtomicU64,
    buckets: [AtomicU64; 13],
    sum_ns: AtomicU64,
}
impl RouteMetrics {
    fn new() -> Self {
        Self {
            completed: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ns: AtomicU64::new(0),
        }
    }
}
#[derive(Clone, Copy)]
pub enum Gauge {
    InflightRequests,
    ActiveTransports,
    ActiveH2Streams,
}
#[derive(Clone, Copy)]
pub enum UpstreamAttempt {
    Started,
    New,
    Reused,
    Fail,
    TlsFail,
}
#[derive(Clone, Copy)]
pub enum Reload {
    Success,
    Fail,
    Busy,
    Timeout,
    Shutdown,
}
#[derive(Clone, Copy)]
pub enum Diagnostic {
    LogQueueDrop,
    TraceQueueDrop,
    LogExportFailure,
    TraceExportFailure,
}

pub struct Metrics {
    identities: Mutex<[Option<Identity>; 128]>,
    completed: [AtomicU64; 63],
    routes: [RouteMetrics; ROUTE_SLOTS],
    body_bytes: AtomicU64,
    gauges: [AtomicU64; 3],
    upstream: [AtomicU64; 5],
    reload: [AtomicU64; 5],
    generation: AtomicU64,
    diagnostics: [AtomicU64; 4],
}
impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
impl Metrics {
    pub fn new() -> Self {
        Self {
            identities: Mutex::new([None; 128]),
            completed: std::array::from_fn(|_| AtomicU64::new(0)),
            routes: std::array::from_fn(|_| RouteMetrics::new()),
            body_bytes: AtomicU64::new(0),
            gauges: std::array::from_fn(|_| AtomicU64::new(0)),
            upstream: std::array::from_fn(|_| AtomicU64::new(0)),
            reload: std::array::from_fn(|_| AtomicU64::new(0)),
            generation: AtomicU64::new(0),
            diagnostics: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    pub fn bind(&self, ids: &[Identity]) -> Vec<RouteId> {
        let mut slots = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ids.iter()
            .map(|id| {
                if let Some(i) = slots.iter().position(|v| *v == Some(*id)) {
                    return RouteId(i + 1);
                }
                if let Some(i) = slots.iter().position(Option::is_none) {
                    slots[i] = Some(*id);
                    return RouteId(i + 1);
                }
                RouteId::OVERFLOW
            })
            .collect()
    }
    pub fn route_label(&self, id: RouteId) -> String {
        match id.0 {
            0 => "unknown".into(),
            129 => "overflow".into(),
            i => self
                .identities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[i - 1]
                .map_or_else(|| "unknown".into(), Identity::hex),
        }
    }
    pub fn complete(
        &self,
        route: RouteId,
        method: Method,
        status: Option<u16>,
        duration: Duration,
        body_bytes: Option<u64>,
    ) {
        add(
            &self.completed[method.index() * 7 + status_index(status)],
            1,
        );
        let r = &self.routes[route.0];
        let ns = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let bucket = BOUNDS_NS
            .iter()
            .position(|bound| ns <= *bound)
            .unwrap_or(BOUNDS_NS.len());
        add(&r.buckets[bucket], 1);
        add(&r.sum_ns, ns);
        add(&r.completed, 1);
        if let Some(bytes) = body_bytes {
            add(&self.body_bytes, bytes);
        }
    }
    pub fn gauge_inc(&self, gauge: Gauge) {
        add(&self.gauges[gauge as usize], 1);
    }
    pub fn gauge_dec(&self, gauge: Gauge) {
        sub(&self.gauges[gauge as usize]);
    }
    pub fn upstream(&self, value: UpstreamAttempt) {
        add(&self.upstream[value as usize], 1);
    }
    pub fn reload(&self, value: Reload) {
        add(&self.reload[value as usize], 1);
    }
    pub fn set_generation(&self, value: u64) {
        self.generation.store(value, Ordering::Relaxed);
    }
    pub fn diagnostic(&self, value: Diagnostic) {
        add(&self.diagnostics[value as usize], 1);
    }
    pub fn render(&self) -> Result<String, RenderError> {
        let mut out = Bounded(String::with_capacity(MAX_RENDER_BYTES));
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        for (m, method) in METHODS.iter().enumerate() {
            for (s, status) in STATUSES.iter().enumerate() {
                writeln!(
                    out,
                    "rill_requests_completed_total{{method=\"{method}\",status=\"{status}\"}} {}",
                    load(&self.completed[m * 7 + s])
                )
                .map_err(|_| RenderError)?;
            }
        }
        let slots = *self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (i, r) in self.routes.iter().enumerate() {
            let label = match i {
                0 => "unknown".to_owned(),
                129 => "overflow".to_owned(),
                _ => match slots[i - 1] {
                    Some(id) => id.hex(),
                    None => continue,
                },
            };
            let buckets: [u64; 13] = std::array::from_fn(|index| load(&r.buckets[index]));
            writeln!(
                out,
                "rill_route_completed_total{{route=\"{label}\"}} {}",
                load(&r.completed)
            )
            .map_err(|_| RenderError)?;
            let mut count = 0u64;
            for (bucket, bound) in buckets[..12].iter().zip(BOUNDS_TEXT) {
                count = count.saturating_add(*bucket);
                writeln!(
                    out,
                    "rill_route_duration_seconds_bucket{{route=\"{label}\",le=\"{bound}\"}} {count}"
                )
                .map_err(|_| RenderError)?;
            }
            count = count.saturating_add(buckets[12]);
            writeln!(out, "rill_route_duration_seconds_bucket{{route=\"{label}\",le=\"+Inf\"}} {count}\nrill_route_duration_seconds_sum{{route=\"{label}\"}} {:.9}\nrill_route_duration_seconds_count{{route=\"{label}\"}} {count}", load(&r.sum_ns) as f64 / 1_000_000_000.0).map_err(|_| RenderError)?;
        }
        writeln!(out, "rill_body_bytes_total {}", load(&self.body_bytes))
            .map_err(|_| RenderError)?;
        for (name, value) in [
            "inflight_requests",
            "active_transports",
            "active_h2_streams",
        ]
        .iter()
        .zip(&self.gauges)
        {
            writeln!(out, "rill_{name} {}", load(value)).map_err(|_| RenderError)?;
        }
        for (name, value) in ["started", "new", "reused", "fail", "tls_fail"]
            .iter()
            .zip(&self.upstream)
        {
            writeln!(
                out,
                "rill_upstream_attempts_total{{result=\"{name}\"}} {}",
                load(value)
            )
            .map_err(|_| RenderError)?;
        }
        for (name, value) in ["success", "fail", "busy", "timeout", "shutdown"]
            .iter()
            .zip(&self.reload)
        {
            writeln!(
                out,
                "rill_reload_total{{result=\"{name}\"}} {}",
                load(value)
            )
            .map_err(|_| RenderError)?;
        }
        writeln!(out, "rill_generation {}", load(&self.generation)).map_err(|_| RenderError)?;
        for (name, value) in [
            "log_queue_drop",
            "trace_queue_drop",
            "log_export_failure",
            "trace_export_failure",
        ]
        .iter()
        .zip(&self.diagnostics)
        {
            writeln!(out, "rill_{name}_total {}", load(value)).map_err(|_| RenderError)?;
        }
        Ok(out.0)
    }
}
#[derive(Debug)]
pub struct RenderError;
struct Bounded(String);
impl Write for Bounded {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > MAX_RENDER_BYTES - self.0.len() {
            return Err(fmt::Error);
        }
        self.0.push_str(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slots_never_reused() {
        let m = Metrics::new();
        let ids: Vec<_> = (0..129)
            .map(|i| Identity::from_config("site", &i.to_string()))
            .collect();
        let first = m.bind(&ids[..128]);
        assert_eq!(m.bind(&ids[128..])[0], RouteId::OVERFLOW);
        assert_eq!(m.bind(&ids[..1])[0], first[0]);
        assert_ne!(
            Identity::from_config("ab", "c"),
            Identity::from_config("a", "bc")
        );
    }
    #[test]
    fn concurrent_histogram_is_monotonic() {
        let metrics = std::sync::Arc::new(Metrics::new());
        let writer = std::sync::Arc::clone(&metrics);
        let thread = std::thread::spawn(move || {
            for i in 0..20_000 {
                let duration = if i % 2 == 0 {
                    Duration::from_micros(1)
                } else {
                    Duration::from_secs(60)
                };
                writer.complete(RouteId::UNKNOWN, Method::Get, Some(200), duration, None);
            }
        });
        for _ in 0..200 {
            let text = metrics.render().unwrap();
            let mut previous = 0;
            let mut buckets = 0;
            let mut infinity = None;
            let mut count = None;
            for line in text.lines() {
                if line.starts_with("rill_route_duration_seconds_bucket{route=\"unknown\",") {
                    let value: u64 = line.rsplit_once(' ').unwrap().1.parse().unwrap();
                    assert!(value >= previous);
                    previous = value;
                    buckets += 1;
                    if line.contains("le=\"+Inf\"") {
                        infinity = Some(value);
                    }
                } else if line.starts_with("rill_route_duration_seconds_count{route=\"unknown\"}") {
                    count = Some(line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap());
                }
            }
            assert_eq!(buckets, 13);
            assert!(infinity.is_some());
            assert_eq!(infinity, count);
        }
        thread.join().unwrap();
    }
    #[test]
    fn saturated_render_fits() {
        let m = Metrics::new();
        let ids: Vec<_> = (0..128)
            .map(|i| Identity::from_config("site", &i.to_string()))
            .collect();
        m.bind(&ids);
        for a in &m.completed {
            a.store(u64::MAX, Ordering::Relaxed);
            add(a, 1);
        }
        for r in &m.routes {
            r.completed.store(u64::MAX, Ordering::Relaxed);
            r.sum_ns.store(u64::MAX, Ordering::Relaxed);
            for b in &r.buckets {
                b.store(u64::MAX, Ordering::Relaxed);
            }
        }
        for a in m
            .gauges
            .iter()
            .chain(&m.upstream)
            .chain(&m.reload)
            .chain(&m.diagnostics)
            .chain([&m.body_bytes, &m.generation])
        {
            a.store(u64::MAX, Ordering::Relaxed);
        }
        let text = m.render().unwrap();
        assert!(text.len() < MAX_RENDER_BYTES);
        assert_eq!(text.lines().count(), MAX_SERIES);
        assert!(text.contains("18446744073709551615"));
        assert!(!text.contains("NaN"));
    }
}
