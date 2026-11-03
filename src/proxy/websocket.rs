use std::fmt;

use openssl::{base64, sha};
use pingora::http::{Method, RequestHeader, ResponseHeader, Version};
use pingora::{Error, ErrorType, Result};

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

use super::headers::HOP_HEADERS;

pub struct Handshake {
    key: String,
    expected_accept: String,
    protocols: Vec<String>,
}

impl fmt::Debug for Handshake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Handshake").finish_non_exhaustive()
    }
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn single_header<'a>(
    headers: &'a pingora::http::HMap,
    name: &str,
) -> std::result::Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

fn request_protocols(header: &RequestHeader) -> std::result::Result<Vec<String>, u16> {
    let mut protocols = Vec::new();
    for value in header.headers.get_all("sec-websocket-protocol").iter() {
        let value = value.to_str().map_err(|_| 400_u16)?;
        for protocol in value.split(',') {
            let protocol = protocol.trim_matches([' ', '\t']);
            if !token(protocol) || protocols.iter().any(|existing| existing == protocol) {
                return Err(400);
            }
            protocols.push(protocol.to_string());
        }
    }
    Ok(protocols)
}

pub fn request(
    header: &RequestHeader,
    http2: bool,
    connection_names: &[String],
    allowed: bool,
) -> std::result::Result<Option<Handshake>, u16> {
    let has_upgrade = header.headers.contains_key("upgrade")
        || connection_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case("upgrade"));
    let connect = header.method == Method::CONNECT;

    if !has_upgrade && !connect {
        return Ok(None);
    }
    if connect || http2 {
        return Err(501);
    }
    if header.version != Version::HTTP_11 || header.method != Method::GET {
        return Err(400);
    }

    if connection_names.iter().any(|name| {
        [
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
        ]
        .iter()
        .any(|protected| name.eq_ignore_ascii_case(protected))
    }) {
        return Err(400);
    }

    let upgrade = single_header(&header.headers, "upgrade").map_err(|()| 400_u16)?;
    let Some(upgrade) = upgrade else {
        return Err(400);
    };
    let upgrade = upgrade.trim_matches([' ', '\t']);
    if !token(upgrade) {
        return Err(400);
    }
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(501);
    }
    if !allowed {
        return Err(403);
    }
    if !connection_names
        .iter()
        .any(|name| name.eq_ignore_ascii_case("upgrade"))
    {
        return Err(400);
    }

    let version = single_header(&header.headers, "sec-websocket-version")
        .map_err(|()| 400_u16)?
        .ok_or(400_u16)?
        .trim_matches([' ', '\t']);
    if version != "13" {
        return Err(400);
    }

    if header.headers.contains_key("transfer-encoding") {
        return Err(400);
    }
    if let Some(length) = single_header(&header.headers, "content-length").map_err(|()| 400_u16)? {
        let length = length.trim_matches([' ', '\t']);
        if length.is_empty() || !length.bytes().all(|byte| byte == b'0') {
            return Err(400);
        }
    }

    let key = single_header(&header.headers, "sec-websocket-key")
        .map_err(|()| 400_u16)?
        .ok_or(400_u16)?
        .trim_matches([' ', '\t']);
    if key.len() != 24 {
        return Err(400);
    }
    let decoded = base64::decode_block(key).map_err(|_| 400_u16)?;
    if decoded.len() != 16 || base64::encode_block(&decoded) != key {
        return Err(400);
    }

    let protocols = request_protocols(header)?;
    let mut accept_input = String::with_capacity(key.len() + WEBSOCKET_GUID.len());
    accept_input.push_str(key);
    accept_input.push_str(WEBSOCKET_GUID);

    Ok(Some(Handshake {
        key: key.to_string(),
        expected_accept: base64::encode_block(&sha::sha1(accept_input.as_bytes())),
        protocols,
    }))
}

pub fn request_headers(handshake: &Handshake, header: &mut RequestHeader) -> Result<()> {
    header.remove_header("sec-websocket-extensions");
    header.remove_header("sec-websocket-protocol");

    header.insert_header("connection", "Upgrade")?;
    header.insert_header("upgrade", "websocket")?;
    header.insert_header("sec-websocket-key", handshake.key.as_str())?;
    header.insert_header("sec-websocket-version", "13")?;
    if !handshake.protocols.is_empty() {
        header.insert_header("sec-websocket-protocol", handshake.protocols.join(", "))?;
    }
    Ok(())
}

fn invalid_response(message: &'static str) -> Box<Error> {
    Error::explain(ErrorType::InvalidHTTPHeader, message).into_up()
}

pub fn response(handshake: &Handshake, header: &mut ResponseHeader) -> Result<()> {
    if header.version != Version::HTTP_11 || header.status.as_u16() != 101 {
        return Err(invalid_response("invalid WebSocket upgrade status"));
    }

    let upgrade = single_header(&header.headers, "upgrade")
        .map_err(|()| invalid_response("invalid upstream Upgrade"))?
        .ok_or_else(|| invalid_response("missing upstream Upgrade"))?
        .trim_matches([' ', '\t']);
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(invalid_response("invalid upstream Upgrade"));
    }

    let accept = single_header(&header.headers, "sec-websocket-accept")
        .map_err(|()| invalid_response("invalid upstream WebSocket Accept"))?
        .ok_or_else(|| invalid_response("missing upstream WebSocket Accept"))?
        .trim_matches([' ', '\t']);
    if accept != handshake.expected_accept {
        return Err(invalid_response("invalid upstream WebSocket Accept"));
    }

    let protocol = single_header(&header.headers, "sec-websocket-protocol")
        .map_err(|()| invalid_response("invalid upstream WebSocket protocol"))?
        .map(|value| value.trim_matches([' ', '\t']).to_string());
    if let Some(protocol) = &protocol
        && (!token(protocol)
            || !handshake
                .protocols
                .iter()
                .any(|offered| offered == protocol))
    {
        return Err(invalid_response("unoffered upstream WebSocket protocol"));
    }

    if header.headers.contains_key("sec-websocket-extensions") {
        return Err(invalid_response("unoffered upstream WebSocket extension"));
    }
    if header.headers.contains_key("content-length")
        || header.headers.contains_key("transfer-encoding")
    {
        return Err(invalid_response("invalid upstream WebSocket framing"));
    }

    let mut connection_names = Vec::new();
    let mut has_upgrade = false;
    for value in header.headers.get_all("connection").iter() {
        let value = value
            .to_str()
            .map_err(|_| invalid_response("invalid upstream Connection"))?;
        for name in value.split(',') {
            let name = name.trim_matches([' ', '\t']);
            if !token(name) {
                return Err(invalid_response("invalid upstream Connection"));
            }
            let name = name.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "sec-websocket-accept"
                    | "sec-websocket-protocol"
                    | "sec-websocket-extensions"
                    | "content-length"
                    | "transfer-encoding"
            ) {
                return Err(invalid_response("ambiguous upstream Connection"));
            }
            has_upgrade |= name == "upgrade";
            connection_names.push(name);
        }
    }
    if !has_upgrade {
        return Err(invalid_response("missing upstream Connection Upgrade"));
    }

    for name in HOP_HEADERS
        .iter()
        .copied()
        .chain(connection_names.iter().map(String::as_str))
    {
        header.remove_header(name);
    }
    header.insert_header("connection", "Upgrade")?;
    header.insert_header("upgrade", "websocket")?;
    header.insert_header("sec-websocket-accept", handshake.expected_accept.as_str())?;
    if let Some(protocol) = protocol {
        header.insert_header("sec-websocket-protocol", protocol)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_handshake_and_accept_validation() {
        let mut request = RequestHeader::build("GET", b"/", None).unwrap();
        request.insert_header("connection", "Upgrade").unwrap();
        request.insert_header("upgrade", "websocket").unwrap();
        request
            .insert_header("sec-websocket-version", "13")
            .unwrap();
        request
            .insert_header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .unwrap();
        let names = vec!["upgrade".to_string()];
        assert!(matches!(
            super::request(&request, false, &names, false),
            Err(403)
        ));
        let handshake = super::request(&request, false, &names, true)
            .unwrap()
            .unwrap();
        let mut response = ResponseHeader::build(101, None).unwrap();
        response.insert_header("connection", "Upgrade").unwrap();
        response.insert_header("upgrade", "websocket").unwrap();
        response
            .insert_header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
            .unwrap();
        super::response(&handshake, &mut response).unwrap();
        response
            .insert_header("sec-websocket-accept", "wrong")
            .unwrap();
        assert!(super::response(&handshake, &mut response).is_err());
        response
            .insert_header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
            .unwrap();
        response
            .append_header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
            .unwrap();
        assert!(super::response(&handshake, &mut response).is_err());
    }
}
