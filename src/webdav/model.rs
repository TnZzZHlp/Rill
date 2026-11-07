use crate::config::source::{Located, Origin};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct WebDav {
    pub root: Located<PathBuf>,
    pub storage_id: String,
    pub origin: Origin,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct StorageIdentity {
    pub storage_id: Box<str>,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct DavError {
    pub status: u16,
    pub code: &'static str,
}

impl DavError {
    pub const fn new(status: u16, code: &'static str) -> Self {
        Self { status, code }
    }
    pub const fn invalid() -> Self {
        Self::new(400, "DAV_INVALID")
    }
    pub const fn limit() -> Self {
        Self::new(503, "DAV_LIMIT")
    }
}

impl From<std::io::Error> for DavError {
    fn from(error: std::io::Error) -> Self {
        use nix::errno::Errno;
        let status = match error.raw_os_error().map(Errno::from_raw) {
            Some(Errno::ENOENT) => 404,
            Some(Errno::ENOTDIR | Errno::EEXIST | Errno::EXDEV | Errno::ENOTEMPTY) => 409,
            Some(Errno::EACCES | Errno::EPERM | Errno::ELOOP | Errno::EROFS) => 403,
            Some(Errno::ENOSPC | Errno::EDQUOT | Errno::EFBIG) => 507,
            _ => 500,
        };
        Self::new(status, "DAV_IO")
    }
}
impl From<nix::errno::Errno> for DavError {
    fn from(error: nix::errno::Errno) -> Self {
        std::io::Error::from_raw_os_error(error as i32).into()
    }
}
impl std::fmt::Display for DavError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code)
    }
}
impl std::error::Error for DavError {}
pub type Result<T> = std::result::Result<T, DavError>;
