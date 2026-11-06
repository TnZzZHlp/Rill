use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

static ACTIVE: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub fn try_active() -> Option<OwnedSemaphorePermit> {
    ACTIVE
        .get_or_init(|| Arc::new(Semaphore::new(128)))
        .clone()
        .try_acquire_owned()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::try_active;

    #[test]
    fn active_capacity_is_bounded_and_released() {
        let mut permits: Vec<_> = (0..128).map(|_| try_active().unwrap()).collect();
        assert!(try_active().is_none());
        permits.pop();
        assert!(try_active().is_some());
    }
}
