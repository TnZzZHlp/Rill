use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use pingora::upstreams::peer::{ALPN, HttpPeer};

use super::dns::{self, Dns};
use super::upstream_tls;

use crate::config::model::{Action, Route, Server};
use crate::config::{ConfigError, Result};

#[derive(Debug)]
pub struct PreparedPeer {
    dns: Dns,
    _leases: Vec<Arc<super::pool_policy::PoolLease>>,
    pub(crate) trust: Option<Arc<super::trust::store::TrustStore>>,
    pub urlhost: String,
    pub protocol: crate::config::model::UpstreamProtocol,
    pub tls: bool,
    pub connect: Duration,
    pub read: Duration,
    pub request: Duration,
    pub idle: Duration,
}

impl PreparedPeer {
    pub(crate) fn prepare(
        route: &Route,
        server: &Server,
        trust: Option<Arc<super::trust::store::TrustStore>>,
    ) -> Result<Self> {
        let Action::Proxy(upstream) = &route.action else {
            return Err(ConfigError::new(
                "INTERNAL",
                "proxy",
                "proxy action required",
                &route.action_origin,
            ));
        };
        let addresses = dns::resolve(&upstream.host, upstream.port, server, &route.action_origin)?;
        let parameters = &route.parameters;
        let initial_addresses = addresses.clone();
        let mut prepared = Self {
            _leases: Vec::new(),
            dns: Dns::new(
                upstream.host.clone(),
                upstream.port,
                server,
                &route.action_origin,
                addresses,
            ),
            trust: if upstream.tls { trust } else { None },
            urlhost: upstream.host.clone(),
            protocol: route.proxy_protocol,
            tls: upstream.tls,
            connect: parameters
                .connect_timeout
                .as_ref()
                .map_or(Duration::from_secs(5), |value| value.value),
            read: parameters
                .read_timeout
                .as_ref()
                .map_or(Duration::from_secs(60), |value| value.value),
            request: parameters
                .request_timeout
                .as_ref()
                .map_or(Duration::from_secs(300), |value| value.value),
            idle: parameters
                .idle_timeout
                .as_ref()
                .map_or(Duration::from_secs(60), |value| value.value),
        };
        for (index, address) in initial_addresses.iter().enumerate() {
            let peer = prepared.peer(&initial_addresses, index).ok_or_else(|| {
                ConfigError::new(
                    "INTERNAL",
                    "proxy",
                    "missing prepared upstream",
                    &route.action_origin,
                )
            })?;
            let lease = super::pool_policy::registry()
                .acquire(prepared.pool_identity(*address, &peer))
                .map_err(|_| {
                    ConfigError::new(
                        "POLICY_LIMIT",
                        "proxy",
                        "upstream pool identity capacity",
                        &route.action_origin,
                    )
                })?;
            prepared._leases.push(lease);
        }
        Ok(prepared)
    }

    pub(crate) fn pool_identity(
        &self,
        target: SocketAddr,
        peer: &HttpPeer,
    ) -> super::pool_policy::PoolIdentity {
        super::pool_policy::PoolIdentity {
            dynamic: false,
            protocol: self.protocol,
            target,
            tls: self.tls,
            tls_name: self.tls.then(|| self.urlhost.clone()),
            trust: self.trust.clone(),
            connect_timeout: self.connect,
            read_timeout: self.read,
            idle_timeout: self.idle,
            max_h2_streams: peer.options.max_h2_streams,
        }
    }

    pub async fn addresses(
        &self,
    ) -> std::result::Result<Arc<Vec<SocketAddr>>, dns::ResolutionFailed> {
        self.dns.addresses().await
    }

    pub fn peer(&self, addresses: &[SocketAddr], attempt: usize) -> Option<HttpPeer> {
        let mut peer = HttpPeer::new(
            *addresses.get(attempt)?,
            self.tls,
            if self.tls {
                self.urlhost.clone()
            } else {
                String::new()
            },
        );
        peer.options.verify_cert = true;
        peer.options.verify_hostname = true;
        peer.options.alternative_cn = None;
        peer.options.alpn = if self.protocol != crate::config::model::UpstreamProtocol::Http1 {
            ALPN::H2
        } else {
            ALPN::H1
        };
        if self.protocol != crate::config::model::UpstreamProtocol::Http1 {
            peer.options.max_h2_streams = 16;
        }
        peer.options.upstream_verify_context =
            self.trust.as_ref().map(|trust| Arc::clone(&trust.context));
        if self.tls
            && let Ok(ip) = self.urlhost.parse()
        {
            peer.options.verify_hostname = false;
            peer.options.upstream_tls_handshake_complete_hook = Some(upstream_tls::ip_hook(ip));
        }
        peer.options.connection_timeout = Some(self.connect);
        peer.options.total_connection_timeout = Some(self.connect);
        peer.options.read_timeout = Some(self.read);
        peer.options.write_timeout = Some(self.idle);
        peer.options.idle_timeout = Some(self.idle);
        Some(peer)
    }
}
