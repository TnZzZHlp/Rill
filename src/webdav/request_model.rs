use super::{
    authorization::Authorization,
    conditions::{self, IfHeader, Submitted},
    model::{DavError, Result},
    namespace,
    path::DavPath,
    registry::StorageHandle,
};
use http::HeaderMap;
use std::collections::BTreeMap;

pub struct DavRequest {
    pub method: http::Method,
    pub path: DavPath,
    pub headers: HeaderMap,
    pub destination: Option<DavPath>,
    pub destination_external: Option<DavPath>,
    pub overwrite: bool,
    pub depth: usize,
    pub timeout: u64,
    pub conditions: Option<IfHeader>,
    tagged: BTreeMap<String, DavPath>,
    pub authorization: Authorization,
    pub namespace: std::sync::Arc<namespace::NamespaceSession>,
    _memory: super::budget::Memory,
}
pub fn single<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|v| v.to_str().map_err(|_| DavError::invalid()))
        .transpose()?;
    if values.next().is_some() {
        return Err(DavError::invalid());
    }
    Ok(value)
}
impl DavRequest {
    pub fn prepare(
        method: http::Method,
        path: &str,
        headers: HeaderMap,
        mut authorization: Authorization,
        mut memory: super::budget::Memory,
        namespace: std::sync::Arc<namespace::NamespaceSession>,
    ) -> Result<Self> {
        let path = DavPath::parse(path)?;
        authorization.check_source(&path)?;
        namespace::metadata(&namespace, &path)?;
        let depth = match single(&headers, "depth")? {
            None => usize::MAX,
            Some("0") => 0,
            Some("1") => 1,
            Some("infinity") => usize::MAX,
            _ => return Err(DavError::invalid()),
        };
        let overwrite = match single(&headers, "overwrite")? {
            None | Some("T") => true,
            Some("F") => false,
            _ => return Err(DavError::invalid()),
        };
        let destination = if matches!(method.as_str(), "COPY" | "MOVE") {
            Some(
                authorization
                    .map(single(&headers, "destination")?.ok_or_else(DavError::invalid)?)?,
            )
        } else {
            None
        };
        if let Some(destination) = &destination {
            if destination.is_root() || path.contains(destination) || destination.contains(&path) {
                return Err(DavError::new(403, "DAV_DESTINATION_RELATION"));
            }
            namespace::metadata(&namespace, destination)?;
        }
        let destination_external = if destination.is_some() {
            let uri: http::Uri = single(&headers, "destination")?
                .ok_or_else(DavError::invalid)?
                .parse()
                .map_err(|_| DavError::invalid())?;
            Some(DavPath::parse(uri.path())?)
        } else {
            None
        };
        let conditions = single(&headers, "if")?.map(IfHeader::parse).transpose()?;
        let mut tagged = BTreeMap::new();
        if let Some(conditions) = &conditions {
            for group in &conditions.groups {
                if let Some(resource) = &group.resource {
                    let path = authorization.map(resource)?;
                    let uri: http::Uri = resource.parse().map_err(|_| DavError::invalid())?;
                    let external = DavPath::parse(uri.path())?;
                    authorization.href_from(&external, &path, &path)?;
                    namespace::metadata(&namespace, &path)?;
                    memory.reserve(resource.len() + path.key().len() + 256)?;
                    tagged.insert(resource.clone(), path);
                }
            }
        }
        let timeout = match single(&headers, "timeout")? {
            None => 3600,
            Some(value) => value
                .split(',')
                .find_map(|value| {
                    let value = value.trim();
                    if value == "Infinite" {
                        Some(3600)
                    } else {
                        value
                            .strip_prefix("Second-")
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|s| *s > 0)
                            .map(|s| s.min(3600))
                    }
                })
                .ok_or_else(DavError::invalid)?,
        };
        Ok(Self {
            method,
            path,
            headers,
            destination,
            destination_external,
            overwrite,
            depth,
            timeout,
            conditions,
            tagged,
            authorization,
            namespace,
            _memory: memory,
        })
    }
    pub fn submitted(&self, storage: &StorageHandle) -> Result<Submitted> {
        let locks = storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(conditions) = &self.conditions else {
            return Ok(Submitted::default());
        };
        conditions.evaluate(
            &self.path,
            |resource| {
                self.tagged
                    .get(resource)
                    .cloned()
                    .ok_or_else(DavError::invalid)
            },
            |path| {
                namespace::metadata(&self.namespace, path).map(|m| m.map(|m| namespace::etag(&m)))
            },
            |path, token| locks.valid(path, token),
        )
    }
    pub fn check(&self, storage: &StorageHandle) -> Result<Submitted> {
        let metadata = namespace::metadata(&self.namespace, &self.path)?;
        let tag = metadata.as_ref().map(namespace::etag);
        if let Some(value) = single(&self.headers, "if-match")? {
            if value.trim() == "*" {
                if metadata.is_none() {
                    return Err(DavError::new(412, "DAV_PRECONDITION"));
                }
            } else if !etag_list(value)?
                .iter()
                .any(|candidate| !candidate.starts_with("W/") && tag.as_deref() == Some(candidate))
            {
                return Err(DavError::new(412, "DAV_PRECONDITION"));
            }
        }
        if let Some(value) = single(&self.headers, "if-none-match")? {
            let matched = if value.trim() == "*" {
                metadata.is_some()
            } else {
                etag_list(value)?.iter().any(|candidate| {
                    tag.as_deref()
                        .is_some_and(|tag| conditions::weak_equal(candidate, tag))
                })
            };
            if matched {
                return Err(DavError::new(
                    if matches!(self.method.as_str(), "GET" | "HEAD") {
                        304
                    } else {
                        412
                    },
                    "DAV_PRECONDITION",
                ));
            }
        }
        if !self.headers.contains_key("if-match")
            && let (Some(value), Some(metadata)) = (
                single(&self.headers, "if-unmodified-since")?,
                metadata.as_ref(),
            )
            && let (Ok(date), Ok(modified)) =
                (httpdate::parse_http_date(value), metadata.modified())
            && modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
                > date
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs())
        {
            return Err(DavError::new(412, "DAV_PRECONDITION"));
        }
        self.submitted(storage)
    }
}
fn etag_list(value: &str) -> Result<Vec<&str>> {
    let mut rest = value.trim();
    let mut tags = Vec::new();
    loop {
        let raw = rest.strip_prefix("W/").unwrap_or(rest);
        if !raw.starts_with('"') {
            return Err(DavError::invalid());
        }
        let end = raw[1..]
            .find('"')
            .map(|n| n + 2)
            .ok_or_else(DavError::invalid)?;
        let length = rest.len() - raw.len() + end;
        let tag = &rest[..length];
        if !conditions::valid_etag(tag) {
            return Err(DavError::invalid());
        }
        tags.push(tag);
        rest = rest[length..].trim_start();
        if rest.is_empty() {
            return Ok(tags);
        }
        rest = rest
            .strip_prefix(',')
            .ok_or_else(DavError::invalid)?
            .trim_start();
    }
}
