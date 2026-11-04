use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

use crate::config::model::Server;
use crate::config::source::Origin;
use crate::config::{ConfigError, Result, check_ip};

static RESOLVERS: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub struct ResolutionFailed;

type Candidates = std::result::Result<Arc<Vec<SocketAddr>>, ResolutionFailed>;

#[derive(Debug)]
pub struct Dns {
    host: String,
    port: u16,
    server: Server,
    origin: Origin,
    literal: bool,
    state: Mutex<(Instant, Candidates)>,
}

pub fn resolve(host: &str, port: u16, server: &Server, origin: &Origin) -> Result<Vec<SocketAddr>> {
    let resolved = (host, port).to_socket_addrs().map_err(|_| {
        ConfigError::new(
            "RESOURCE_IO",
            "proxy",
            "cannot resolve upstream hostname",
            origin,
        )
    })?;
    let mut addresses = Vec::new();
    for address in resolved {
        check_ip(address.ip(), server, origin)?;
        if !addresses.contains(&address) {
            addresses.push(address);
        }
        if addresses.len() > 64 {
            return Err(ConfigError::new(
                "CONFIG_LIMIT",
                "proxy",
                "upstream resolved to more than 64 addresses",
                origin,
            ));
        }
    }
    if addresses.is_empty() {
        return Err(ConfigError::new(
            "RESOURCE_IO",
            "proxy",
            "upstream hostname has no addresses",
            origin,
        ));
    }
    Ok(addresses)
}

impl Dns {
    pub fn new(
        host: String,
        port: u16,
        server: &Server,
        origin: &Origin,
        addresses: Vec<SocketAddr>,
    ) -> Self {
        Self {
            literal: host.parse::<std::net::IpAddr>().is_ok(),
            host,
            port,
            server: server.clone(),
            origin: origin.clone(),
            state: Mutex::new((Instant::now(), Ok(Arc::new(addresses)))),
        }
    }

    pub async fn addresses(&self) -> Candidates {
        let mut state = self.state.lock().await;
        let lifetime = if state.1.is_ok() {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(1)
        };
        if self.literal || state.0.elapsed() < lifetime {
            return state.1.clone();
        }
        let permit = RESOLVERS
            .get_or_init(|| Arc::new(Semaphore::new(16)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ResolutionFailed)?;
        let host = self.host.clone();
        let port = self.port;
        let server = self.server.clone();
        let origin = self.origin.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            resolve(&host, port, &server, &origin)
                .map(Arc::new)
                .map_err(|_| ResolutionFailed)
        })
        .await
        .unwrap_or(Err(ResolutionFailed));
        *state = (Instant::now(), result.clone());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::Path;

    use crate::config::model::Network;
    use crate::config::source::Located;

    #[tokio::test]
    async fn expired_candidates_do_not_survive_denied_refresh() {
        let origin = Origin::file(Path::new("dns-test.kdl"));
        let server = Server {
            origin: origin.clone(),
            http: Vec::new(),
            https: Vec::new(),
            watch: false,
            static_cache_bytes: 0,
            redirect_https: false,
            redirect_port: None,
            control_socket: Default::default(),
            log_level: String::new(),
            trusted_proxy: Vec::new(),
            upstream_allow: vec![Located::new(
                Network {
                    address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
                    prefix: 8,
                },
                origin.clone(),
            )],
        };

        let resolved: Vec<_> = ("localhost", 80)
            .to_socket_addrs()
            .expect("localhost must resolve in the test environment")
            .collect();
        assert!(!resolved.is_empty());
        assert!(
            resolved
                .iter()
                .all(|address| check_ip(address.ip(), &server, &origin).is_err())
        );
        assert_eq!(
            resolve("localhost", 80, &server, &origin)
                .expect_err("localhost candidates must be denied")
                .code,
            "UPSTREAM_DENIED"
        );

        let stale = SocketAddr::from(([127, 0, 0, 1], 80));
        let dns = Dns::new("localhost".into(), 80, &server, &origin, vec![stale]);
        dns.state.lock().await.0 = Instant::now() - Duration::from_secs(61);

        assert!(dns.addresses().await.is_err());
        assert!(dns.addresses().await.is_err());
    }
}
