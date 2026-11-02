use std::fs::Metadata;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pingora::http::{RequestHeader, ResponseHeader};

use super::range::{self, RangeDecision};

const HTTP_DATE_LIMIT: u64 = 253_402_300_800;

pub struct Plan {
    pub header: ResponseHeader,
    pub offset: u64,
    pub length: u64,
    pub send_body: bool,
}

fn date_seconds(time: SystemTime) -> Option<u64> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    (seconds < HTTP_DATE_LIMIT).then_some(seconds)
}

fn http_date(seconds: u64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn single_header<'a>(request: &'a RequestHeader, name: &str) -> Option<&'a str> {
    let mut values = request.headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

fn parsed_date(value: &str) -> Option<u64> {
    date_seconds(httpdate::parse_http_date(value).ok()?)
}

fn mime_type(path: &Path) -> &'static str {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return "application/octet-stream";
    };

    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn plan(
    request: &RequestHeader,
    metadata: &Metadata,
    served_path: &Path,
    cache_control: Option<&str>,
) -> pingora::Result<Plan> {
    let now = SystemTime::now();
    let date = match now.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().min(HTTP_DATE_LIMIT - 1),
        Err(_) => 0,
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(date_seconds)
        .map(|seconds| seconds.min(date));
    let file_length = metadata.len();
    let is_head = request.method.as_str() == "HEAD";

    let not_modified = !request.headers.contains_key("if-none-match")
        && modified.is_some_and(|mtime| {
            single_header(request, "if-modified-since")
                .and_then(parsed_date)
                .is_some_and(|condition| mtime <= condition)
        });

    let range_allowed = if request.headers.contains_key("if-range") {
        modified.is_some_and(|mtime| {
            single_header(request, "if-range")
                .and_then(parsed_date)
                .is_some_and(|condition| {
                    condition == mtime && date.checked_sub(mtime).is_some_and(|age| age >= 60)
                })
        })
    } else {
        true
    };

    let decision = if !is_head && !not_modified && range_allowed {
        single_header(request, "range")
            .map(|value| range::select(value, file_length))
            .unwrap_or(RangeDecision::Full)
    } else {
        RangeDecision::Full
    };

    let (status, offset, length, content_range) = if not_modified {
        (304, 0, 0, None)
    } else {
        match decision {
            RangeDecision::Full => (200, 0, file_length, None),
            RangeDecision::Partial { start, length } => {
                let end = start + length - 1;
                (
                    206,
                    start,
                    length,
                    Some(format!("bytes {start}-{end}/{file_length}")),
                )
            }
            RangeDecision::Unsatisfiable => (416, 0, 0, Some(format!("bytes */{file_length}"))),
        }
    };

    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("Date", http_date(date))?;
    header.insert_header("Accept-Ranges", "bytes")?;
    header.insert_header("Content-Type", mime_type(served_path))?;
    if let Some(mtime) = modified {
        header.insert_header("Last-Modified", http_date(mtime))?;
    }
    if let Some(value) = cache_control {
        header.insert_header("Cache-Control", value)?;
    }
    if let Some(value) = content_range {
        header.insert_header("Content-Range", value)?;
    }
    if status != 304 {
        header.insert_header("Content-Length", length.to_string())?;
    }

    Ok(Plan {
        header,
        offset,
        length,
        send_body: !is_head && status != 304 && status != 416 && length != 0,
    })
}
