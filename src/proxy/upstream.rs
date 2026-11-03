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
    pub urlhost: String,
    pub tls: bool,
    pub connect: Duration,
    pub read: Duration,
    pub request: Duration,
    pub idle: Duration,
}

impl PreparedPeer {
    pub fn prepare(route: &Route, server: &Server) -> Result<Self> {
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
        Ok(Self {
            dns: Dns::new(
                upstream.host.clone(),
                upstream.port,
                server,
                &route.action_origin,
                addresses,
            ),
            urlhost: upstream.host.clone(),
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
        })
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
        peer.options.alpn = ALPN::H1;
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
