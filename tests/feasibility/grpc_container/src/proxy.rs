use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use openssl::x509::X509;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use pingora::server::{Server, configuration::ServerConf};
use pingora::upstreams::peer::HttpPeer;

use crate::Result;

#[derive(PartialEq, Eq)]
struct PolicyIdentity {
    protocol: u8,
    target: SocketAddr,
    tls: bool,
    server_name: String,
    trust: Vec<u8>,
}

#[derive(Default)]
struct PolicyRegistry(Vec<PolicyIdentity>);

impl PolicyRegistry {
    fn group_key(&mut self, policy: PolicyIdentity) -> Result<u64> {
        if let Some(index) = self.0.iter().position(|existing| existing == &policy) {
            return Ok(u64::try_from(index)?
                .checked_add(1)
                .ok_or("policy ID exhausted")?);
        }
        let id = u64::try_from(self.0.len())?
            .checked_add(1)
            .ok_or("policy ID exhausted")?;
        self.0.push(policy);
        Ok(id)
    }
}

struct GrpcProxy {
    peers: Vec<(String, HttpPeer, u64)>,
    connections: Mutex<HashMap<std::os::fd::RawFd, u64>>,
}

#[async_trait]
impl ProxyHttp for GrpcProxy {
    type CTX = u64;
    fn new_ctx(&self) -> u64 {
        0
    }
    async fn upstream_peer(
        &self,
        session: &mut Session,
        token: &mut u64,
    ) -> pingora::Result<Box<HttpPeer>> {
        let policy = session
            .req_header()
            .headers
            .get("x-pool-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("good");
        let peer = self
            .peers
            .iter()
            .find(|(name, _, _)| name == policy)
            .unwrap_or(&self.peers[0]);
        *token = peer.2;
        Ok(Box::new(peer.1.clone()))
    }

    async fn connected_to_upstream(
        &self,
        session: &mut Session,
        reused: bool,
        _: &HttpPeer,
        fd: std::os::fd::RawFd,
        _: Option<&pingora::protocols::Digest>,
        token: &mut u64,
    ) -> pingora::Result<()> {
        let policy = session
            .req_header()
            .headers
            .get("x-pool-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("good");
        let mut connections = self
            .connections
            .lock()
            .map_err(|_| pingora::Error::new_str("POOL_IDENTITY_LOCK_FAILURE"))?;
        if reused && connections.get(&fd) != Some(token) {
            eprintln!(
                "POOL_IDENTITY_MISMATCH policy={policy} reused={reused} fd={fd} expected={token} actual={:?}",
                connections.get(&fd)
            );
            return Err(pingora::Error::new_str("POOL_IDENTITY_MISMATCH"));
        }
        if !reused {
            connections.insert(fd, *token);
        }
        eprintln!("POOL policy={policy} reused={reused} fd={fd} token={token}");
        Ok(())
    }

    async fn logging(&self, _: &mut Session, error: Option<&pingora::Error>, _: &mut u64) {
        if let Some(error) = error {
            eprintln!("proxy failure: {error}");
        }
    }
}

pub fn run(args: &[String]) -> Result {
    if args.len() != 5 && args.len() != 9 {
        return Err("proxy LISTEN UPSTREAM h2c|tls-h2 CA NAME".into());
    }
    let listen: SocketAddr = args[0].parse()?;
    let upstream: SocketAddr = args[1].parse()?;
    if !listen.ip().is_loopback() || !upstream.ip().is_loopback() {
        return Err("acceptance proxy only allows loopback".into());
    }
    let tls = match args[2].as_str() {
        "h2c" => false,
        "tls-h2" => true,
        _ => return Err("invalid protocol".into()),
    };
    let mut peer = HttpPeer::new(upstream, tls, args[4].clone());
    peer.options.set_http_version(2, 2);
    peer.options.max_h2_streams = 16;
    peer.options.connection_timeout = Some(Duration::from_secs(3));
    peer.options.read_timeout = Some(Duration::from_secs(5));
    peer.options.write_timeout = Some(Duration::from_secs(5));
    if tls {
        peer.options.ca = Some(Arc::new(
            X509::stack_from_pem(&std::fs::read(&args[3])?)?.into_boxed_slice(),
        ));
        peer.options.verify_cert = true;
        peer.options.verify_hostname = true;
    }
    let mut peers = vec![("good".to_string(), peer.clone(), 1)];
    if args.len() == 9 {
        let ca = std::fs::read(&args[3])?;
        let other_ca = std::fs::read(&args[5])?;
        let first: SocketAddr = args[6].parse()?;
        let second: SocketAddr = args[7].parse()?;
        let h1_target: SocketAddr = args[8].parse()?;
        if [first, second, h1_target]
            .iter()
            .any(|addr| !addr.ip().is_loopback())
        {
            return Err("acceptance proxy only allows loopback".into());
        }
        let mut registry = PolicyRegistry::default();
        peers.clear();
        for (name, protocol, target, use_tls, server_name, trust) in [
            ("good", 2, upstream, true, args[4].as_str(), ca.as_slice()),
            (
                "wrong-ca",
                2,
                upstream,
                true,
                args[4].as_str(),
                other_ca.as_slice(),
            ),
            ("wrong-name", 2, upstream, true, "wrong.test", ca.as_slice()),
            ("h1", 1, upstream, true, args[4].as_str(), ca.as_slice()),
            ("target-one", 2, first, false, "", &[][..]),
            ("target-two", 2, second, false, "", &[][..]),
            ("plain-h1", 1, h1_target, false, "", &[][..]),
            ("plain-h2", 2, h1_target, false, "", &[][..]),
        ] {
            let mut candidate = HttpPeer::new(target, use_tls, server_name.to_string());
            candidate.options = peer.options.clone();
            candidate.options.ca = if use_tls {
                Some(Arc::new(X509::stack_from_pem(trust)?.into_boxed_slice()))
            } else {
                None
            };
            candidate.options.set_http_version(protocol, protocol);
            candidate.group_key = registry.group_key(PolicyIdentity {
                protocol,
                target,
                tls: use_tls,
                server_name: server_name.to_owned(),
                trust: trust.to_vec(),
            })?;
            eprintln!("POLICY {name} group={}", candidate.group_key);
            let token = candidate.group_key;
            peers.push((name.to_string(), candidate, token));
        }
    }
    if args.len() == 9 {
        let good_group = peers[0].1.group_key;
        let mut collision_peer = peers[1].1.clone();
        let token = peers[1].2;
        collision_peer.group_key = good_group;
        eprintln!("POLICY forced-collision group={good_group} token={token}");
        peers.push(("forced-collision".into(), collision_peer, token));
    }
    let configuration = ServerConf {
        threads: 2,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(2),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, configuration);
    let mut service = http_proxy_service(
        &server.configuration,
        GrpcProxy {
            peers,
            connections: Mutex::new(HashMap::new()),
        },
    );
    let mut options = pingora::apps::HttpServerOptions::default();
    options.h2c = true;
    service
        .app_logic_mut()
        .ok_or("missing proxy")?
        .server_options = Some(options);
    service.add_tcp(&listen.to_string());
    server.add_service(service);
    println!("proxy ready: {listen} -> {upstream} {}", args[2]);
    server.run_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_registry_compares_complete_identity() -> Result {
        let mut registry = PolicyRegistry::default();
        let identity = |trust: &[u8], protocol| PolicyIdentity {
            protocol,
            target: "127.0.0.1:1234".parse().expect("literal address"),
            tls: true,
            server_name: "grpc.test".into(),
            trust: trust.to_vec(),
        };
        let first = registry.group_key(identity(b"ca-one", 2))?;
        assert_eq!(first, registry.group_key(identity(b"ca-one", 2))?);
        assert_ne!(first, registry.group_key(identity(b"ca-two", 2))?);
        assert_ne!(first, registry.group_key(identity(b"ca-one", 1))?);
        Ok(())
    }
}
