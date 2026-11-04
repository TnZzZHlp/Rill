use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::config::source::Located;
use crate::config::{ConfigError, Result};

use super::root::Root;

const MAX_ERROR_PAGE_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ErrorPage {
    root: Root,
    name: OsString,
}

pub(crate) struct OpenedErrorPage {
    pub(crate) file: File,
    pub(crate) metadata: Metadata,
    pub(crate) mime_path: PathBuf,
}

impl ErrorPage {
    pub(crate) fn prepare(path: &Located<PathBuf>) -> Result<Self> {
        let invalid = || {
            ConfigError::new(
                "RESOURCE_IO",
                "error-page",
                "cannot securely prepare error page parent",
                &path.origin,
            )
        };
        let last = path
            .value
            .as_os_str()
            .as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .ok_or_else(invalid)?;
        if last.is_empty() || last == b"." || last == b".." {
            return Err(invalid());
        }
        let name = path.value.file_name().ok_or_else(invalid)?.to_os_string();
        let parent = path
            .value
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(invalid)?;
        let parent = Located::new(parent.to_path_buf(), path.origin.clone());
        let root = Root::prepare(&parent).map_err(|_| invalid())?;
        Ok(Self { root, name })
    }

    pub(crate) fn open(&self) -> io::Result<OpenedErrorPage> {
        let path = Path::new(&self.name);
        let file = self.root.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_ERROR_PAGE_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(OpenedErrorPage {
            file,
            metadata,
            mime_path: path.to_path_buf(),
        })
    }
}
