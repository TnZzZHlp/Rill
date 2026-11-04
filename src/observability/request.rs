use super::access::{AccessRecord, HexId, OpaqueRoute, Outcome};
use super::metrics::{Gauge, Identity, Method, RouteId};
use super::owner::Observability;
use super::trace::TraceGuard;
use crate::config::model::ObservabilityPolicy;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
pub struct ObservationRoute {
    pub site: Identity,
    pub route: Identity,
    pub slot: RouteId,
}

pub struct RequestGuard {
    owner: Arc<Observability>,
    generation: u64,
    start: Instant,
    method: Method,
    route: Option<ObservationRoute>,
    selected: bool,
    timeout: Option<Arc<AtomicBool>>,
    finished: bool,
    trace: Option<TraceGuard>,
    request_id: Option<HexId<32>>,
}
impl RequestGuard {
    pub fn new(owner: Arc<Observability>, generation: u64, policy: &ObservabilityPolicy) -> Self {
        if owner.metrics_enabled {
            owner.metrics.gauge_inc(Gauge::InflightRequests);
        }
        let selected =
            policy.access_log && owner.access.should_sample(policy.access_sample_per_mille);
        let start = Instant::now();
        let mut trace =
            owner
                .trace
                .begin(policy.tracing, policy.trace_sample_per_mille, Method::Other);
        if let Some(trace) = &mut trace {
            trace.update(None, None, generation);
        }
        let request_id = if selected || trace.is_some() {
            owner.request_id()
        } else {
            None
        };
        Self {
            owner,
            generation,
            start,
            method: Method::Other,
            route: None,
            selected,
            timeout: None,
            finished: false,
            trace,
            request_id,
        }
    }
    pub fn method(&mut self, value: &str) {
        self.method = match value {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            "PATCH" => Method::Patch,
            "CONNECT" => Method::Connect,
            _ => Method::Other,
        };
        if let Some(trace) = &mut self.trace {
            trace.set_method(self.method);
        }
    }
    pub fn route(&mut self, route: ObservationRoute) {
        self.route = Some(route);
        if let Some(trace) = &mut self.trace {
            trace.update(
                Some(route.site),
                (route.slot != RouteId::OVERFLOW).then_some(route.route),
                self.generation,
            );
        }
    }
    pub fn traceparent(&self) -> Option<String> {
        self.trace.as_ref().and_then(TraceGuard::traceparent)
    }
    pub fn timeout_flag(&mut self, flag: Arc<AtomicBool>) {
        self.timeout = Some(flag);
    }
    pub fn finish(&mut self, status: Option<u16>, bytes: Option<u64>, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = self.start.elapsed();
        let trace_ids = self.trace.as_ref().and_then(TraceGuard::ids);
        if let Some(trace) = self.trace.take() {
            trace.finish(status, bytes, outcome, elapsed);
        }
        if self.owner.metrics_enabled {
            self.owner.metrics.complete(
                self.route.map_or(RouteId::UNKNOWN, |route| route.slot),
                self.method,
                status,
                elapsed,
                bytes,
            );
            self.owner.metrics.gauge_dec(Gauge::InflightRequests);
        }
        if self.selected {
            let route = self.route.map_or(OpaqueRoute::Unknown, |route| {
                if route.slot == RouteId::OVERFLOW {
                    OpaqueRoute::Overflow
                } else {
                    OpaqueRoute::Identity(route.route)
                }
            });
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let mut record = AccessRecord::new(
                u64::try_from(timestamp).unwrap_or(u64::MAX),
                self.generation,
                route,
                self.method,
                u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                outcome,
            );
            record.site = self.route.map_or(OpaqueRoute::Unknown, |route| {
                OpaqueRoute::Identity(route.site)
            });
            record.request_id = self.request_id;
            if let Some((trace_id, span_id)) = trace_ids {
                record.trace_id = HexId::parse(&trace_id);
                record.span_id = HexId::parse(&span_id);
            }
            record.status = status;
            record.body_bytes = bytes;
            self.owner.access.submit_selected(record);
        }
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        let outcome = if self
            .timeout
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            Outcome::Timeout
        } else {
            Outcome::Cancelled
        };
        self.finish(None, None, outcome);
    }
}
