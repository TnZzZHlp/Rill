use percent_encoding::percent_decode_str;
use url::{Host, Url};

const MAX_TARGET_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTarget;

pub fn parse(raw: &str, allowed_ports: &[u16]) -> Result<Url, InvalidTarget> {
    if raw.len() > MAX_TARGET_BYTES * 3 || !valid_percent_encoding(raw) {
        return Err(InvalidTarget);
    }
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| InvalidTarget)?;
    if decoded.len() > MAX_TARGET_BYTES
        || decoded.bytes().any(|b| b.is_ascii_control() || b == b'\\')
        || decoded.contains('#')
    {
        return Err(InvalidTarget);
    }
    let authority = decoded
        .strip_prefix("http://")
        .or_else(|| decoded.strip_prefix("https://"))
        .and_then(|rest| rest.split(['/', '?']).next())
        .ok_or(InvalidTarget)?;
    if authority.contains(['@', '%']) || authority.is_empty() {
        return Err(InvalidTarget);
    }
    let url = Url::parse(&decoded).map_err(|_| InvalidTarget)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(InvalidTarget);
    }
    let port = url.port_or_known_default().ok_or(InvalidTarget)?;
    if !matches!(port, 80 | 443) && !allowed_ports.contains(&port) {
        return Err(InvalidTarget);
    }
    if let Some(Host::Ipv4(ip)) = url.host() {
        let raw_host = authority.split(':').next().ok_or(InvalidTarget)?;
        if raw_host != ip.to_string() {
            return Err(InvalidTarget);
        }
    }
    if matches!(url.host(), Some(Host::Ipv6(ip)) if ip.to_ipv4_mapped().is_some())
        || url.host().is_none()
    {
        return Err(InvalidTarget);
    }
    Ok(url)
}

fn valid_percent_encoding(input: &str) -> bool {
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !(bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                && bytes.next().is_some_and(|b| b.is_ascii_hexdigit()))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn target_normalization_rejects_ambiguous_authorities() {
        assert!(parse("https%3A%2F%2Fexample.com%2Fpath%3Fx%3D1", &[]).is_ok());
        for raw in [
            "http://user@example.com/",
            "http://@example.com/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://2130706433/",
            "http://[::ffff:127.0.0.1]/",
            "http://example.com/#fragment",
            "http://example.com/%0a",
            "http://example.com/%",
            "http://example.com:8080/",
        ] {
            assert!(parse(raw, &[]).is_err(), "{raw}");
        }
        assert!(parse("http://example.com:8080/", &[8080]).is_ok());
    }
}
