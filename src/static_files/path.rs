use std::path::PathBuf;

use pingora::http::RequestHeader;

pub fn decode(path: &str) -> Result<PathBuf, u16> {
    if !path.starts_with('/') || !path.is_ascii() {
        return Err(400);
    }
    let source = path.as_bytes();
    let mut decoded = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        let byte = if source[index] == b'%' {
            let high = source
                .get(index + 1)
                .and_then(|byte| char::from(*byte).to_digit(16))
                .ok_or(400_u16)?;
            let low = source
                .get(index + 2)
                .and_then(|byte| char::from(*byte).to_digit(16))
                .ok_or(400_u16)?;
            index += 3;
            let byte = (high * 16 + low) as u8;
            if byte == b'/' {
                return Err(400);
            }
            byte
        } else {
            let byte = source[index];
            index += 1;
            byte
        };
        if byte == 0 || byte == b'\\' || byte.is_ascii_control() {
            return Err(400);
        }
        decoded.push(byte);
    }
    let decoded = String::from_utf8(decoded).map_err(|_| 400_u16)?;
    if decoded.chars().any(char::is_control)
        || decoded.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(400);
    }
    Ok(PathBuf::from(decoded.trim_start_matches('/')))
}

pub fn navigation(request: &RequestHeader, path: &std::path::Path) -> bool {
    if request.headers.contains_key("range")
        || !matches!(request.method.as_str(), "GET" | "HEAD")
        || path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().contains(&b'.'))
    {
        return false;
    }
    request.headers.get_all("accept").iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value.split(',').any(|item| {
                let mut parts = item.split(';');
                if !parts
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/html"))
                {
                    return false;
                }
                let mut quality = None;
                for parameter in parts {
                    let Some((name, value)) = parameter.trim().split_once('=') else {
                        return false;
                    };
                    if name.trim().eq_ignore_ascii_case("q") {
                        if quality.is_some() {
                            return false;
                        }
                        let value = value.trim();
                        if value.is_empty()
                            || !value
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || byte == b'.')
                        {
                            return false;
                        }
                        let Ok(value) = value.parse::<f64>() else {
                            return false;
                        };
                        if !(0.0..=1.0).contains(&value) {
                            return false;
                        }
                        quality = Some(value);
                    }
                }
                quality.is_none_or(|value| value > 0.0)
            })
        })
    })
}
