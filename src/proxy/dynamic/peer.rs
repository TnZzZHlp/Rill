use super::{
    bridge::Ready,
    limiter::{self, Origin},
};
use crate::{
    config::model::UpstreamProtocol,
    proxy::{
        pool_policy::{self, PoolIdentity, PoolLease},
        trust::store::TrustStore,
    },
};
use pingora::{
    Error, ErrorType,
    upstreams::peer::{ALPN, HttpPeer},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};

pub(crate) fn make(
    ready: &Ready,
    address: SocketAddr,
    trust: Option<Arc<TrustStore>>,
) -> pingora::Result<(HttpPeer, Arc<PoolLease>)> {
    let tls = ready.url.scheme() == "https";
    let host = ready
        .url
        .host_str()
        .ok_or_else(|| Error::explain(ErrorType::InternalError, "DYNAMIC_TARGET_INVALID"))?
        .trim_matches(['[', ']'])
        .to_owned();
    let origin = Origin {
        tls,
        host: host.clone(),
        port: ready
            .url
            .port_or_known_default()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "DYNAMIC_TARGET_INVALID"))?,
    };
    let trust =
        if tls {
            Some(trust.ok_or_else(|| {
                Error::explain(ErrorType::InternalError, "DYNAMIC_TRUST_UNAVAILABLE")
            })?)
        } else {
            None
        };
    let mut peer = HttpPeer::new(address, tls, if tls { host.clone() } else { String::new() });
    peer.options.verify_cert = true;
    peer.options.verify_hostname = true;
    peer.options.alpn = ALPN::H1;
    peer.options.upstream_verify_context = trust.as_ref().map(|t| Arc::clone(&t.context));
    if tls && let Ok(ip) = host.parse() {
        peer.options.verify_hostname = false;
        peer.options.upstream_tls_handshake_complete_hook =
            Some(crate::proxy::upstream_tls::ip_hook(ip));
    }
    peer.options.connection_timeout = Some(Duration::from_secs(5));
    peer.options.total_connection_timeout = peer.options.connection_timeout;
    peer.options.read_timeout = Some(Duration::from_secs(60));
    peer.options.write_timeout = Some(Duration::from_secs(60));
    peer.options.idle_timeout = Some(Duration::from_secs(60));
    let identity = PoolIdentity {
        dynamic: true,
        protocol: UpstreamProtocol::Http1,
        target: address,
        tls,
        tls_name: Some(host),
        trust: trust.clone(),
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(60),
        max_h2_streams: peer.options.max_h2_streams,
    };
    let lease = pool_policy::registry()
        .acquire(identity)
        .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_CAPACITY"))?;
    peer.group_key = lease.token;
    let birth_lease = lease.clone();
    peer.options.upstream_connection_birth_hook = Some(Arc::new(move |digest| {
        pool_policy::registry()
            .birth(&digest, &birth_lease, trust.clone())
            .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_CAPACITY"))
    }));
    let limiter = limiter::parameterized_global(8, 4096)
        .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_CAPACITY"))?
        .clone();
    peer.options.upstream_connection_reservation_hook =
        Some(Arc::new(move || limiter.reserve(origin.clone())));
    Ok((peer, lease))
}
