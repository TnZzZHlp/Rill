use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use super::state::{HttpPreparer, RuntimeState};
use crate::config::Result;
use crate::config::prepare::build;

pub struct ReloadCoordinator {
    state: Arc<RuntimeState>,
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
}

impl ReloadCoordinator {
    pub fn new(state: Arc<RuntimeState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            admission: Arc::new(Semaphore::new(2)),
            execution: Arc::new(Semaphore::new(1)),
        })
    }

    pub async fn reload(&self) -> Result<u64> {
        let mut shutdown = self.state.shutdown_receiver();
        if *shutdown.borrow() {
            return Err(self
                .state
                .error("SHUTTING_DOWN", "runtime is shutting down"));
        }
        let admission = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.state.error("BUSY", "reload capacity is exhausted"))?;
        let execution = tokio::select! {
            biased;
            _ = shutdown.changed() => return Err(self.state.error("SHUTTING_DOWN", "runtime is shutting down")),
            permit = self.execution.clone().acquire_owned() => permit
                .map_err(|_| self.state.error("INTERNAL", "reload execution is unavailable"))?,
        };
        let permits = Arc::new((admission, execution));
        let blocking_permits = permits.clone();
        let entry = self.state.entry().to_path_buf();
        let deadline = Instant::now() + Duration::from_secs(30);
        let task = tokio::task::spawn_blocking(move || {
            let _permits = blocking_permits;
            build(&entry, &HttpPreparer)
        });
        let prepared = tokio::select! {
            biased;
            _ = shutdown.changed() => return Err(self.state.error("SHUTTING_DOWN", "runtime is shutting down")),
            _ = tokio::time::sleep_until(deadline.into()) => return Err(self.state.error("PREPARE_TIMEOUT", "reload preparation timed out")),
            result = task => result.map_err(|_| self.state.error("INTERNAL", "reload preparation failed"))??,
        };
        let result = self.state.publish(prepared, deadline);
        drop(permits);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn queued_reload_is_bounded_and_close_cancels_waiter()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let state = RuntimeState::new(build(Path::new("examples/config.kdl"), &HttpPreparer)?);
        let coordinator = ReloadCoordinator::new(state.clone());
        let admission = coordinator.admission.clone().acquire_owned().await?;
        let execution = coordinator.execution.clone().acquire_owned().await?;
        let queued_coordinator = coordinator.clone();
        let queued = tokio::spawn(async move { queued_coordinator.reload().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while coordinator.admission.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            coordinator
                .reload()
                .await
                .expect_err("third reload must be busy")
                .code,
            "BUSY"
        );
        state.close();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), queued)
                .await??
                .expect_err("queued reload must stop")
                .code,
            "SHUTTING_DOWN"
        );
        drop((admission, execution));
        assert_eq!(
            coordinator
                .reload()
                .await
                .expect_err("closed runtime must reject reload")
                .code,
            "SHUTTING_DOWN"
        );
        assert_eq!(state.snapshot().generation, 1);
        Ok(())
    }
}
