use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::config::ConfigError;
use crate::config::source::Origin;

pub const MAX_FRAME_SIZE: usize = 64 * 1024;
const MAX_DIAGNOSTIC_FIELD_SIZE: usize = 4 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub id: String,
    pub operation: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u8,
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub item: String,
    pub reason: String,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn invalid_frame() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid control frame")
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "control frame timed out")
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

impl Request {
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1 || !valid_id(&self.id) || self.operation != "reload" {
            return Err(invalid_frame());
        }
        Ok(())
    }
}

impl Response {
    pub fn validate(&self, expected_id: &str) -> io::Result<()> {
        if self.version != 1 || !valid_id(&self.id) || self.id != expected_id {
            return Err(invalid_frame());
        }

        match self.status.as_str() {
            "ok" if self.generation.is_some_and(|generation| generation > 0)
                && self.code.is_none()
                && self.errors.is_empty()
                && !self.truncated =>
            {
                Ok(())
            }
            "error"
                if self.generation.is_none()
                    && self.code.as_ref().is_some_and(|code| valid_code(code)) =>
            {
                Ok(())
            }
            _ => Err(invalid_frame()),
        }
    }

    pub fn from_config_error(id: String, error: &ConfigError) -> Self {
        let mut errors = Vec::new();
        let mut primary = config_diagnostic(&error.origin);
        primary.item = error.item.to_string();
        primary.reason = error.reason.to_string();
        errors.push(primary);

        for origin in error.related.iter().chain(&error.origin.references) {
            errors.push(related_diagnostic(origin));
        }

        Self {
            version: 1,
            id,
            status: "error".to_owned(),
            generation: None,
            code: Some(config_code(error.code).to_owned()),
            errors,
            truncated: false,
        }
    }
}

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn config_code(code: &str) -> &'static str {
    match code {
        "CONFIG_SYNTAX" => "CONFIG_SYNTAX",
        "CONFIG_TYPE" => "CONFIG_TYPE",
        "CONFIG_UNKNOWN" => "CONFIG_UNKNOWN",
        "CONFIG_LIMIT" => "CONFIG_LIMIT",
        "CONFIG_UNSUPPORTED" => "CONFIG_UNSUPPORTED",
        "POLICY_MODE_CONFLICT" => "POLICY_MODE_CONFLICT",
        "POLICY_INVALID_CHAIN" => "POLICY_INVALID_CHAIN",
        "POLICY_INVALID_MATCHER" => "POLICY_INVALID_MATCHER",
        "POLICY_INVALID_BRANCH" => "POLICY_INVALID_BRANCH",
        "POLICY_INVALID_RESPONSE" => "POLICY_INVALID_RESPONSE",
        "POLICY_INVALID_REGEX" => "POLICY_INVALID_REGEX",
        "POLICY_INVALID_PATH" => "POLICY_INVALID_PATH",
        "POLICY_LIMIT" => "POLICY_LIMIT",
        "RESOURCE_CHANGED" => "RESOURCE_CHANGED",
        "IMPORT_CYCLE" => "IMPORT_CYCLE",
        "DUPLICATE" => "DUPLICATE",
        "ROUTE_CONFLICT" => "ROUTE_CONFLICT",
        "PROFILE_REF" => "PROFILE_REF",
        "RESOURCE_IO" => "RESOURCE_IO",
        "TLS_INVALID" => "TLS_INVALID",
        "UPSTREAM_DENIED" => "UPSTREAM_DENIED",
        "RESTART_REQUIRED" => "RESTART_REQUIRED",
        "BUSY" => "BUSY",
        "PREPARE_TIMEOUT" => "PREPARE_TIMEOUT",
        "SHUTTING_DOWN" => "SHUTTING_DOWN",
        "UNAUTHORIZED" => "UNAUTHORIZED",
        "PROTOCOL" => "PROTOCOL",
        _ => "INTERNAL",
    }
}

fn config_diagnostic(origin: &Origin) -> Diagnostic {
    let (line, column) = origin.position();
    Diagnostic {
        file: origin.source.path.to_string_lossy().into_owned(),
        line,
        column,
        item: "configuration".to_owned(),
        reason: "configuration validation failed".to_owned(),
    }
}

fn related_diagnostic(origin: &Origin) -> Diagnostic {
    let (line, column) = origin.position();
    Diagnostic {
        file: origin.source.path.to_string_lossy().into_owned(),
        line,
        column,
        item: "configuration".to_owned(),
        reason: "related location".to_owned(),
    }
}

fn trim_field(value: &mut String) -> bool {
    if value.len() <= MAX_DIAGNOSTIC_FIELD_SIZE {
        return false;
    }

    let mut end = MAX_DIAGNOSTIC_FIELD_SIZE;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn serialize_frame<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(value).map_err(|_| invalid_frame())?;
    if frame.len() >= MAX_FRAME_SIZE {
        return Err(invalid_frame());
    }
    frame.push(b'\n');
    Ok(frame)
}

fn frame_body(frame: &[u8]) -> io::Result<&[u8]> {
    if frame.len() > MAX_FRAME_SIZE || frame.last() != Some(&b'\n') {
        return Err(invalid_frame());
    }
    let body = &frame[..frame.len() - 1];
    if body.contains(&b'\n') {
        return Err(invalid_frame());
    }
    Ok(body)
}

pub fn encode_request(request: &Request) -> io::Result<Vec<u8>> {
    request.validate()?;
    serialize_frame(request)
}

pub fn decode_request(frame: &[u8]) -> io::Result<Request> {
    let request: Request =
        serde_json::from_slice(frame_body(frame)?).map_err(|_| invalid_frame())?;
    request.validate()?;
    Ok(request)
}

pub fn encode_response(response: &mut Response) -> io::Result<Vec<u8>> {
    response.validate(&response.id)?;

    for diagnostic in &mut response.errors {
        let clipped = trim_field(&mut diagnostic.file)
            | trim_field(&mut diagnostic.item)
            | trim_field(&mut diagnostic.reason);
        response.truncated |= clipped;
    }

    loop {
        match serialize_frame(response) {
            Ok(frame) => return Ok(frame),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                if response.errors.pop().is_none() {
                    return Err(error);
                }
                response.truncated = true;
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn decode_response(frame: &[u8], expected_id: &str) -> io::Result<Response> {
    let response: Response =
        serde_json::from_slice(frame_body(frame)?).map_err(|_| invalid_frame())?;
    response.validate(expected_id)?;
    Ok(response)
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    timeout(FRAME_TIMEOUT, async {
        let mut frame = Vec::new();
        let mut byte = [0_u8; 1];

        while frame.len() < MAX_FRAME_SIZE {
            if reader.read(&mut byte).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete control frame",
                ));
            }
            frame.push(byte[0]);
            if byte[0] == b'\n' {
                return Ok(frame);
            }
        }

        Err(invalid_frame())
    })
    .await
    .map_err(|_| timed_out())?
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
    frame_body(frame)?;
    timeout(FRAME_TIMEOUT, async {
        writer.write_all(frame).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| timed_out())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_request_rejects_duplicate_and_unknown_fields() {
        let duplicate = b"{\"version\":1,\"id\":\"a\",\"id\":\"b\",\"operation\":\"reload\"}\n";
        let unknown = b"{\"version\":1,\"id\":\"a\",\"operation\":\"reload\",\"extra\":1}\n";
        assert!(decode_request(duplicate).is_err());
        assert!(decode_request(unknown).is_err());
    }

    #[test]
    fn decode_request_rejects_invalid_version_id_and_oversize() {
        assert!(
            decode_request(b"{\"version\":2,\"id\":\"a\",\"operation\":\"reload\"}\n").is_err()
        );
        assert!(
            decode_request(b"{\"version\":1,\"id\":\"bad id\",\"operation\":\"reload\"}\n")
                .is_err()
        );
        let mut oversized = vec![b' '; MAX_FRAME_SIZE];
        oversized.push(b'\n');
        assert!(decode_request(&oversized).is_err());
    }
}
