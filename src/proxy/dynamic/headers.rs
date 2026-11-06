use super::bridge::Ready;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use pingora::{
    Error, ErrorType,
    http::{RequestHeader, ResponseHeader},
};

pub fn request(
    request: &mut RequestHeader,
    ready: &Ready,
    auth_headers: &[http::HeaderName],
) -> pingora::Result<()> {
    let names: Vec<_> = request
        .headers
        .keys()
        .filter(|n| {
            n.as_str().starts_with("x-forwarded-")
                || n.as_str().starts_with("x-real-ip")
                || n.as_str().starts_with("x-auth-")
        })
        .cloned()
        .collect();
    for name in names.iter().chain(auth_headers) {
        request.remove_header(name);
    }
    for name in [
        "authorization",
        "proxy-authorization",
        "cookie",
        "forwarded",
        "traceparent",
        "tracestate",
        "baggage",
    ] {
        request.remove_header(name);
    }
    let authority = &ready.url[url::Position::BeforeHost..url::Position::AfterPort];
    request.insert_header("host", authority)?;
    let mut path = ready.url.path().to_owned();
    if let Some(query) = ready.url.query() {
        path.push('?');
        path.push_str(query);
    }
    request.set_raw_path(path.as_bytes())?;
    crate::http_policy::headers::check(&request.headers)
        .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_HEADER_LIMIT"))?;
    Ok(())
}

pub fn response(response: &mut ResponseHeader, ready: &Ready) -> pingora::Result<()> {
    response.remove_header("set-cookie");
    if let Some(value) = response.headers.get(http::header::LOCATION)
        && let Ok(raw) = value.to_str()
        && let Ok(url) = ready.url.join(raw)
        && matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.origin() == ready.url.origin()
    {
        let fragment = url
            .fragment()
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default();
        let target = &url[..url::Position::AfterQuery];
        let location = format!(
            "{}{}{}",
            ready.prefix,
            utf8_percent_encode(target, NON_ALPHANUMERIC),
            fragment,
        );
        response.insert_header("location", location)?;
    }
    crate::http_policy::headers::check(&response.headers)
        .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "DYNAMIC_HEADER_LIMIT"))?;
    Ok(())
}
