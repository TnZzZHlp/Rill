use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use pingora::upstreams::peer::HttpPeer;

use crate::config::model::{Action, Route, Server};
use crate::config::{ConfigError, Result, check_ip};

#[derive(Debug)]
pub struct PreparedPeer {
    pub addresses: Vec<SocketAddr>,
    pub connect: Duration,
    pub read: Duration,
    pub request: Duration,
    pub idle: Duration,
}

impl PreparedPeer {
    pub fn prepare(route: &Route, server: &Server) -> Result<Self> {
        let Action::Proxy(upstream) = &route.action else {
            return Err(ConfigError::new(
                "CONFIG_UNSUPPORTED",
                "root",
                "static service is unavailable until stage 005",
                &route.action_origin,
            ));
        };
        if upstream.tls {
            return Err(ConfigError::new(
                "CONFIG_UNSUPPORTED",
                "proxy",
                "HTTPS upstreams are unavailable until stage 006",
                &route.action_origin,
            ));
        }
        let resolved = (upstream.host.as_str(), upstream.port)
            .to_socket_addrs()
            .map_err(|_| {
                ConfigError::new(
                    "RESOURCE_IO",
                    "proxy",
                    "cannot resolve upstream hostname",
                    &route.action_origin,
                )
            })?;
        let mut addresses = Vec::new();
        for address in resolved {
            check_ip(address.ip(), server, &route.action_origin)?;
            if !addresses.contains(&address) {
                addresses.push(address);
            }
            if addresses.len() > 64 {
                return Err(ConfigError::new(
                    "CONFIG_LIMIT",
                    "proxy",
                    "upstream resolved to more than 64 addresses",
                    &route.action_origin,
                ));
            }
        }
        if addresses.is_empty() {
            return Err(ConfigError::new(
                "RESOURCE_IO",
                "proxy",
                "upstream hostname has no addresses",
                &route.action_origin,
            ));
        }
        let parameters = &route.parameters;
        Ok(Self {
            addresses,
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

    pub fn peer(&self, attempt: usize) -> Option<HttpPeer> {
        let mut peer = HttpPeer::new(*self.addresses.get(attempt)?, false, String::new());
        peer.options.connection_timeout = Some(self.connect);
        peer.options.total_connection_timeout = Some(self.connect);
        peer.options.read_timeout = Some(self.read);
        peer.options.write_timeout = Some(self.idle);
        peer.options.idle_timeout = Some(self.idle);
        Some(peer)
    }
}
