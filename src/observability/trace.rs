mod exporter;
mod processor;

use super::access::Outcome;
use super::metrics::{Identity, Method, Metrics};
use opentelemetry::{
    Context, KeyValue,
    trace::{Span as _, SpanKind, Status, Tracer as _, TracerProvider as _},
};
use opentelemetry_sdk::{
    Resource,
    trace::{Sampler, SdkTracerProvider},
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum TraceError {
    UnsupportedEnv,
    Startup,
}

pub struct TraceSink {
    provider: SdkTracerProvider,
    tracer: opentelemetry_sdk::trace::SdkTracer,
    sequence: AtomicU64,
}
impl TraceSink {
    pub fn new(endpoint: &str, metrics: Arc<Metrics>) -> Result<Self, TraceError> {
        if exporter::unsupported_env_present() {
            return Err(TraceError::UnsupportedEnv);
        }
        let processor =
            processor::Processor::new(endpoint, metrics).map_err(|_| TraceError::Startup)?;
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", "rill"),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            ])
            .build();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_max_attributes_per_span(16)
            .with_max_events_per_span(0)
            .with_max_links_per_span(0)
            .with_max_attributes_per_event(0)
            .with_max_attributes_per_link(0)
            .with_resource(resource)
            .with_span_processor(processor)
            .build();
        let tracer = provider.tracer("rill");
        Ok(Self {
            provider,
            tracer,
            sequence: AtomicU64::new(0),
        })
    }
    pub fn begin(&self, enabled: bool, per_mille: u16, method: Method) -> Option<TraceGuard> {
        if !enabled
            || per_mille == 0
            || self.sequence.fetch_add(1, Ordering::Relaxed) % 1000
                >= u64::from(per_mille.min(1000))
        {
            return None;
        }
        let mut span = self
            .tracer
            .span_builder("rill.request")
            .with_kind(SpanKind::Server)
            .start_with_context(&self.tracer, &Context::new());
        if !span.span_context().is_valid() {
            span.end();
            return None;
        }
        span.set_attribute(KeyValue::new("event", "rill.request"));
        Some(TraceGuard {
            span: Some(span),
            started: Instant::now(),
            method,
            generation_set: false,
        })
    }
    pub fn shutdown(&self, timeout: Duration) {
        let _ = self
            .provider
            .shutdown_with_timeout(timeout.min(Duration::from_secs(3)));
    }
}
fn method_name(method: Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        Method::Options => "OPTIONS",
        Method::Patch => "PATCH",
        Method::Connect => "CONNECT",
        Method::Other => "OTHER",
    }
}
fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Completed => "completed",
        Outcome::Error => "error",
        Outcome::Cancelled => "cancelled",
        Outcome::Timeout => "timeout",
    }
}

pub struct TraceGuard {
    span: Option<opentelemetry_sdk::trace::Span>,
    started: Instant,
    method: Method,
    generation_set: bool,
}
impl TraceGuard {
    pub fn set_method(&mut self, method: Method) {
        self.method = method;
    }
    pub fn ids(&self) -> Option<(String, String)> {
        self.span.as_ref().map(|s| {
            (
                s.span_context().trace_id().to_string(),
                s.span_context().span_id().to_string(),
            )
        })
    }
    pub fn traceparent(&self) -> Option<String> {
        self.ids()
            .map(|(trace, span)| format!("00-{trace}-{span}-01"))
    }
    pub fn update(&mut self, site: Option<Identity>, route: Option<Identity>, generation: u64) {
        if let Some(span) = self.span.as_mut() {
            if let Some(id) = site {
                span.set_attribute(KeyValue::new("site.id", id.hex()));
            }
            if let Some(id) = route {
                span.set_attribute(KeyValue::new("route.id", id.hex()));
            }
            if !self.generation_set {
                span.set_attribute(KeyValue::new(
                    "generation",
                    generation.min(i64::MAX as u64) as i64,
                ));
                self.generation_set = true;
            }
        }
    }
    pub fn finish(
        mut self,
        status: Option<u16>,
        body_bytes: Option<u64>,
        outcome: Outcome,
        duration: Duration,
    ) {
        self.complete(status, body_bytes, outcome, duration);
    }
    fn complete(
        &mut self,
        status: Option<u16>,
        body_bytes: Option<u64>,
        outcome: Outcome,
        duration: Duration,
    ) {
        if let Some(mut span) = self.span.take() {
            span.set_attribute(KeyValue::new("method", method_name(self.method)));
            if status.is_some_and(|value| value >= 500) || matches!(outcome, Outcome::Error) {
                span.set_status(Status::error("error"));
            }
            if let Some(status) = status {
                span.set_attribute(KeyValue::new("status", i64::from(status)));
            }
            span.set_attribute(KeyValue::new("body_bytes_known", body_bytes.is_some()));
            if let Some(bytes) = body_bytes {
                span.set_attribute(KeyValue::new(
                    "body_bytes",
                    bytes.min(i64::MAX as u64) as i64,
                ));
            }
            span.set_attribute(KeyValue::new("outcome", outcome_name(outcome)));
            span.set_attribute(KeyValue::new(
                "duration_ns",
                duration.as_nanos().min(i64::MAX as u128) as i64,
            ));
            span.end();
        }
    }
}
impl Drop for TraceGuard {
    fn drop(&mut self) {
        self.complete(None, None, Outcome::Cancelled, self.started.elapsed());
    }
}
