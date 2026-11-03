pub mod resources;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use super::reload::ReloadCoordinator;
use super::state::RuntimeState;
use crate::config::Result;
use crate::observability::logging::Level;

type ReloadFuture = Pin<Box<dyn Future<Output = Result<u64>> + Send>>;

pub async fn run(
    state: Arc<RuntimeState>,
    coordinator: Arc<ReloadCoordinator>,
    mut shutdown: watch::Receiver<bool>,
) -> std::result::Result<(), &'static str> {
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut first: Option<Instant> = None;
    let mut last: Option<Instant> = None;
    let mut retry_after = Instant::now();
    let mut recovery_armed = false;
    let mut inflight: Option<ReloadFuture> = None;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            result = async {
                match inflight.as_mut() {
                    Some(future) => future.await,
                    None => std::future::pending().await,
                }
            } => {
                inflight = None;
                recovery_armed = false;
                match result {
                    Ok(generation) => state.snapshot().logger.event(Level::Info, "watch_reload_succeeded", &[("generation", &generation.to_string())]),
                    Err(error) => {
                        state.snapshot().logger.event(Level::Warn, "watch_reload_failed", &[("code", error.code)]);
                        if matches!(error.code, "BUSY" | "RESOURCE_CHANGED" | "PREPARE_TIMEOUT") {
                            let now = Instant::now();
                            first.get_or_insert(now);
                            last = Some(now);
                            retry_after = now + Duration::from_secs(1);
                        }
                    }
                }
            }
            _ = interval.tick() => {
                let now = Instant::now();
                let (dirty, recovery, code) = state.poll_watch();
                if let Some(code) = code {
                    state.snapshot().logger.event(Level::Warn, "watch_event_error", &[("code", code)]);
                }
                if dirty {
                    first.get_or_insert(now);
                    last = Some(now);
                }
                if recovery && inflight.is_none() && !recovery_armed {
                    first.get_or_insert(now);
                    last.get_or_insert(now);
                    retry_after = retry_after.max(now + Duration::from_secs(1));
                    recovery_armed = true;
                } else if !recovery {
                    recovery_armed = false;
                }
                if inflight.is_none() && now >= retry_after && first.zip(last).is_some_and(|(start, latest)| now.duration_since(latest) >= Duration::from_millis(250) || now.duration_since(start) >= Duration::from_secs(2)) {
                    first = None;
                    last = None;
                    let coordinator = coordinator.clone();
                    inflight = Some(Box::pin(async move { coordinator.reload().await }));
                }
            }
        }
    }
    Ok(())
}
