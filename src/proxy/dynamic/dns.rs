use super::address::approve_candidates;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Copy)]
pub enum Failure {
    Invalid,
    Denied,
    Capacity,
    Unavailable,
}

fn interfaces() -> Result<Vec<IpAddr>, Failure> {
    let mut addresses = Vec::new();
    for interface in nix::ifaddrs::getifaddrs().map_err(|_| Failure::Unavailable)? {
        if let Some(addr) = interface.address {
            let ip = addr
                .as_sockaddr_in()
                .map(|a| IpAddr::V4(a.ip()))
                .or_else(|| addr.as_sockaddr_in6().map(|a| IpAddr::V6(a.ip())));
            if let Some(ip) = ip {
                addresses.push(ip);
            }
        }
    }
    if addresses.is_empty() {
        return Err(Failure::Unavailable);
    }
    Ok(addresses)
}

fn verify(candidates: &[SocketAddr], listeners: &[SocketAddr]) -> Result<Vec<SocketAddr>, Failure> {
    approve_candidates(candidates, listeners, &interfaces()?).map_err(|_| Failure::Denied)
}

pub async fn resolve(url: &Url, listeners: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, Failure> {
    let host = url
        .host_str()
        .ok_or(Failure::Invalid)?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url.port_or_known_default().ok_or(Failure::Invalid)?;
    let permit = super::super::dns::resolver_capacity()
        .try_acquire_owned()
        .map_err(|_| Failure::Capacity)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut candidates = Vec::new();
        for addr in (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| Failure::Unavailable)?
        {
            if candidates.len() >= 64 {
                return Err(Failure::Capacity);
            }
            candidates.push(addr);
        }
        verify(&candidates, &listeners)
    });
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?
}

pub async fn revalidate(
    candidates: Vec<SocketAddr>,
    listeners: Vec<SocketAddr>,
) -> Result<(), Failure> {
    let permit = super::super::dns::resolver_capacity()
        .try_acquire_owned()
        .map_err(|_| Failure::Capacity)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify(&candidates, &listeners).map(|_| ())
    });
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?
}
