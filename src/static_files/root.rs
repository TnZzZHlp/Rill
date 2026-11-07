use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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

fn parse_mount_id(bytes: &[u8]) -> io::Result<u64> {
    let invalid = || io::Error::from_raw_os_error(Errno::EIO as i32);
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let mut ids = text.lines().filter_map(|line| line.strip_prefix("mnt_id:"));
    let value = ids.next().ok_or_else(invalid)?.trim();
    if ids.next().is_some() || value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    value.parse().map_err(|_| invalid())
}

fn constrained_open_with_flags(root: &File, path: &Path, flags: OFlag) -> io::Result<OwnedFd> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let how = OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_CLOEXEC | flags)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS);
        match openat2(root, path, how) {
            Err(Errno::EAGAIN) if attempts < 8 => std::thread::yield_now(),
            result => return result.map_err(io_error),
        }
    }
}

fn constrained_open(root: &File, path: &Path) -> io::Result<OwnedFd> {
    constrained_open_with_flags(root, path, OFlag::empty())
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
        Self::from_prepared_fd(root)
    }

    pub(crate) fn from_prepared_fd(root: File) -> io::Result<Self> {
        if !root.metadata()?.is_dir() {
            return Err(io_error(Errno::ENOTDIR));
        }
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

    pub(crate) fn mount_id(&self, file: &File) -> io::Result<u64> {
        let path = format!("../fdinfo/{}", file.as_raw_fd());
        let info = File::from(
            openat(
                &self.proc_fd,
                path.as_str(),
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(io_error)?,
        );
        if fstatfs(&info).map_err(io_error)?.filesystem_type() != PROC_SUPER_MAGIC {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        let mut bytes = Vec::new();
        info.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        parse_mount_id(&bytes)
    }

    pub(crate) fn same_directory(&self, relative: &Path, directory: &File) -> io::Result<bool> {
        let rooted = self.open(relative)?;
        Ok(same_file(&rooted.metadata()?, &directory.metadata()?)
            && self.mount_id(&rooted)? == self.mount_id(directory)?)
    }

    pub(crate) fn same_entry(
        &self,
        relative: &Path,
        directory: &File,
        name: &Path,
        ino: u64,
    ) -> io::Result<bool> {
        let local = File::from(constrained_open_with_flags(
            directory,
            name,
            OFlag::O_NOFOLLOW,
        )?);
        let rooted = File::from(constrained_open_with_flags(
            &self.root,
            relative,
            OFlag::O_NOFOLLOW,
        )?);
        let metadata = local.metadata()?;
        if !same_file(&metadata, &rooted.metadata()?) {
            return Ok(false);
        }
        let local_mount = self.mount_id(&local)?;
        if local_mount != self.mount_id(&rooted)? {
            return Ok(false);
        }
        Ok(metadata.ino() == ino || local_mount != self.mount_id(directory)?)
    }

    pub(crate) fn open_directory(&self, file: File) -> io::Result<nix::dir::Dir> {
        let metadata = file.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::from_raw_os_error(Errno::ENOTDIR as i32));
        }
        let fd: OwnedFd = file.into();
        let reopened = reopen(&self.proc_fd, &fd, true)?;
        if !same_file(&metadata, &reopened.metadata()?) {
            return Err(io::Error::from_raw_os_error(Errno::EIO as i32));
        }
        nix::dir::Dir::from_fd(reopened.into()).map_err(io_error)
    }

    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        let metadata = self.root.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(crate) fn link_anonymous(&self, file: &File, parent: &File, name: &str) -> io::Result<()> {
        let before = file.metadata()?;
        if !before.is_file() || before.nlink() != 0 || !parent.metadata()?.is_dir() {
            return Err(io_error(Errno::EACCES));
        }
        let number = file.as_raw_fd().to_string();
        nix::unistd::linkat(
            &self.proc_fd,
            number.as_str(),
            parent,
            name,
            nix::fcntl::AtFlags::AT_SYMLINK_FOLLOW,
        )
        .map_err(io_error)?;
        let named = File::from(constrained_open_with_flags(
            parent,
            Path::new(name),
            OFlag::O_NOFOLLOW,
        )?);
        if !same_file(&before, &named.metadata()?) || named.metadata()?.nlink() != 1 {
            return Err(io_error(Errno::EIO));
        }
        Ok(())
    }

    pub(crate) fn private_directory(&self, directory: &File) -> io::Result<bool> {
        let mut current = directory.try_clone()?;
        for _ in 0..4096 {
            let metadata = current.metadata()?;
            if !metadata.is_dir() {
                return Err(io_error(Errno::ENOTDIR));
            }
            let parent = File::from(
                openat(
                    &current,
                    "..",
                    OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io_error)?,
            );
            let candidate = openat(
                &parent,
                ".rill-dav",
                OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            );
            match candidate {
                Ok(fd) => {
                    let private = File::from(fd);
                    if same_file(&metadata, &private.metadata()?) {
                        return Ok(true);
                    }
                }
                Err(Errno::ENOENT) => (),
                Err(error) => return Err(io_error(error)),
            }
            if same_file(&metadata, &parent.metadata()?)
                && self.mount_id(&current)? == self.mount_id(&parent)?
            {
                return Ok(false);
            }
            current = parent;
        }
        Err(io_error(Errno::EACCES))
    }

    pub(crate) fn capability(&self) -> &File {
        &self.root
    }

    pub(crate) fn reopen_checked(&self, file: File, directory: bool) -> io::Result<File> {
        let metadata = file.metadata()?;
        if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
            return Err(io_error(Errno::EACCES));
        }
        let fd: OwnedFd = file.into();
        let reopened = reopen(&self.proc_fd, &fd, directory)?;
        if !same_file(&metadata, &reopened.metadata()?) {
            return Err(io_error(Errno::EIO));
        }
        Ok(reopened)
    }

    pub fn open(&self, relative: &Path) -> io::Result<File> {
        let visibility = crate::webdav::visibility::read();
        if relative
            .components()
            .any(|part| crate::webdav::visibility::reserved(part.as_os_str()))
        {
            return Err(io_error(Errno::EACCES));
        }
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
        let resolved =
            nix::fcntl::readlinkat(&self.proc_fd, path_file.as_raw_fd().to_string().as_str())
                .map_err(io_error)?;
        if Path::new(std::ffi::OsStr::from_bytes(resolved.as_bytes()))
            .components()
            .any(|part| crate::webdav::visibility::reserved(part.as_os_str()))
            || crate::webdav::visibility::denied(&path_file, &visibility)?
        {
            return Err(io_error(Errno::EACCES));
        }
        if metadata.is_dir() {
            if self.private_directory(&path_file)? {
                return Err(io_error(Errno::EACCES));
            }
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
        if crate::webdav::private_file::marked(&file)? {
            return Err(io_error(Errno::EACCES));
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::{Root, parse_mount_id};
    use crate::config::source::{Located, Origin};
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn parse_fdinfo_mount_id_is_bounded_and_unambiguous() {
        assert_eq!(
            parse_mount_id(b"pos:\t0\nmnt_id:\t42\nino:\t3\n").unwrap(),
            42
        );
        for value in [
            &b"mnt_id: x\n"[..],
            &b"mnt_id: 1\nmnt_id: 2\n"[..],
            &b"pos: 0\n"[..],
        ] {
            assert!(parse_mount_id(value).is_err());
        }
    }

    #[test]
    fn ordinary_entries_and_symlinks_match() -> Result<(), Box<dyn std::error::Error>> {
        let base = std::env::temp_dir().join(format!(
            "rill-mount-id-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&base)?;
        fs::write(base.join("file"), b"x")?;
        symlink("file", base.join("link"))?;
        let root = Root::prepare(&Located::new(
            base.clone(),
            Origin::file(&base.join("config")),
        ))?;
        let directory = root.open(Path::new(""))?;
        let mut dir = root.open_directory(root.open(Path::new(""))?)?;
        let mut found = 0;
        for entry in dir.iter() {
            let entry = entry?;
            let name = entry.file_name().to_str()?;
            if matches!(name, "file" | "link") {
                assert!(root.same_entry(
                    Path::new(name),
                    &directory,
                    Path::new(name),
                    entry.ino()
                )?);
                found += 1;
            }
        }
        assert_eq!(found, 2);
        fs::remove_dir_all(base)?;
        Ok(())
    }
}
