use crate::support::{TestResult, connect, head};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
};

const BODY_LIMIT: usize = 4 * 1024 * 1024;
pub fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> TestResult<(u16, String, Vec<u8>)> {
    let mut stream = connect(address)?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: app.test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    response(&mut stream, method)
}
pub fn response(stream: &mut TcpStream, method: &str) -> TestResult<(u16, String, Vec<u8>)> {
    let (status, headers) = loop {
        let headers = head(stream)?;
        let status: u16 = headers
            .split_whitespace()
            .nth(1)
            .ok_or("missing status")?
            .parse()?;
        if !(100..=599).contains(&status) {
            return Err("invalid status".into());
        }
        if status >= 200 {
            break (status, headers);
        }
    };
    if method == "HEAD" || matches!(status, 204 | 304) {
        return Ok((status, headers, Vec::new()));
    }
    let mut length = None;
    let mut chunked = false;
    for line in headers
        .split("\r\n")
        .skip(1)
        .filter(|line| !line.is_empty())
    {
        let (name, value) = line.split_once(':').ok_or("invalid response header")?;
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("duplicate length".into());
            }
            length = Some(value.trim().parse::<usize>()?);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.trim().eq_ignore_ascii_case("chunked") {
                return Err("unsupported response framing".into());
            }
            chunked = true;
        }
    }
    if chunked && length.is_some() {
        return Err("ambiguous framing".into());
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let line = line(stream, 8192)?;
            let line = std::str::from_utf8(&line)?;
            let digits = line.split_once(';').map_or(line, |(digits, _)| digits);
            if digits.is_empty()
                || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                || line.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err("invalid chunk length".into());
            }
            let amount = usize::from_str_radix(digits, 16)?;
            if amount == 0 {
                let mut remaining = 64 * 1024;
                loop {
                    let trailer = line_bytes(stream, &mut remaining)?;
                    if trailer.is_empty() {
                        break;
                    }
                    let (name, value) = trailer.split_at(
                        trailer
                            .iter()
                            .position(|byte| *byte == b':')
                            .ok_or("invalid trailer")?,
                    );
                    http::HeaderName::from_bytes(name)?;
                    http::HeaderValue::from_bytes(&value[1..])?;
                }
                break;
            }
            let end = body
                .len()
                .checked_add(amount)
                .filter(|size| *size <= BODY_LIMIT)
                .ok_or("response exceeds test limit")?;
            let start = body.len();
            body.resize(end, 0);
            stream.read_exact(&mut body[start..])?;
            let mut crlf = [0_u8; 2];
            stream.read_exact(&mut crlf)?;
            if crlf != *b"\r\n" {
                return Err("invalid chunk terminator".into());
            }
        }
    } else if let Some(length) = length {
        if length > BODY_LIMIT {
            return Err("response exceeds test limit".into());
        }
        body.resize(length, 0);
        stream.read_exact(&mut body)?;
    } else {
        stream
            .take((BODY_LIMIT + 1) as u64)
            .read_to_end(&mut body)?;
        if body.len() > BODY_LIMIT {
            return Err("response exceeds test limit".into());
        }
    }
    Ok((status, headers, body))
}
fn line(stream: &mut TcpStream, maximum: usize) -> TestResult<Vec<u8>> {
    line_bytes(stream, &mut { maximum })
}
fn line_bytes(stream: &mut TcpStream, remaining: &mut usize) -> TestResult<Vec<u8>> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n") {
        *remaining = remaining.checked_sub(1).ok_or("oversized framing line")?;
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if byte[0] == b'\n' && !bytes.ends_with(b"\r\n") {
            return Err("invalid framing newline".into());
        }
    }
    bytes.truncate(bytes.len() - 2);
    Ok(bytes)
}
