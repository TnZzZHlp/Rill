use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressDenied;

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 168 || (b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99)))
        || (a == 198 && ((b == 18 || b == 19) || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if ip.to_ipv4_mapped().is_some() {
                return false;
            }
            let s = ip.segments();
            // Admit only global unicast, excluding special-purpose and transition blocks.
            (s[0] & 0xe000 == 0x2000)
                && !(s[0] == 0x2001 && ((s[1] & 0xfe00 == 0) || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] & 0xf000 == 0)
        }
    }
}

pub fn approve_candidates(
    candidates: &[SocketAddr],
    listeners: &[SocketAddr],
    local_interfaces: &[IpAddr],
) -> Result<Vec<SocketAddr>, AddressDenied> {
    if candidates.is_empty() || candidates.len() > 64 {
        return Err(AddressDenied);
    }
    for candidate in candidates {
        if !public_ip(candidate.ip())
            || matches!(candidate, SocketAddr::V6(v6) if v6.scope_id() != 0)
        {
            return Err(AddressDenied);
        }
        if listeners.iter().any(|listener| {
            listener.port() == candidate.port()
                && (listener.ip() == candidate.ip()
                    || (listener.ip().is_unspecified()
                        && local_interfaces.contains(&candidate.ip())))
        }) {
            return Err(AddressDenied);
        }
    }
    Ok(candidates.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_special_mixed_and_self_candidates() {
        for raw in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "::ffff:8.8.8.8",
            "2001:db8::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
        ] {
            assert!(!public_ip(raw.parse().unwrap()), "{raw}");
        }
        let public: SocketAddr = "8.8.8.8:443".parse().unwrap();
        let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
        assert!(approve_candidates(&[public, private], &[], &[]).is_err());
        assert!(
            approve_candidates(&[public], &["0.0.0.0:443".parse().unwrap()], &[public.ip()])
                .is_err()
        );
        assert!(approve_candidates(&[public], &[], &[]).is_ok());
    }
}
