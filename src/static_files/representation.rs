#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fraction_value = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u16>().ok()? * 10_u16.pow(3 - fraction.len() as u32)
    };
    match whole {
        "0" => Some(fraction_value),
        "1" if fraction_value == 0 => Some(1000),
        _ => None,
    }
}

fn item(item: &str) -> (&str, u16) {
    let mut parts = item.split(';');
    let name = parts.next().unwrap_or("").trim_matches([' ', '\t']);
    let mut q = None;
    let mut valid = !name.is_empty();
    for parameter in parts {
        let Some((key, value)) = parameter.trim_matches([' ', '\t']).split_once('=') else {
            valid = false;
            continue;
        };
        if !key.trim_matches([' ', '\t']).eq_ignore_ascii_case("q") || q.is_some() {
            valid = false;
            continue;
        }
        q = Some(quality(value.trim_matches([' ', '\t'])));
    }
    (
        name,
        if valid {
            q.unwrap_or(Some(1000)).unwrap_or(0)
        } else {
            0
        },
    )
}

pub fn ranked_request(request: &pingora::http::RequestHeader, enabled: bool) -> Vec<Encoding> {
    ranked_raw(
        request
            .headers
            .get_all("accept-encoding")
            .iter()
            .map(|value| value.as_bytes()),
        enabled,
    )
}

#[cfg(test)]
pub fn ranked<'a>(headers: impl IntoIterator<Item = &'a str>, enabled: bool) -> Vec<Encoding> {
    ranked_raw(headers.into_iter().map(str::as_bytes), enabled)
}

fn ranked_raw<'a>(headers: impl IntoIterator<Item = &'a [u8]>, enabled: bool) -> Vec<Encoding> {
    let mut br = None::<u16>;
    let mut gzip = None::<u16>;
    let mut identity = None::<u16>;
    let mut wildcard = None::<u16>;
    let mut nonempty = false;
    for header in headers {
        if !header.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
            nonempty = true;
        }
        for part in header.split(|byte| *byte == b',') {
            let name_bytes = part.split(|byte| *byte == b';').next().unwrap_or_default();
            let Ok(name) = std::str::from_utf8(name_bytes) else {
                continue;
            };
            let name = name.trim_matches([' ', '\t']);
            let q = std::str::from_utf8(part).map_or(0, |text| item(text).1);
            let slot = if name.eq_ignore_ascii_case("br") {
                &mut br
            } else if name.eq_ignore_ascii_case("gzip") {
                &mut gzip
            } else if name.eq_ignore_ascii_case("identity") {
                &mut identity
            } else if name == "*" {
                &mut wildcard
            } else {
                continue;
            };
            *slot = Some(slot.map_or(q, |previous| previous.min(q)));
        }
    }
    if !nonempty {
        return vec![Encoding::Identity];
    }
    let identity = identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 });
    let mut candidates = Vec::new();
    if enabled {
        candidates.push((br.or(wildcard).unwrap_or(0), Encoding::Brotli));
        candidates.push((gzip.or(wildcard).unwrap_or(0), Encoding::Gzip));
    }
    candidates.push((identity, Encoding::Identity));
    candidates.retain(|(q, _)| *q > 0);
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates
        .into_iter()
        .map(|(_, encoding)| encoding)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        Encoding::{Brotli, Gzip, Identity},
        ranked,
    };

    #[test]
    fn non_utf8_header_items_do_not_hide_identity_rejection() {
        let mut request = pingora::http::RequestHeader::build("GET", b"/", None).unwrap();
        request
            .append_header("accept-encoding", &b"identity;q=0,\xff"[..])
            .unwrap();
        assert!(super::ranked_request(&request, false).is_empty());
        let mut request = pingora::http::RequestHeader::build("GET", b"/", None).unwrap();
        request
            .append_header("accept-encoding", &b"\xff"[..])
            .unwrap();
        request
            .append_header("accept-encoding", "identity;q=0")
            .unwrap();
        assert!(super::ranked_request(&request, false).is_empty());
        let mut request = pingora::http::RequestHeader::build("GET", b"/", None).unwrap();
        request
            .append_header("accept-encoding", &b"identity;q=0\xff"[..])
            .unwrap();
        assert!(super::ranked_request(&request, false).is_empty());
        let mut request = pingora::http::RequestHeader::build("GET", b"/", None).unwrap();
        request
            .append_header("accept-encoding", &b"br;q=\xff, *;q=1"[..])
            .unwrap();
        request
            .append_header("accept-encoding", "identity;q=0")
            .unwrap();
        assert_eq!(super::ranked_request(&request, true), [Gzip]);
    }

    #[test]
    fn unicode_whitespace_is_not_http_ows() -> pingora::Result<()> {
        for value in [
            &b"br;q=\xc2\xa01, identity;q=0"[..],
            &b"br;\xc2\xa0q=1, identity;q=0"[..],
            &b"\xc2\xa0br;q=1, identity;q=0"[..],
        ] {
            let mut request = pingora::http::RequestHeader::build("GET", b"/", None)?;
            request.append_header("accept-encoding", value)?;
            assert!(super::ranked_request(&request, true).is_empty());
        }
        Ok(())
    }

    #[test]
    fn ranking_and_rejections() {
        assert_eq!(ranked(["gzip, br"], true), [Brotli, Gzip, Identity]);
        assert_eq!(
            ranked(["gzip;q=0.2", "gzip;q=0.1, br;q=0.9, identity;q=0"], true),
            [Brotli, Gzip]
        );
        assert_eq!(ranked(["*;q=0"], true), []);
        assert_eq!(ranked(["br;q=1.0000, identity;q=0"], true), []);
        assert_eq!(ranked([""], true), [Identity]);
        assert_eq!(ranked(["gzip, identity;q=0"], false), []);
    }
}
