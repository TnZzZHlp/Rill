use super::template::{Context, Template};
use http::{HeaderMap, HeaderName, HeaderValue};

const LIMIT: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Request,
    Response,
}
#[derive(Clone, Debug)]
pub struct HeaderPolicy {
    pub side: Side,
    pub content_type: Option<String>,
    pub status: Option<(u16, u16)>,
    pub real_client_ip: bool,
    pub operations: Vec<Operation>,
}
#[derive(Clone, Debug)]
pub enum Operation {
    Set(HeaderName, Template),
    Add(HeaderName, Template),
    Delete(HeaderName),
    Replace(HeaderName, String, Template),
    Vary(String),
}
#[derive(Debug)]
pub struct HeaderError;

pub fn protected(name: &HeaderName, extra: &[HeaderName]) -> bool {
    let name_str = name.as_str();
    matches!(
        name_str,
        "content-length"
            | "transfer-encoding"
            | "content-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "proxy-connection"
            | "proxy-authenticate"
            | "www-authenticate"
            | "host"
            | "forwarded"
            | "authorization"
            | "proxy-authorization"
            | "x-real-ip"
    ) || name_str.starts_with("x-forwarded-")
        || extra.contains(name)
}
fn size(map: &HeaderMap) -> Option<usize> {
    map.iter().try_fold(0usize, |n, (k, v)| {
        n.checked_add(k.as_str().len())?
            .checked_add(v.len())?
            .checked_add(4)
    })
}
pub fn check(map: &HeaderMap) -> Result<(), HeaderError> {
    if size(map).is_none_or(|n| n > LIMIT) {
        Err(HeaderError)
    } else {
        Ok(())
    }
}
fn value(s: &str) -> Result<HeaderValue, HeaderError> {
    HeaderValue::from_str(s).map_err(|_| HeaderError)
}
fn name_of(op: &Operation) -> Option<&HeaderName> {
    match op {
        Operation::Set(n, _)
        | Operation::Add(n, _)
        | Operation::Delete(n)
        | Operation::Replace(n, _, _) => Some(n),
        Operation::Vary(_) => None,
    }
}
impl HeaderPolicy {
    pub fn allocated_bytes(&self) -> usize {
        self.content_type
            .as_ref()
            .map_or(0, String::capacity)
            .saturating_add(
                self.operations
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Operation>()),
            )
            .saturating_add(
                self.operations
                    .iter()
                    .map(|op| match op {
                        Operation::Set(n, t) | Operation::Add(n, t) => {
                            n.as_str().len().saturating_add(t.allocated_bytes())
                        }
                        Operation::Delete(n) => n.as_str().len(),
                        Operation::Replace(n, s, t) => n
                            .as_str()
                            .len()
                            .saturating_add(s.capacity())
                            .saturating_add(t.allocated_bytes()),
                        Operation::Vary(s) => s.capacity(),
                    })
                    .sum::<usize>(),
            )
    }
    pub fn applies(&self, status: u16, content_type: Option<&str>) -> bool {
        self.status
            .is_none_or(|(start, end)| (start..=end).contains(&status))
            && self.content_type.as_ref().is_none_or(|expected| {
                content_type.is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case(expected))
                })
            })
    }
    pub fn apply(
        &self,
        headers: &mut HeaderMap,
        ctx: &Context<'_>,
        connection_names: &[String],
        extra_protected: &[HeaderName],
    ) -> Result<(), HeaderError> {
        check(headers)?;
        for op in &self.operations {
            let name = name_of(op).map_or("vary", HeaderName::as_str);
            if connection_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
                || name_of(op).is_some_and(|n| protected(n, extra_protected))
                || (name == "vary" && extra_protected.contains(&http::header::VARY))
            {
                return Err(HeaderError);
            }
        }
        if self.real_client_ip
            && (connection_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case("x-real-ip"))
                || extra_protected.contains(&HeaderName::from_static("x-real-ip")))
        {
            return Err(HeaderError);
        }
        let mut result = headers.clone();
        if self.real_client_ip {
            let ip = ctx.client_ip.to_string();
            let removed = result
                .get_all("x-real-ip")
                .iter()
                .try_fold(0usize, |n, v| n.checked_add(13)?.checked_add(v.len()))
                .ok_or(HeaderError)?;
            if size(&result)
                .and_then(|n| n.checked_sub(removed))
                .and_then(|n| n.checked_add(13 + ip.len()))
                .is_none_or(|n| n > LIMIT)
            {
                return Err(HeaderError);
            }
            result.insert("x-real-ip", value(&ip)?);
        }
        for op in &self.operations {
            match op {
                Operation::Set(n, t) | Operation::Add(n, t) => {
                    let v = t.evaluate(ctx, LIMIT).map_err(|_| HeaderError)?;
                    let existing = size(&result).ok_or(HeaderError)?;
                    let add = n
                        .as_str()
                        .len()
                        .checked_add(v.len())
                        .and_then(|x| x.checked_add(4))
                        .ok_or(HeaderError)?;
                    let removed = if matches!(op, Operation::Set(..)) {
                        result
                            .get_all(n)
                            .iter()
                            .try_fold(0usize, |acc, v| {
                                acc.checked_add(n.as_str().len())?
                                    .checked_add(v.len())?
                                    .checked_add(4)
                            })
                            .ok_or(HeaderError)?
                    } else {
                        0
                    };
                    if existing
                        .checked_sub(removed)
                        .and_then(|x| x.checked_add(add))
                        .is_none_or(|x| x > LIMIT)
                    {
                        return Err(HeaderError);
                    }
                    if matches!(op, Operation::Set(..)) {
                        result.insert(n, value(&v)?);
                    } else {
                        result.append(n, value(&v)?);
                    }
                }
                Operation::Delete(n) => {
                    result.remove(n);
                }
                Operation::Replace(n, old, template) => {
                    let replacement = template.evaluate(ctx, LIMIT).map_err(|_| HeaderError)?;
                    let mut values = Vec::new();
                    let mut remaining = LIMIT
                        .checked_sub(size(&result).ok_or(HeaderError)?)
                        .ok_or(HeaderError)?;
                    for old_value in result.get_all(n).iter() {
                        remaining = remaining.checked_add(old_value.len()).ok_or(HeaderError)?;
                        let bytes = old_value.as_bytes();
                        let pattern = old.as_bytes();
                        let mut output = Vec::new();
                        let mut cursor = 0;
                        loop {
                            let found = if pattern.is_empty() {
                                Some(cursor)
                            } else {
                                bytes[cursor..]
                                    .windows(pattern.len())
                                    .position(|w| w == pattern)
                                    .map(|p| cursor + p)
                            };
                            let Some(pos) = found else {
                                bounded_extend(&mut output, &bytes[cursor..], remaining)?;
                                break;
                            };
                            bounded_extend(&mut output, &bytes[cursor..pos], remaining)?;
                            bounded_extend(&mut output, replacement.as_bytes(), remaining)?;
                            cursor = pos + pattern.len();
                            if pattern.is_empty() {
                                if cursor == bytes.len() {
                                    break;
                                }
                                bounded_extend(&mut output, &bytes[cursor..cursor + 1], remaining)?;
                                cursor += 1;
                            }
                        }
                        remaining = remaining.checked_sub(output.len()).ok_or(HeaderError)?;
                        values.push(HeaderValue::from_bytes(&output).map_err(|_| HeaderError)?);
                    }
                    result.remove(n);
                    for v in values {
                        result.append(n, v);
                    }
                }
                Operation::Vary(token) => {
                    if token.len() > LIMIT {
                        return Err(HeaderError);
                    }
                    let mut tokens: Vec<String> = Vec::new();
                    for v in result.get_all(http::header::VARY).iter() {
                        for part in v.to_str().map_err(|_| HeaderError)?.split(',') {
                            let part = part.trim();
                            if !part.is_empty()
                                && !tokens.iter().any(|t| t.eq_ignore_ascii_case(part))
                            {
                                tokens.push(part.to_owned());
                            }
                        }
                    }
                    if tokens.iter().any(|v| v == "*") || token == "*" {
                        tokens.clear();
                        tokens.push("*".to_owned());
                    } else if !tokens.iter().any(|v| v.eq_ignore_ascii_case(token)) {
                        tokens.push(token.clone());
                    }
                    let joined_len = tokens
                        .iter()
                        .try_fold(0usize, |n, t| n.checked_add(t.len()))
                        .and_then(|n| n.checked_add(tokens.len().saturating_sub(1).checked_mul(2)?))
                        .ok_or(HeaderError)?;
                    let old = result
                        .get_all(http::header::VARY)
                        .iter()
                        .try_fold(0usize, |n, v| n.checked_add(8)?.checked_add(v.len()))
                        .ok_or(HeaderError)?;
                    let next = size(&result)
                        .and_then(|n| n.checked_sub(old))
                        .and_then(|n| n.checked_add(8))
                        .and_then(|n| n.checked_add(joined_len))
                        .ok_or(HeaderError)?;
                    if next > LIMIT {
                        return Err(HeaderError);
                    }
                    let joined = tokens.join(", ");
                    result.insert(http::header::VARY, value(&joined)?);
                }
            }
            check(&result)?;
        }
        *headers = result;
        Ok(())
    }
}
fn bounded_extend(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), HeaderError> {
    if output
        .len()
        .checked_add(bytes.len())
        .is_none_or(|n| n > limit)
    {
        return Err(HeaderError);
    }
    output.extend_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_and_replacement_fail_atomically_at_limits() {
        let ctx = Context {
            client_ip: "127.0.0.1".parse().unwrap(),
            upstream_authority: None,
            path: "bad\r\nvalue",
            site: "test",
            captures: [None; 9],
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-test", HeaderValue::from_static("abc"));
        let original = headers.clone();
        let mut policy = HeaderPolicy {
            side: Side::Response,
            content_type: None,
            status: None,
            real_client_ip: false,
            operations: vec![Operation::Set(
                HeaderName::from_static("x-test"),
                Template::compile("{path}").unwrap(),
            )],
        };
        assert!(policy.apply(&mut headers, &ctx, &[], &[]).is_err());
        assert_eq!(headers, original);
        policy.operations = vec![Operation::Replace(
            HeaderName::from_static("x-test"),
            String::new(),
            Template::compile(&"a".repeat(LIMIT)).unwrap(),
        )];
        assert!(policy.apply(&mut headers, &ctx, &[], &[]).is_err());
        assert_eq!(headers, original);
        policy.operations = vec![Operation::Delete(HeaderName::from_static("x-test"))];
        assert!(
            policy
                .apply(&mut headers, &ctx, &["X-Test".into()], &[])
                .is_err()
        );
        assert_eq!(headers, original);
    }

    #[test]
    fn vary_deduplicates_and_wildcard_wins() {
        let ctx = Context {
            client_ip: "127.0.0.1".parse().unwrap(),
            upstream_authority: None,
            path: "/",
            site: "test",
            captures: [None; 9],
        };
        let mut headers = HeaderMap::new();
        headers.append("vary", HeaderValue::from_static("Accept, accept-encoding"));
        headers.append("vary", HeaderValue::from_static("ACCEPT"));
        let mut policy = HeaderPolicy {
            side: Side::Response,
            content_type: Some("text/html".into()),
            status: Some((200, 399)),
            real_client_ip: false,
            operations: vec![Operation::Vary("Accept-Encoding".into())],
        };
        assert!(policy.applies(200, Some("TEXT/HTML; charset=utf-8")));
        assert!(!policy.applies(400, Some("text/html")));
        policy.apply(&mut headers, &ctx, &[], &[]).unwrap();
        assert_eq!(headers["vary"], "Accept, accept-encoding");
        policy.operations = vec![Operation::Vary("*".into())];
        policy.apply(&mut headers, &ctx, &[], &[]).unwrap();
        assert_eq!(headers["vary"], "*");
    }
}
