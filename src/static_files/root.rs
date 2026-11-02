use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat, openat2};
use nix::sys::stat::Mode;
use nix::sys::statfs::{PROC_SUPER_MAGIC, fstatfs};

use crate::config::source::Located;
use crate::config::{ConfigError, Result as ConfigResult};

#[derive(Debug)]
pub struct Root {
    root: File,
    proc_fd: File,
}

fn io_error(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type() == right.file_type()
}

fn constrained_open(root: &File, path: &Path) -> io::Result<OwnedFd> {
    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    openat2(root, path, how).map_err(io_error)
}

fn reopen(proc_fd: &File, fd: &OwnedFd, directory: bool) -> io::Result<File> {
    let mut flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
    if directory {
        flags |= OFlag::O_DIRECTORY;
    }
    let name = fd.as_raw_fd().to_string();
    openat(proc_fd, name.as_str(), flags, Mode::empty())
        .map(File::from)
        .map_err(io_error)
}

impl Root {
    pub fn prepare(root: &Located<PathBuf>) -> ConfigResult<Self> {
        Self::prepare_inner(&root.value).map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "root",
                "cannot securely prepare static root or required kernel resources",
                &root.origin,
            )
        })
    }

    fn prepare_inner(path: &Path) -> io::Result<Self> {
        let root = File::from(
            open(
                path,
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(io_error)?,
        );
        let probe = constrained_open(&root, Path::new("."))?;
        let probe_metadata = File::from(probe).metadata()?;
        if !probe_metadata.is_dir() || !same_file(&root.metadata()?, &probe_metadata) {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }

        let pid = std::process::id().to_string();
        if fs::read_link("/proc/self")? != Path::new(&pid) {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        let proc_fd = File::open(format!("/proc/{pid}/fd"))?;
        if !proc_fd.metadata()?.is_dir()
            || fstatfs(&proc_fd).map_err(io_error)?.filesystem_type() != PROC_SUPER_MAGIC
        {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        let probe = constrained_open(&root, Path::new("."))?;
        let reopened = reopen(&proc_fd, &probe, true)?;
        if !same_file(&root.metadata()?, &reopened.metadata()?) {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        Ok(Self { root, proc_fd })
    }

    pub fn open(&self, relative: &Path) -> io::Result<File> {
        if relative.components().any(|component| {
            matches!(
                component,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir
            )
        }) {
            return Err(io::Error::from_raw_os_error(Errno::EINVAL as i32));
        }
        let path = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let fd = constrained_open(&self.root, path)?;
        let path_file = File::from(fd);
        let metadata = path_file.metadata()?;
        if metadata.is_dir() {
            return Ok(path_file);
        }
        if !metadata.is_file() {
            return Err(io::Error::from_raw_os_error(Errno::EACCES as i32));
        }
        let fd: OwnedFd = path_file.into();
        let file = reopen(&self.proc_fd, &fd, false)?;
        if !same_file(&metadata, &file.metadata()?) {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        Ok(file)
    }
}
