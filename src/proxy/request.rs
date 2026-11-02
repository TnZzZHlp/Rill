use std::net::{IpAddr, Ipv6Addr};

use pingora::http::RequestHeader;

use crate::config::model::Server;
use crate::config::{normalize_domain, normalized_ip};

#[derive(Debug)]
pub struct Authority {
    pub original: String,
    pub host: String,
    pub port: Option<u16>,
}

pub fn authority(request: &RequestHeader) -> Result<Authority, u16> {
    let values: Vec<_> = request.headers.get_all("host").iter().collect();
    if values.len() != 1 {
        return Err(400);
    }
    let original = values[0].to_str().map_err(|_| 400_u16)?;
    if original.is_empty()
        || !original.is_ascii()
        || original
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || original.contains(['@', ',', '/', '\\', '?', '#', '"'])
    {
        return Err(400);
    }
    let (host, port) = if original.starts_with('[') {
        let end = original.find(']').ok_or(400_u16)?;
        let host = &original[1..end];
        host.parse::<Ipv6Addr>().map_err(|_| 400_u16)?;
        let suffix = &original[end + 1..];
        (
            host,
            if suffix.is_empty() {
                None
            } else {
                Some(suffix.strip_prefix(':').ok_or(400_u16)?)
            },
        )
    } else {
        match original.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            Some(_) => return Err(400),
            None => (original, None),
        }
    };
    let port = match port {
        Some(port) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => Some(
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or(400_u16)?,
        ),
        Some(_) => return Err(400),
        None => None,
    };
    let host = if let Ok(address) = host.parse::<IpAddr>() {
        address.to_string()
    } else {
        normalize_domain(host, false).ok_or(400_u16)?
    };
    if request
        .uri
        .authority()
        .is_some_and(|value| value.as_str() != original)
    {
        return Err(400);
    }
    Ok(Authority {
        original: original.to_string(),
        host,
        port,
    })
}

#[derive(Debug)]
pub struct Forwarding {
    pub forwarded: String,
    pub for_header: String,
    pub host: String,
    pub proto: &'static str,
    pub port: u16,
}

pub fn forwarding(
    request: &RequestHeader,
    authority: &Authority,
    peer: IpAddr,
    local_port: u16,
    server: &Server,
) -> Result<Forwarding, u16> {
    let peer = normalized_ip(peer);
    let trusted = |address| {
        server
            .trusted_proxy
            .iter()
            .any(|network| network.value.contains(address))
    };
    let mut client = peer;
    let mut chain = vec![peer];
    let mut proto = "http";
    if trusted(peer) {
        let mut addresses = Vec::new();
        for value in request.headers.get_all("x-forwarded-for").iter() {
            let value = value.to_str().map_err(|_| 400_u16)?;
            for address in value.split(',') {
                let address = address.trim().parse::<IpAddr>().map_err(|_| 400_u16)?;
                if addresses.len() >= 32 {
                    return Err(400);
                }
                addresses.push(normalized_ip(address));
            }
        }
        let mut cut = addresses.len();
        for (index, address) in addresses.iter().enumerate().rev() {
            if !trusted(client) {
                break;
            }
            client = *address;
            cut = index;
        }
        chain = addresses[cut..]
            .iter()
            .copied()
            .chain(std::iter::once(peer))
            .collect();
        let values: Vec<_> = request
            .headers
            .get_all("x-forwarded-proto")
            .iter()
            .collect();
        if values.len() > 1 {
            return Err(400);
        }
        if let Some(value) = values.first() {
            proto = match value.to_str().map_err(|_| 400_u16)? {
                "http" => "http",
                "https" => "https",
                _ => return Err(400),
            };
        }
    }
    let port = authority.port.unwrap_or(if trusted(peer) {
        if proto == "https" { 443 } else { 80 }
    } else {
        local_port
    });
    let client = match client {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => format!("[{address}]"),
    };
    Ok(Forwarding {
        forwarded: format!(
            "for=\"{client}\";proto={proto};host=\"{}\"",
            authority.original
        ),
        for_header: chain
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        host: authority.original.clone(),
        proto,
        port,
    })
}

pub fn connection_names(request: &RequestHeader) -> Result<Vec<String>, u16> {
    let mut result = Vec::new();
    for value in request.headers.get_all("connection").iter() {
        let value = value.to_str().map_err(|_| 400_u16)?;
        for name in value.split(',') {
            let name = name.trim();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err(400);
            }
            let name = name.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "host" | "content-length" | "transfer-encoding"
            ) {
                return Err(400);
            }
            result.push(name);
        }
    }
    Ok(result)
}
