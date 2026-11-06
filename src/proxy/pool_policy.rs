use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::config::model::UpstreamProtocol;
use pingora::protocols::SocketDigest;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PoolIdentity {
    pub dynamic: bool,
    pub protocol: UpstreamProtocol,
    pub target: SocketAddr,
    pub tls: bool,
    pub tls_name: Option<String>,
    pub trust: Option<Arc<super::trust::store::TrustStore>>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_h2_streams: usize,
}

#[derive(Debug)]
pub(crate) struct PoolLease {
    pub token: u64,
    _identity: PoolIdentity,
}

#[derive(Debug)]
struct ConnectionRecord {
    digest: Weak<SocketDigest>,
    lease: Arc<PoolLease>,
    _connection_trust: Option<Arc<super::trust::store::TrustStore>>,
}

#[derive(Debug, Default)]
struct State {
    next_token: u64,
    identities: HashMap<PoolIdentity, Weak<PoolLease>>,
    connections: HashMap<usize, ConnectionRecord>,
}

#[derive(Debug)]
pub(crate) struct PoolRegistry {
    state: Mutex<State>,
    connection_limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PoolError {
    Capacity,
    TokenExhausted,
    Mismatch,
    Poisoned,
}

static REGISTRY: OnceLock<PoolRegistry> = OnceLock::new();

pub(crate) fn registry() -> &'static PoolRegistry {
    REGISTRY.get_or_init(|| PoolRegistry::new(4096))
}

impl PoolRegistry {
    pub fn new(connection_limit: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            connection_limit,
        }
    }

    pub fn collect_expired(&self) -> Result<(), PoolError> {
        let mut state = self.state.lock().map_err(|_| PoolError::Poisoned)?;
        state
            .connections
            .retain(|_, record| record.digest.strong_count() > 0);
        state.identities.retain(|_, lease| lease.strong_count() > 0);
        Ok(())
    }

    pub fn acquire(&self, identity: PoolIdentity) -> Result<Arc<PoolLease>, PoolError> {
        let mut state = self.state.lock().map_err(|_| PoolError::Poisoned)?;
        state
            .connections
            .retain(|_, record| record.digest.strong_count() > 0);
        state.identities.retain(|_, lease| lease.strong_count() > 0);
        if let Some(lease) = state.identities.get(&identity).and_then(Weak::upgrade) {
            return Ok(lease);
        }
        if state.identities.len() >= 4096 {
            return Err(PoolError::Capacity);
        }
        let token = state
            .next_token
            .checked_add(1)
            .ok_or(PoolError::TokenExhausted)?;
        state.next_token = token;
        let lease = Arc::new(PoolLease {
            token,
            _identity: identity.clone(),
        });
        state.identities.insert(identity, Arc::downgrade(&lease));
        Ok(lease)
    }

    pub fn birth(
        &self,
        digest: &Arc<SocketDigest>,
        lease: &Arc<PoolLease>,
        connection_trust: Option<Arc<super::trust::store::TrustStore>>,
    ) -> Result<(), PoolError> {
        let mut state = self.state.lock().map_err(|_| PoolError::Poisoned)?;
        state
            .connections
            .retain(|_, record| record.digest.strong_count() > 0);
        let key = Arc::as_ptr(digest) as usize;
        if state.connections.contains_key(&key) {
            return Err(PoolError::Mismatch);
        }
        if state.connections.len() >= self.connection_limit {
            return Err(PoolError::Capacity);
        }
        state.connections.insert(
            key,
            ConnectionRecord {
                digest: Arc::downgrade(digest),
                lease: Arc::clone(lease),
                _connection_trust: connection_trust,
            },
        );
        Ok(())
    }

    pub fn verify(&self, digest: &Arc<SocketDigest>, lease: &PoolLease) -> Result<(), PoolError> {
        let state = self.state.lock().map_err(|_| PoolError::Poisoned)?;
        let key = Arc::as_ptr(digest) as usize;
        let record = state.connections.get(&key).ok_or(PoolError::Mismatch)?;
        if record
            .digest
            .upgrade()
            .is_some_and(|live| Arc::ptr_eq(&live, digest))
            && record.lease.token == lease.token
        {
            Ok(())
        } else {
            Err(PoolError::Mismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(port: u16) -> PoolIdentity {
        PoolIdentity {
            dynamic: false,
            protocol: UpstreamProtocol::H2c,
            target: SocketAddr::from(([127, 0, 0, 1], port)),
            tls: false,
            tls_name: None,
            trust: None,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            max_h2_streams: 1,
        }
    }

    #[test]
    fn connection_birth_is_bound_to_live_digest_and_exact_lease() {
        let registry = PoolRegistry::new(1);
        let first = registry.acquire(identity(1)).expect("first identity");
        let equal = registry.acquire(identity(1)).expect("same identity");
        let other = registry.acquire(identity(2)).expect("other identity");
        assert_eq!(first.token, equal.token);
        let digest = Arc::new(SocketDigest::from_raw_fd(42));
        registry.birth(&digest, &first, None).expect("birth");
        assert_eq!(registry.verify(&digest, &other), Err(PoolError::Mismatch));
        assert_eq!(registry.verify(&digest, &equal), Ok(()));
        let reused_fd = Arc::new(SocketDigest::from_raw_fd(42));
        assert_eq!(
            registry.verify(&reused_fd, &first),
            Err(PoolError::Mismatch)
        );
        assert_eq!(
            registry.birth(&reused_fd, &first, None),
            Err(PoolError::Capacity)
        );
        drop(digest);
        registry
            .birth(&reused_fd, &other, None)
            .expect("reclaimed birth");
        assert_eq!(registry.verify(&reused_fd, &other), Ok(()));
    }

    #[test]
    fn tokens_are_not_reused_after_lease_and_connection_die() {
        let registry = PoolRegistry::new(1);
        let first = registry.acquire(identity(1)).expect("first identity");
        let token = first.token;
        drop(first);
        let next = registry.acquire(identity(1)).expect("new identity");
        assert!(next.token > token);
    }
    #[test]
    fn collect_expired_releases_dead_connection_lease_without_new_acquire() {
        let registry = PoolRegistry::new(2);
        let lease = registry.acquire(identity(1)).expect("lease");
        let weak = Arc::downgrade(&lease);
        let socket = Arc::new(SocketDigest::from_raw_fd(7));
        registry.birth(&socket, &lease, None).expect("birth");
        drop(lease);
        registry.collect_expired().expect("collect live");
        assert!(weak.upgrade().is_some());
        drop(socket);
        registry.collect_expired().expect("collect dead");
        assert!(weak.upgrade().is_none());
        assert!(registry.state.lock().expect("lock").identities.is_empty());
    }
    #[test]
    fn identity_capacity_counts_live_leases_and_reclaims_after_drop() {
        let registry = PoolRegistry::new(1);
        let mut leases = Vec::new();
        for port in 0..4096 {
            leases.push(registry.acquire(identity(port)).expect("capacity"));
        }
        assert_eq!(
            registry.acquire(identity(4096)).err(),
            Some(PoolError::Capacity)
        );
        leases.pop();
        registry.collect_expired().expect("collect");
        assert!(registry.acquire(identity(4096)).is_ok());
    }
}
