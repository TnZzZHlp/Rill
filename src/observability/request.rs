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

enum FileRoute {
    Legacy(usize),
    Rule(bool, usize),
}
struct FileObservation {
    snapshot: Arc<crate::runtime::state::RuntimeSnapshot>,
    site: usize,
    method: http::Method,
    uri: http::Uri,
    client_ip: Option<std::net::IpAddr>,
    route: Option<FileRoute>,
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
    file: Option<FileObservation>,
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
            file: None,
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
    pub fn file_site(
        &mut self,
        snapshot: Arc<crate::runtime::state::RuntimeSnapshot>,
        site: usize,
        method: http::Method,
        uri: http::Uri,
        client_ip: Option<std::net::IpAddr>,
    ) {
        if snapshot.file_logs.get(site).is_some_and(Option::is_some) {
            self.file = Some(FileObservation {
                snapshot,
                site,
                method,
                uri,
                client_ip,
                route: None,
            });
        }
    }
    pub fn file_client_ip(&mut self, client_ip: std::net::IpAddr) {
        if let Some(file) = &mut self.file {
            file.client_ip = Some(client_ip);
        }
    }
    pub fn file_legacy_route(&mut self, route: usize) {
        if let Some(file) = &mut self.file {
            file.route = Some(FileRoute::Legacy(route));
        }
    }
    pub fn file_rule(&mut self, rule: Option<(bool, usize)>) {
        if let Some(file) = &mut self.file {
            file.route = rule.map(|(error, index)| FileRoute::Rule(error, index));
        }
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
        if let Some(file) = &self.file {
            let site = &file.snapshot.configuration.sites[file.site];
            if let (Some(config), Some(sink)) =
                (&site.access_log, &file.snapshot.file_logs[file.site])
            {
                let generated;
                let route = match file.route {
                    Some(FileRoute::Legacy(index)) => site
                        .routes
                        .get(index)
                        .map(|route| route.pattern.value.as_str()),
                    Some(FileRoute::Rule(error, index)) => {
                        let rules = site.chain.as_ref().and_then(|chain| {
                            if error {
                                chain.errors.as_ref()
                            } else {
                                Some(&chain.rules)
                            }
                        });
                        if let Some(rule) = rules.and_then(|rules| rules.get(index)) {
                            if let Some(id) = &rule.id {
                                Some(id.as_str())
                            } else {
                                let offset = if error {
                                    site.chain.as_ref().map_or(0, |chain| chain.rules.len())
                                } else {
                                    0
                                };
                                generated = format!("rule-{}", offset + index + 1);
                                Some(generated.as_str())
                            }
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let record = super::file_config::FileLogRecord {
                    fields: config.fields,
                    timestamp: u64::try_from(timestamp).unwrap_or(u64::MAX),
                    site: Some(super::file_config::site_identity(
                        site.site_label.as_ref().map(|value| value.value.as_str()),
                        &site.domains[0].value,
                    )),
                    route,
                    method: file.method.as_str(),
                    status,
                    duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    bytes,
                    uri: Some(&file.uri),
                    client_ip: file.client_ip,
                };
                let _ = sink.submit(&record, super::log_queue::RotationPolicy::from(config));
            }
        }
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
