use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::errno::Errno;
use pingora::http::ResponseHeader;
use pingora::proxy::Session;
use pingora::{Error, ErrorType, Result};
use tokio::sync::OwnedSemaphorePermit;

use super::PreparedStatic;
use super::{path, response};

struct Opened {
    file: File,
    metadata: Metadata,
    path: PathBuf,
    _permit: OwnedSemaphorePermit,
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
        Some(Errno::EMFILE | Errno::ENFILE | Errno::ENOMEM) => 503,
        _ => 500,
    }
}

fn open(
    route: &PreparedStatic,
    requested: &Path,
    navigation: bool,
    permit: OwnedSemaphorePermit,
) -> io::Result<Opened> {
    let (file, served_path) = match route.root.open(requested) {
        Ok(file) => {
            if file.metadata()?.is_dir() {
                let index = requested.join("index.html");
                (route.root.open(&index)?, index)
            } else {
                (file, requested.to_path_buf())
            }
        }
        Err(error) if missing(&error) && navigation && route.spa => (
            route.root.open(Path::new("index.html"))?,
            PathBuf::from("index.html"),
        ),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::from_raw_os_error(Errno::ENOENT as i32));
    }
    Ok(Opened {
        file,
        metadata,
        path: served_path,
        _permit: permit,
    })
}

fn error_header(status: u16) -> Result<ResponseHeader> {
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header("content-length", "0")?;
    if status == 405 {
        header.insert_header("allow", "GET, HEAD")?;
    }
    Ok(header)
}

pub async fn serve<F>(
    route: Arc<PreparedStatic>,
    session: &mut Session,
    requested: &str,
    on_headers: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send,
{
    let (header, body) = if !matches!(session.req_header().method.as_str(), "GET" | "HEAD") {
        (error_header(405)?, None)
    } else {
        match path::decode(requested) {
            Err(status) => (error_header(status)?, None),
            Ok(requested) => {
                let navigation = path::navigation(session.req_header(), &requested);
                let permit = route
                    .capacity
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| failure("static capacity closed"))?;
                let opener = route.clone();
                let opened = tokio::task::spawn_blocking(move || {
                    open(&opener, &requested, navigation, permit)
                })
                .await
                .map_err(|_| failure("static open task failed"))?;
                match opened {
                    Err(error) => (error_header(status(&error))?, None),
                    Ok(opened) => {
                        let plan = response::plan(
                            session.req_header(),
                            &opened.metadata,
                            &opened.path,
                            route.cache_control.as_deref(),
                        )?;
                        let body = plan.send_body.then_some((opened, plan.offset, plan.length));
                        (plan.header, body)
                    }
                }
            }
        }
    };
    on_headers()?;
    session
        .write_response_header(Box::new(header), body.is_none())
        .await?;
    if let Some((mut opened, offset, mut remaining)) = body {
        let mut first = true;
        while remaining != 0 {
            let amount = remaining.min(64 * 1024) as usize;
            let seek = first.then_some(offset);
            first = false;
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
            session
                .write_response_body(Some(bytes.into()), remaining == 0)
                .await?;
        }
    }
    Ok(())
}
