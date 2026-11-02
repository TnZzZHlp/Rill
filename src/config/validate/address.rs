use std::net::{IpAddr, Ipv4Addr};

use crate::config::error::{ConfigError, Result};
use crate::config::model::{Server, Upstream};
use crate::config::source::Origin;

pub fn domain(value: &str, wildcard: bool) -> Option<String> {
    let lower = value
        .strip_suffix('.')
        .unwrap_or(value)
        .to_ascii_lowercase();
    let domain = if wildcard {
        lower.strip_prefix("*.").unwrap_or(&lower)
    } else {
        &lower
    };
    if !domain.is_ascii()
        || domain.is_empty()
        || domain.len() > 253
        || domain.parse::<IpAddr>().is_ok()
    {
        return None;
    }
    if !domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }
    Some(lower)
}

pub fn normalized_ip(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn permitted(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_link_local()
                && a != 0
                && a < 224
                && address != Ipv4Addr::BROADCAST
                && !(a == 192 && b == 0 && c <= 2)
                && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                && !(a == 203 && b == 0 && c == 113)
                && !(a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            address.is_loopback()
                || (segments[0] & 0xfe00 == 0xfc00)
                || ((segments[0] & 0xe000 == 0x2000)
                    && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
                    && segments[0] != 0x2002)
        }
    }
}

pub fn check_ip(address: IpAddr, server: &Server, origin: &Origin) -> Result<()> {
    let address = normalized_ip(address);
    if !permitted(address)
        || (!server.upstream_allow.is_empty()
            && !server
                .upstream_allow
                .iter()
                .any(|network| network.value.contains(address)))
    {
        return Err(ConfigError::new(
            "UPSTREAM_DENIED",
            "proxy",
            "upstream address is outside the permitted target range",
            origin,
        ));
    }
    Ok(())
}

pub fn upstream(value: &str, server: &Server, origin: &Origin) -> Result<Upstream> {
    let invalid = || {
        ConfigError::new(
            "CONFIG_TYPE",
            "proxy",
            "expected an HTTP/HTTPS origin without credentials, base path, query or fragment",
            origin,
        )
    };
    if !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['\\', '?', '#', '@'])
    {
        return Err(invalid());
    }
    let (scheme, rest) = value.split_once("://").ok_or_else(invalid)?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        _ => return Err(invalid()),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty() || authority.contains('/') {
        return Err(invalid());
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').ok_or_else(invalid)?;
        let host = &authority[1..end];
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(invalid());
        }
        let suffix = &authority[end + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or_else(invalid)?)
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            Some(_) => return Err(invalid()),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(port) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => port
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(invalid)?,
        Some(_) => return Err(invalid()),
        None => {
            if tls {
                443
            } else {
                80
            }
        }
    };
    let host = if let Ok(address) = host.parse::<IpAddr>() {
        check_ip(address, server, origin)?;
        address.to_string()
    } else {
        let host = domain(host, false).ok_or_else(invalid)?;
        if host.split('.').all(|part| {
            part.bytes().all(|byte| byte.is_ascii_digit())
                || part.strip_prefix("0x").is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        }) {
            return Err(invalid());
        }
        host
    };
    Ok(Upstream { tls, host, port })
}
