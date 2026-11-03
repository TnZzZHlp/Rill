use pingora::http::{RequestHeader, ResponseHeader};
use pingora::{Error, ErrorType, Result};

use super::request::Forwarding;

pub(super) const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub fn request(
    request: &mut RequestHeader,
    names: &[String],
    forwarding: &Forwarding,
) -> Result<()> {
    let untrusted: Vec<_> = request
        .headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-forwarded-"))
        .map(ToString::to_string)
        .collect();
    for name in HOP_HEADERS
        .iter()
        .copied()
        .chain(names.iter().map(String::as_str))
        .chain(untrusted.iter().map(String::as_str))
        .chain(std::iter::once("forwarded"))
    {
        request.remove_header(name);
    }
    request.insert_header("host", forwarding.host.as_str())?;
    request.insert_header("forwarded", forwarding.forwarded.as_str())?;
    request.insert_header("x-forwarded-for", forwarding.for_header.as_str())?;
    request.insert_header("x-forwarded-host", forwarding.host.as_str())?;
    request.insert_header("x-forwarded-proto", forwarding.proto)?;
    request.insert_header("x-forwarded-port", forwarding.port.to_string())?;
    Ok(())
}

pub fn response(response: &mut ResponseHeader) -> Result<()> {
    if response.status.as_u16() == 101 {
        return Err(
            Error::explain(ErrorType::InvalidHTTPHeader, "unexpected upstream upgrade").into_up(),
        );
    }
    let mut names = Vec::new();
    for value in response.headers.get_all("connection").iter() {
        let value = value.to_str().map_err(|_| {
            Error::explain(ErrorType::InvalidHTTPHeader, "invalid upstream Connection").into_up()
        })?;
        for name in value.split(',') {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
                || matches!(name.as_str(), "content-length" | "transfer-encoding")
            {
                return Err(Error::explain(
                    ErrorType::InvalidHTTPHeader,
                    "ambiguous upstream Connection",
                )
                .into_up());
            }
            names.push(name);
        }
    }
    let chunked = response
        .headers
        .get("transfer-encoding")
        .map(|value| value.as_bytes().eq_ignore_ascii_case(b"chunked"));
    if chunked == Some(false) {
        return Err(Error::explain(
            ErrorType::InvalidHTTPHeader,
            "unsupported upstream transfer coding",
        )
        .into_up());
    }
    for name in HOP_HEADERS
        .iter()
        .copied()
        .chain(names.iter().map(String::as_str))
    {
        response.remove_header(name);
    }
    if chunked == Some(true) {
        response.insert_header("transfer-encoding", "chunked")?;
    }
    Ok(())
}
