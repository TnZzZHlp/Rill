use pingora::protocols::SocketDigest;
use pingora::upstreams::peer::UpstreamConnectionReservation;
use pingora::{Error, ErrorType};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Origin {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

#[derive(Default)]
struct State {
    origins: HashMap<Origin, Weak<Semaphore>>,
    connections: HashMap<usize, (Weak<SocketDigest>, OwnedSemaphorePermit)>,
}

pub struct Limiter {
    state: Mutex<State>,
    per_origin: usize,
    max_origins: usize,
}

impl Limiter {
    pub fn new(per_origin: usize, max_origins: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            per_origin,
            max_origins,
        })
    }
    pub fn reserve(
        self: &Arc<Self>,
        origin: Origin,
    ) -> pingora::Result<Box<dyn UpstreamConnectionReservation>> {
        if origin.host.len() > 253 || self.per_origin == 0 {
            return Err(capacity());
        }
        let mut state = self.state.lock().map_err(|_| capacity())?;
        state
            .connections
            .retain(|_, (digest, _)| digest.strong_count() != 0);
        state.origins.retain(|_, quota| quota.strong_count() != 0);
        let quota = if let Some(quota) = state.origins.get(&origin).and_then(Weak::upgrade) {
            quota
        } else {
            if state.origins.len() >= self.max_origins {
                return Err(capacity());
            }
            let quota = Arc::new(Semaphore::new(self.per_origin));
            state.origins.insert(origin, Arc::downgrade(&quota));
            quota
        };
        let permit = quota.try_acquire_owned().map_err(|_| capacity())?;
        Ok(Box::new(Reservation {
            limiter: Arc::clone(self),
            permit,
        }))
    }
}
fn capacity() -> Box<Error> {
    Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_CAPACITY").into_up()
}
struct Reservation {
    limiter: Arc<Limiter>,
    permit: OwnedSemaphorePermit,
}
impl UpstreamConnectionReservation for Reservation {
    fn attach(self: Box<Self>, digest: Arc<SocketDigest>) -> pingora::Result<()> {
        let Self { limiter, permit } = *self;
        let mut state = limiter.state.lock().map_err(|_| capacity())?;
        let key = Arc::as_ptr(&digest) as usize;
        if state.connections.contains_key(&key) {
            return Err(capacity());
        }
        state
            .connections
            .insert(key, (Arc::downgrade(&digest), permit));
        Ok(())
    }
}
static LIMITER: OnceLock<Arc<Limiter>> = OnceLock::new();
pub fn parameterized_global(
    per_origin: usize,
    max_origins: usize,
) -> Option<&'static Arc<Limiter>> {
    let limiter = LIMITER.get_or_init(|| Limiter::new(per_origin, max_origins));
    (limiter.per_origin == per_origin && limiter.max_origins == max_origins).then_some(limiter)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_sockets_and_handshakes_share_capacity_until_death() {
        let limiter = Limiter::new(1, 2);
        let origin = Origin {
            tls: true,
            host: "example.com".into(),
            port: 443,
        };
        let reservation = limiter.reserve(origin.clone()).unwrap();
        assert!(limiter.reserve(origin.clone()).is_err());
        let digest = Arc::new(SocketDigest::from_raw_fd(42));
        reservation.attach(digest.clone()).unwrap();
        assert!(limiter.reserve(origin.clone()).is_err());
        drop(digest);
        assert!(limiter.reserve(origin).is_ok());
    }
    #[tokio::test]
    async fn connector_reserves_before_new_transport_and_reclaims() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let limiter = Limiter::new(1, 2);
        let origin = Origin {
            tls: false,
            host: "example.com".into(),
            port: 80,
        };
        let mut peer = pingora::upstreams::peer::HttpPeer::new(address, false, String::new());
        peer.options.upstream_connection_reservation_hook =
            Some(Arc::new(move || limiter.reserve(origin.clone())));
        let connector = pingora::connectors::TransportConnector::new(None);
        let stream = connector.new_stream(&peer).await.unwrap();
        assert!(connector.new_stream(&peer).await.is_err());
        drop(stream);
        assert!(connector.new_stream(&peer).await.is_ok());
    }
}
