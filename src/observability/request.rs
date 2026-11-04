use super::access::{AccessRecord, HexId, OpaqueRoute, Outcome};
use super::identity::{Identity, Method};
use super::owner::Observability;
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
    request_id: Option<HexId<32>>,
}
impl RequestGuard {
    pub fn new(owner: Arc<Observability>, generation: u64, policy: &ObservabilityPolicy) -> Self {
        let selected =
            policy.access_log && owner.access.should_sample(policy.access_sample_per_mille);
        let start = Instant::now();
        let request_id = if selected { owner.request_id() } else { None };
        Self {
            owner,
            generation,
            start,
            method: Method::Other,
            route: None,
            selected,
            timeout: None,
            finished: false,
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
    }
    pub fn route(&mut self, route: ObservationRoute) {
        self.route = Some(route);
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
        if self.selected {
            let route = self.route.map_or(OpaqueRoute::Unknown, |route| {
                OpaqueRoute::Identity(route.route)
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
