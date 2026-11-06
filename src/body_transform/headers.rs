use super::model;
use crate::http_policy::state::RequestPolicyState;
use http::{HeaderMap, Method, header};
use pingora::http::{RequestHeader, ResponseHeader};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coding {
    Identity,
    Gzip,
    Zstd,
}

impl Selection {
    pub fn active(&self) -> bool {
        !self.replacements.is_empty()
            || self.decode != Coding::Identity
            || self.encode != Coding::Identity
    }
}

pub struct Selection {
    pub replacements: Vec<std::sync::Arc<model::Replace>>,
    pub decode: Coding,
    pub encode: Coding,
    pub vary: bool,
}

pub(crate) fn no_transform(headers: &HeaderMap) -> bool {
    headers.get_all(header::CACHE_CONTROL).iter().any(|value| {
        value
            .as_bytes()
            .split(|&b| b == b',')
            .any(|part| part.trim_ascii().eq_ignore_ascii_case(b"no-transform"))
    })
}
fn streaming(mime: Option<&str>) -> bool {
    mime.and_then(|s| s.split(';').next()).is_some_and(|s| {
        s.trim().eq_ignore_ascii_case("text/event-stream")
            || s.trim()
                .to_ascii_lowercase()
                .starts_with("application/grpc")
    })
}
pub fn request_bypass(request: &RequestHeader) -> bool {
    request.headers.contains_key(header::RANGE)
        || no_transform(&request.headers)
        || request.headers.contains_key(header::UPGRADE)
        || streaming(
            request
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        )
}
pub fn suppress_upstream_validators(
    request: &mut RequestHeader,
    policy: Option<&RequestPolicyState>,
) {
    if policy.is_some_and(RequestPolicyState::has_body_transform)
        && matches!(request.method, Method::GET | Method::HEAD)
        && !request_bypass(request)
    {
        request.remove_header(&header::IF_NONE_MATCH);
        request.remove_header(&header::IF_MODIFIED_SINCE);
    }
}

#[derive(Debug)]
pub struct Unacceptable;

pub fn select(
    request: &RequestHeader,
    response: &ResponseHeader,
    policy: Option<&RequestPolicyState>,
    static_sidecar: bool,
) -> Result<Option<Selection>, Unacceptable> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    let mime = response
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if request_bypass(request)
        || response.status.as_u16() < 200
        || matches!(response.status.as_u16(), 204 | 206 | 304)
        || response.headers.contains_key(header::CONTENT_RANGE)
        || no_transform(&response.headers)
        || streaming(mime)
    {
        return Ok(None);
    }
    let replacements = policy.body_rules(mime);
    let encode = policy.body_encode(mime);
    if replacements.is_empty() && encode.is_none() {
        return Ok(None);
    }
    let encodings = response.headers.get_all(header::CONTENT_ENCODING);
    let mut values = encodings.iter();
    let decode = match values.next().map(|v| v.as_bytes().trim_ascii()) {
        None => Coding::Identity,
        Some(value) if value.eq_ignore_ascii_case(b"identity") => Coding::Identity,
        Some(value) if value.eq_ignore_ascii_case(b"gzip") => Coding::Gzip,
        Some(value) if value.eq_ignore_ascii_case(b"zstd") => Coding::Zstd,
        _ => return Ok(None),
    };
    if values.next().is_some() {
        return Ok(None);
    }
    if static_sidecar && replacements.is_empty() && decode != Coding::Identity {
        return Ok(None);
    }
    let coding = negotiate(request, encode.as_deref()).ok_or(Unacceptable)?;
    let vary = true;
    Ok(Some(Selection {
        replacements,
        decode,
        encode: coding,
        vary,
    }))
}

pub fn negotiate(request: &RequestHeader, encode: Option<&model::Encode>) -> Option<Coding> {
    let choices = [Coding::Zstd, Coding::Gzip, Coding::Identity];
    let mut best = None;
    let mut highest = 0;
    for coding in choices {
        if (coding == Coding::Zstd && encode.is_none_or(|e| !e.zstd))
            || (coding == Coding::Gzip && encode.is_none_or(|e| !e.gzip))
        {
            continue;
        }
        let name = match coding {
            Coding::Zstd => "zstd",
            Coding::Gzip => "gzip",
            Coding::Identity => "identity",
        };
        let q = crate::static_files::representation::coding_quality(request, name);
        if q > highest {
            highest = q;
            best = Some(coding);
        }
    }
    best
}

pub fn vary(response: &mut ResponseHeader) -> pingora::Result<()> {
    if !response.headers.get_all(header::VARY).iter().any(|v| {
        v.as_bytes().split(|&b| b == b',').any(|s| {
            s.trim_ascii() == b"*" || s.trim_ascii().eq_ignore_ascii_case(b"Accept-Encoding")
        })
    }) {
        response.append_header(header::VARY, "Accept-Encoding")?;
    }
    Ok(())
}

pub fn finalize(
    response: &mut ResponseHeader,
    selection: &Selection,
    head: bool,
) -> pingora::Result<()> {
    for name in [
        "etag",
        "last-modified",
        "content-length",
        "content-md5",
        "content-encoding",
        "transfer-encoding",
        "accept-ranges",
    ] {
        response.remove_header(name);
    }
    if selection.vary {
        vary(response)?;
    }
    match selection.encode {
        Coding::Identity => (),
        Coding::Gzip => response.insert_header(header::CONTENT_ENCODING, "gzip")?,
        Coding::Zstd => response.insert_header(header::CONTENT_ENCODING, "zstd")?,
    }
    if !head {
        response.set_version(http::Version::HTTP_11);
        response.insert_header(header::TRANSFER_ENCODING, "chunked")?;
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct StaticOptions {
    request: RequestHeader,
    replacements: Vec<std::sync::Arc<model::Replace>>,
    encode: Option<std::sync::Arc<model::Encode>>,
}
impl StaticOptions {
    pub fn new(
        request: &RequestHeader,
        policy: Option<&RequestPolicyState>,
        cache_control: Option<&str>,
    ) -> Option<Self> {
        if request_bypass(request)
            || cache_control.is_some_and(|s| {
                s.split(',')
                    .any(|s| s.trim().eq_ignore_ascii_case("no-transform"))
            })
        {
            return None;
        }
        let (replacements, encode) = policy?.body_declarations();
        Some(Self {
            request: request.clone(),
            replacements,
            encode,
        })
    }
    pub fn replace(&self, mime: &str) -> bool {
        self.replacements
            .iter()
            .any(|r| model::matches(&r.mimes, Some(mime)))
    }
    pub fn identity(&self, mime: &str) -> bool {
        self.replace(mime)
            || self.encode.as_ref().is_some_and(|e| {
                model::matches(&e.mimes, Some(mime)) && negotiate(&self.request, Some(e)).is_some()
            })
    }
}
