use pingora::http::ResponseHeader;

pub fn prepare(
    status: u16,
    text: &str,
    content_type: &str,
    head: bool,
) -> pingora::Result<(ResponseHeader, bool)> {
    let mut response = ResponseHeader::build(status, None)?;
    response.insert_header("content-type", content_type)?;
    if status != 204 && status != 304 {
        response.insert_header("content-length", text.len().to_string())?;
    }
    let bytes = response
        .headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
        .sum::<usize>();
    if bytes > 64 * 1024 {
        return Err(pingora::Error::explain(
            pingora::ErrorType::HTTPStatus(503),
            "policy response header limit",
        ));
    }
    Ok((
        response,
        !head && status != 204 && status != 304 && !text.is_empty(),
    ))
}
