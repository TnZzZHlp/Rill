use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use nix::errno::Errno;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::Session;
use pingora::{Error, ErrorType, Result};
use tokio::sync::OwnedSemaphorePermit;

use super::PreparedStatic;
use super::cache::{FileVersion, Key, Lookup};
use super::representation::{self, Encoding};
use super::{listing, path, response};

struct Opened {
    file: File,
    metadata: Metadata,
    path: PathBuf,
    encoding: Encoding,
    cacheable: bool,
    _permit: OwnedSemaphorePermit,
}

enum Resource {
    File(Opened),
    Listing(Vec<u8>),
}

enum Body {
    File(Box<Opened>, u64, u64),
    Listing(Vec<u8>),
    Cached(Bytes, Box<Opened>),
    Retained(Bytes, Option<Box<Opened>>),
}

fn failure(reason: &'static str) -> Box<Error> {
    Error::explain(ErrorType::HTTPStatus(500), reason)
}

fn missing(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(value) if value == Errno::ENOENT as i32 || value == Errno::ENOTDIR as i32)
}

fn status(error: &io::Error) -> u16 {
    if missing(error) {
        return 404;
    }
    match error.raw_os_error().map(Errno::from_raw) {
        Some(Errno::EACCES | Errno::EPERM | Errno::EXDEV | Errno::ELOOP | Errno::EAGAIN) => 403,
        Some(Errno::EINVAL | Errno::ENAMETOOLONG) => 400,
        Some(Errno::EMFILE | Errno::ENFILE | Errno::ENOMEM | Errno::ESTALE) => 503,
        _ => 500,
    }
}

fn open(
    route: &PreparedStatic,
    requested: &Path,
    navigation: bool,
    permit: OwnedSemaphorePermit,
    candidates: &[Encoding],
    mode: (bool, bool),
    transform: Option<&crate::body_transform::headers::StaticOptions>,
) -> io::Result<Option<Resource>> {
    let (error_page, strict) = mode;
    let (file, served_path) = match route.root.open(requested) {
        Ok(file) => {
            if file.metadata()?.is_dir() {
                if error_page {
                    return Err(io::Error::from_raw_os_error(Errno::EACCES as i32));
                }
                let index = requested.join(&route.index);
                match route.root.open(&index) {
                    Ok(index_file) => (index_file, index),
                    Err(error)
                        if missing(&error)
                            && (!strict || error.raw_os_error() == Some(Errno::ENOENT as i32))
                            && route.browse =>
                    {
                        if !candidates.contains(&Encoding::Identity)
                            && !transform.is_some_and(|t| t.identity("text/html"))
                        {
                            return Ok(None);
                        }
                        let bytes =
                            listing::render(&route.root, requested, file).map_err(|error| {
                                match error {
                                    listing::ListingError::Io(error) => error,
                                    listing::ListingError::Limit => {
                                        io::Error::from_raw_os_error(Errno::EMFILE as i32)
                                    }
                                }
                            })?;
                        return Ok(Some(Resource::Listing(bytes)));
                    }
                    Err(error) => return Err(error),
                }
            } else {
                (file, requested.to_path_buf())
            }
        }
        Err(error)
            if missing(&error)
                && (!strict || error.raw_os_error() == Some(Errno::ENOENT as i32))
                && navigation
                && route.spa =>
        {
            (
                route.root.open(Path::new(&route.index))?,
                PathBuf::from(&route.index),
            )
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::from_raw_os_error(if strict {
            Errno::EACCES as i32
        } else {
            Errno::ENOENT as i32
        }));
    }
    let mime = response::mime_type(&served_path);
    let mut candidates = candidates.to_vec();
    if transform.is_some_and(|t| t.replace(mime)) {
        candidates = vec![Encoding::Identity];
    } else if transform.is_some_and(|t| t.identity(mime))
        && !candidates.contains(&Encoding::Identity)
    {
        candidates.push(Encoding::Identity);
    }
    for encoding in candidates {
        if encoding == Encoding::Identity {
            return Ok(Some(Resource::File(Opened {
                file,
                metadata,
                path: served_path,
                encoding,
                cacheable: true,
                _permit: permit,
            })));
        }
        let mut sidecar = served_path.as_os_str().to_os_string();
        sidecar.push(match encoding {
            Encoding::Brotli => ".br",
            Encoding::Gzip => ".gz",
            Encoding::Identity => unreachable!(),
        });
        let sidecar = match route.root.open(Path::new(&sidecar)) {
            Ok(file) => file,
            Err(error)
                if missing(&error)
                    && (!strict || error.raw_os_error() == Some(Errno::ENOENT as i32)) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let sidecar_metadata = sidecar.metadata()?;
        if !sidecar_metadata.is_file() {
            return Err(io::Error::from_raw_os_error(Errno::EACCES as i32));
        }
        if sidecar_metadata.modified()? < metadata.modified()? {
            continue;
        }
        return Ok(Some(Resource::File(Opened {
            file: sidecar,
            metadata: sidecar_metadata,
            path: served_path,
            encoding,
            cacheable: true,
            _permit: permit,
        })));
    }
    Ok(None)
}

fn cache_body(
    route: &PreparedStatic,
    mut opened: Box<Opened>,
    offset: u64,
    length: u64,
) -> io::Result<Body> {
    let Some(binding) = route.cache.as_ref().filter(|_| opened.cacheable) else {
        return Ok(Body::File(opened, offset, length));
    };
    let version = FileVersion::from(&opened.metadata);
    let key = Key {
        namespace: binding.namespace,
        generation: binding.generation,
        route: binding.route,
        root: route.root.identity()?,
        path: opened.path.clone(),
        encoding: match opened.encoding {
            Encoding::Identity => "identity",
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
        },
        version: version.clone(),
    };
    let bytes = match binding.pool.lookup_or_begin(key) {
        Lookup::Stream => return Ok(Body::File(opened, offset, length)),
        Lookup::Hit(bytes) => {
            if FileVersion::from(&opened.file.metadata()?) != version {
                return Err(io::Error::from_raw_os_error(Errno::ESTALE as i32));
            }
            bytes
        }
        Lookup::Load(guard, permit) => {
            if FileVersion::from(&opened.file.metadata()?) != version {
                return Err(io::Error::from_raw_os_error(Errno::ESTALE as i32));
            }
            let mut body = permit.allocate();
            opened.file.read_exact(body.as_mut_slice())?;
            let mut extra = [0; 1];
            if opened.file.read(&mut extra)? != 0
                || FileVersion::from(&opened.file.metadata()?) != version
            {
                return Err(io::Error::from_raw_os_error(Errno::ESTALE as i32));
            }
            guard
                .publish(body)
                .ok_or_else(|| io::Error::from_raw_os_error(Errno::ESTALE as i32))?
        }
    };
    let start =
        usize::try_from(offset).map_err(|_| io::Error::from_raw_os_error(Errno::ESTALE as i32))?;
    let end = usize::try_from(length)
        .ok()
        .and_then(|length| start.checked_add(length))
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| io::Error::from_raw_os_error(Errno::ESTALE as i32))?;
    Ok(Body::Cached(bytes.slice(start..end), opened))
}

fn error_header(status: u16) -> Result<ResponseHeader> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("content-length", "0")?;
    if status == 405 {
        header.insert_header("allow", "GET, HEAD")?;
    }
    Ok(header)
}

async fn custom_error(
    route: &Arc<PreparedStatic>,
    header: ResponseHeader,
    is_head: bool,
    identity_allowed: bool,
    request: &RequestHeader,
    policy: Option<&crate::http_policy::state::RequestPolicyState>,
) -> Result<(ResponseHeader, Option<Body>)> {
    let status = header.status.as_u16();
    if (!identity_allowed
        && !policy.is_some_and(crate::http_policy::state::RequestPolicyState::has_body_transform))
        || !route.error_pages.contains_key(&status)
    {
        return Ok((header, None));
    }
    let permit = route
        .capacity
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| failure("static capacity closed"))?;
    let opener = route.clone();
    let opened = tokio::task::spawn_blocking(move || {
        opener
            .error_pages
            .get(&status)
            .and_then(|page| page.open().ok())
            .map(|page| (page, permit))
    })
    .await
    .map_err(|_| failure("error page open task failed"))?;
    let Some((page, permit)) = opened else {
        return Ok((header, None));
    };
    if !identity_allowed {
        let mut probe = ResponseHeader::build(status, None)?;
        probe.insert_header("content-type", response::mime_type(&page.mime_path))?;
        if !crate::body_transform::headers::select(request, &probe, policy, true)
            .ok()
            .flatten()
            .is_some_and(|s| s.active())
        {
            return Ok((header, None));
        }
    }
    let mut result = error_header(status)?;
    if let Some(value) = header.headers.get("content-range") {
        result.insert_header("content-range", value.clone())?;
    }
    if let Some(value) = header.headers.get("vary") {
        result.insert_header("vary", value.clone())?;
    }
    result.insert_header("cache-control", "no-store")?;
    result.insert_header("content-type", response::mime_type(&page.mime_path))?;
    let length = page.metadata.len();
    result.insert_header("content-length", length.to_string())?;
    let body = if is_head || length == 0 {
        None
    } else {
        Some(Body::File(
            Box::new(Opened {
                file: page.file,
                metadata: page.metadata,
                path: page.mime_path,
                encoding: Encoding::Identity,
                cacheable: false,
                _permit: permit,
            }),
            0,
            length,
        ))
    };
    Ok((result, body))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalError {
    Missing,
    Method,
    Unavailable,
}

pub struct PreparedResponse {
    pub header: ResponseHeader,
    body: Option<Body>,
    pub local_error: Option<LocalError>,
}

impl PreparedResponse {
    pub(crate) fn retain_source(
        &mut self,
        budget: &Arc<crate::body_transform::budget::ResponseBudget>,
    ) -> Result<()> {
        let capacity = match &self.body {
            Some(Body::Listing(bytes)) => bytes.capacity(),
            Some(Body::Cached(_, opened)) => {
                usize::try_from(opened.metadata.len()).map_err(|_| {
                    crate::body_transform::pipeline::proxy_error(
                        crate::body_transform::limits::Failure::Budget,
                    )
                })?
            }
            _ => return Ok(()),
        };
        let permit = budget.reserve(capacity).ok_or_else(|| {
            crate::body_transform::pipeline::proxy_error(
                crate::body_transform::limits::Failure::Budget,
            )
        })?;
        self.body = match self.body.take() {
            Some(Body::Listing(bytes)) => Some(Body::Retained(permit.retain_source(bytes), None)),
            Some(Body::Cached(bytes, opened)) => {
                Some(Body::Retained(permit.retain_source(bytes), Some(opened)))
            }
            _ => return Err(failure("invalid static source handoff")),
        };
        Ok(())
    }
}

pub async fn prepare(
    route: Arc<PreparedStatic>,
    request: &RequestHeader,
    requested: &str,
    defer_errors: bool,
    error_page: bool,
) -> Result<PreparedResponse> {
    prepare_with_policy(route, request, requested, defer_errors, error_page, None).await
}

pub async fn prepare_with_policy(
    route: Arc<PreparedStatic>,
    request: &RequestHeader,
    requested: &str,
    defer_errors: bool,
    error_page: bool,
    policy: Option<&crate::http_policy::state::RequestPolicyState>,
) -> Result<PreparedResponse> {
    let mut local_error = None;
    let is_head = request.method.as_str() == "HEAD";
    let identity_allowed = !representation::ranked_request(request, false).is_empty();
    let (header, body) = match path::decode(requested) {
        Err(status) => (error_header(status)?, None),
        Ok(_) if !matches!(request.method.as_str(), "GET" | "HEAD") => {
            local_error = Some(LocalError::Method);
            (error_header(405)?, None)
        }
        Ok(requested) => {
            let navigation = !error_page && path::navigation(request, &requested);
            let candidates = representation::ranked_request(request, route.precompressed);
            let permit = route
                .capacity
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| failure("static capacity closed"))?;
            let transform = crate::body_transform::headers::StaticOptions::new(
                request,
                policy,
                route.cache_control.as_deref(),
            );
            let opener = route.clone();
            let opened = tokio::task::spawn_blocking(move || {
                open(
                    &opener,
                    &requested,
                    navigation,
                    permit,
                    &candidates,
                    (error_page, defer_errors || error_page),
                    transform.as_ref(),
                )
            })
            .await
            .map_err(|_| failure("static open task failed"))?;
            match opened {
                Err(error) => {
                    if error.raw_os_error() == Some(Errno::ENOENT as i32) {
                        local_error = Some(LocalError::Missing);
                    } else if error.raw_os_error() == Some(Errno::ESTALE as i32) {
                        local_error = Some(LocalError::Unavailable);
                    }
                    let status = if (defer_errors || error_page)
                        && error.raw_os_error() == Some(Errno::ENOTDIR as i32)
                    {
                        403
                    } else {
                        status(&error)
                    };
                    (error_header(status)?, None)
                }
                Ok(None) => {
                    let mut header = error_header(406)?;
                    header.insert_header("Vary", "Accept-Encoding")?;
                    (header, None)
                }
                Ok(Some(Resource::Listing(bytes))) => {
                    let mut conditions_request = request.clone();
                    suppress_static_validators(
                        &mut conditions_request,
                        policy,
                        "text/html; charset=utf-8",
                        Encoding::Identity,
                        None,
                    )?;
                    let values = conditions_request.headers.get_all("if-none-match");
                    let mut conditions = values.iter();
                    let not_modified = conditions
                        .next()
                        .is_some_and(|value| value.as_bytes().trim_ascii() == b"*")
                        && conditions.next().is_none();
                    let mut header =
                        ResponseHeader::build(if not_modified { 304 } else { 200 }, None)?;
                    header.insert_header("content-type", "text/html; charset=utf-8")?;
                    if !not_modified {
                        header.insert_header("content-length", bytes.len().to_string())?;
                    }
                    header.insert_header("cache-control", "no-store")?;
                    header.insert_header("vary", "Accept-Encoding")?;
                    header.insert_header(
                        "content-security-policy",
                        "default-src 'none'; style-src 'none'",
                    )?;
                    let body = (!is_head && !not_modified && !bytes.is_empty())
                        .then_some(Body::Listing(bytes));
                    (header, body)
                }
                Ok(Some(Resource::File(opened))) => {
                    let mut conditions_request = request.clone();
                    suppress_static_validators(
                        &mut conditions_request,
                        policy,
                        response::mime_type(&opened.path),
                        opened.encoding,
                        route.cache_control.as_deref(),
                    )?;
                    let plan = response::plan(
                        &conditions_request,
                        &opened.metadata,
                        &opened.path,
                        route.cache_control.as_deref(),
                        opened.encoding,
                        route.precompressed,
                    )?;
                    if plan.send_body
                        && route.cache.is_some()
                        && opened.cacheable
                        && opened.metadata.len() <= 64 * 1024
                    {
                        let cache_route = route.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            cache_body(&cache_route, Box::new(opened), plan.offset, plan.length)
                        })
                        .await
                        .map_err(|_| failure("static cache task failed"))?;
                        match result {
                            Ok(body) => (plan.header, Some(body)),
                            Err(_) => (error_header(503)?, None),
                        }
                    } else {
                        let body = plan.send_body.then_some(Body::File(
                            Box::new(opened),
                            plan.offset,
                            plan.length,
                        ));
                        (plan.header, body)
                    }
                }
            }
        }
    };
    let (header, body) = if body.is_none()
        && !(defer_errors && local_error.is_some())
        && matches!(
            header.status.as_u16(),
            403 | 404 | 405 | 406 | 416 | 500 | 503
        ) {
        let mut header = header;
        header.insert_header("vary", "Accept-Encoding")?;
        custom_error(&route, header, is_head, identity_allowed, request, policy).await?
    } else {
        (header, body)
    };
    Ok(PreparedResponse {
        header,
        body,
        local_error,
    })
}

fn suppress_static_validators(
    request: &mut RequestHeader,
    policy: Option<&crate::http_policy::state::RequestPolicyState>,
    mime: &str,
    encoding: Encoding,
    cache_control: Option<&str>,
) -> Result<()> {
    let mut probe = ResponseHeader::build(200, None)?;
    probe.insert_header("content-type", mime)?;
    if let Some(cache_control) = cache_control {
        probe.insert_header("cache-control", cache_control)?;
    }
    match encoding {
        Encoding::Gzip => probe.insert_header("content-encoding", "gzip")?,
        Encoding::Brotli => probe.insert_header("content-encoding", "br")?,
        Encoding::Identity => (),
    }
    if match crate::body_transform::headers::select(request, &probe, policy, true) {
        Ok(selection) => selection.is_some_and(|s| s.active()),
        Err(_) => true,
    } {
        request.remove_header("if-none-match");
        request.remove_header("if-modified-since");
    }
    Ok(())
}

pub async fn serve<F>(
    route: Arc<PreparedStatic>,
    session: &mut Session,
    requested: &str,
    on_headers: F,
) -> Result<()>
where
    F: FnOnce(&mut ResponseHeader) -> Result<()> + Send,
{
    let prepared = prepare(route, session.req_header(), requested, false, false).await?;
    send(session, prepared, on_headers).await
}

pub async fn send<F>(session: &mut Session, prepared: PreparedResponse, on_headers: F) -> Result<()>
where
    F: FnOnce(&mut ResponseHeader) -> Result<()> + Send,
{
    send_transformed(
        session,
        prepared,
        crate::body_transform::response::Transform::default(),
        on_headers,
    )
    .await
}

pub(crate) async fn send_transformed<F>(
    session: &mut Session,
    prepared: PreparedResponse,
    mut transform: crate::body_transform::response::Transform,
    on_headers: F,
) -> Result<()>
where
    F: FnOnce(&mut ResponseHeader) -> Result<()> + Send,
{
    let PreparedResponse {
        mut header, body, ..
    } = prepared;
    on_headers(&mut header)?;
    session
        .write_response_header(Box::new(header), body.is_none() && !transform.enabled())
        .await?;
    match body {
        Some(Body::Listing(bytes)) => transform.write(session, Some(bytes.into()), true).await?,
        Some(Body::Cached(bytes, _opened)) => transform.write(session, Some(bytes), true).await?,
        Some(Body::Retained(bytes, _opened)) => transform.write(session, Some(bytes), true).await?,
        Some(Body::File(mut opened, offset, mut remaining)) => {
            let mut first = true;
            while remaining != 0 {
                let amount = remaining.min(64 * 1024) as usize;
                let seek = first.then_some(offset);
                first = false;
                if let Some(mut buffer) = transform.input_buffer(amount)? {
                    let (returned, buffer) =
                        tokio::task::spawn_blocking(move || -> io::Result<_> {
                            if let Some(offset) = seek {
                                opened.file.seek(SeekFrom::Start(offset))?;
                            }
                            opened.file.read_exact(buffer.data_mut())?;
                            Ok((opened, buffer))
                        })
                        .await
                        .map_err(|_| failure("static read task failed"))?
                        .map_err(|_| failure("static file read failed"))?;
                    opened = returned;
                    remaining -= buffer.data().len() as u64;
                    transform
                        .write_buffer(session, buffer, remaining == 0)
                        .await?;
                    continue;
                }
                let (returned, bytes) = tokio::task::spawn_blocking(move || -> io::Result<_> {
                    if let Some(offset) = seek {
                        opened.file.seek(SeekFrom::Start(offset))?;
                    }
                    let mut bytes = vec![0; amount];
                    opened.file.read_exact(&mut bytes)?;
                    Ok((opened, bytes))
                })
                .await
                .map_err(|_| failure("static read task failed"))?
                .map_err(|_| failure("static file read failed"))?;
                opened = returned;
                remaining -= bytes.len() as u64;
                transform
                    .write(session, Some(bytes.into()), remaining == 0)
                    .await?;
            }
        }
        None if transform.enabled() => transform.write(session, None, true).await?,
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[test]
    fn cache_hit_rechecks_opened_file_version()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use crate::config::source::{Located, Origin};
        use crate::static_files::cache::{self, Binding};
        use crate::static_files::root::Root;

        let namespace = cache::next_namespace().ok_or("namespace exhausted")?;
        let dir =
            std::env::temp_dir().join(format!("rill-cache-hit-{}-{namespace}", std::process::id()));
        fs::create_dir(&dir)?;
        let result = (|| -> std::result::Result<(Option<i32>, u64), Box<dyn std::error::Error>> {
            let path = dir.join("file");
            fs::write(&path, b"cached")?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            let capacity = Arc::new(Semaphore::new(2));
            let pool = cache::register(65536)
                .map_err(io::Error::other)?
                .ok_or("cache disabled")?;
            let route = PreparedStatic {
                index: "index.html".to_owned(),
                root: Root::prepare(&Located::new(dir.clone(), Origin::file(&dir)))?,
                cache: Some(Binding {
                    pool: pool.clone(),
                    namespace,
                    generation: 1,
                    route: (0, 0),
                }),
                spa: false,
                precompressed: false,
                browse: false,
                error_pages: BTreeMap::new(),
                cache_control: None,
                capacity: capacity.clone(),
                request: Duration::from_secs(1),
                idle: Duration::from_secs(1),
            };
            let Some(Resource::File(first)) = open(
                &route,
                Path::new("file"),
                false,
                capacity.clone().try_acquire_owned()?,
                &[Encoding::Identity],
                (false, false),
                None,
            )?
            else {
                return Err("first open did not return a file".into());
            };
            if !matches!(
                cache_body(&route, Box::new(first), 0, 6)?,
                Body::Cached(_, _)
            ) {
                return Err("cache was not populated".into());
            }
            let Some(Resource::File(second)) = open(
                &route,
                Path::new("file"),
                false,
                capacity.try_acquire_owned()?,
                &[Encoding::Identity],
                (false, false),
                None,
            )?
            else {
                return Err("second open did not return a file".into());
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
            let error = match cache_body(&route, Box::new(second), 0, 6) {
                Err(error) => error.raw_os_error(),
                Ok(_) => None,
            };
            Ok((error, pool.stats().hits))
        })();
        fs::remove_dir_all(&dir)?;
        let (error, hits) = result?;
        assert_eq!(hits, 1);
        assert_eq!(error, Some(Errno::ESTALE as i32));
        Ok(())
    }
}
