use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, OpenHow, RenameFlags, ResolveFlag, openat2, renameat2};
use nix::sys::stat::{Mode, SFlag, fstatat, mkdirat};
use nix::sys::statfs::{PROC_SUPER_MAGIC, fstatfs};
use nix::unistd::{UnlinkatFlags, geteuid, unlinkat};
use socket2::{Domain, SockAddr, Socket, Type};
use tokio::net::UnixListener;

static NEXT_TEMP_NAME: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
}

impl Identity {
    fn from_stat(stat: &nix::sys::stat::FileStat) -> Option<Self> {
        if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFSOCK {
            return None;
        }

        Some(Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
            uid: stat.st_uid,
        })
    }

    fn matches(self, stat: &nix::sys::stat::FileStat) -> bool {
        Self::from_stat(stat).is_some_and(|current| {
            current.dev == self.dev && current.ino == self.ino && current.uid == self.uid
        })
    }
}

pub struct ControlSocket {
    pub listener: UnixListener,
    parent: File,
    name: OsString,
    created_identity: Identity,
}

struct CreatedSocket {
    parent: File,
    name: OsString,
    identity: Identity,
    armed: bool,
}

impl Drop for CreatedSocket {
    fn drop(&mut self) {
        if self.armed {
            remove_if_owned(&self.parent, &self.name, self.identity);
        }
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        remove_if_owned(&self.parent, &self.name, self.created_identity);
    }
}

impl ControlSocket {
    pub fn bind(path: &Path) -> io::Result<Self> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("control socket requires a Tokio runtime"))?;

        let (parent_path, name) = split_socket_path(path)?;
        let uid = geteuid().as_raw();
        let parent = open_parent(&parent_path, uid)?;
        let parent_metadata = parent.metadata()?;

        let proc_fd = validate_proc(&parent, &parent_metadata, uid)?;
        let _ = SockAddr::unix(path)?;

        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket.set_nonblocking(true)?;

        let mut bound = None;
        for _ in 0..8 {
            let temp_name = OsString::from(format!(
                ".rill-{}-{}",
                std::process::id(),
                NEXT_TEMP_NAME.fetch_add(1, Ordering::Relaxed)
            ));
            let capability_path = proc_fd.join(&temp_name);
            let address = SockAddr::unix(&capability_path)?;
            match socket.bind(&address) {
                Ok(()) => {
                    bound = Some((temp_name, capability_path));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
                Err(error) => return Err(error),
            }
        }
        let (temp_name, capability_path) = bound.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                "control socket temporary names exhausted",
            )
        })?;

        let stat = stat_entry(&parent, &temp_name)?;
        let identity = Identity::from_stat(&stat)
            .filter(|identity| identity.uid == uid)
            .ok_or_else(|| io::Error::other("created control socket identity is invalid"))?;

        let mut created = CreatedSocket {
            parent,
            name: temp_name,
            identity,
            armed: true,
        };

        verify_entry(&created.parent, &created.name, identity, None)?;

        fs::set_permissions(&capability_path, fs::Permissions::from_mode(0o600))?;

        verify_entry(&created.parent, &created.name, identity, Some(0o600))?;

        renameat2(
            &created.parent,
            created.name.as_os_str(),
            &created.parent,
            name.as_os_str(),
            RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        created.name = name;
        verify_entry(&created.parent, &created.name, identity, Some(0o600))?;

        socket.listen(128)?;
        let listener: std::os::unix::net::UnixListener = socket.into();
        let listener = UnixListener::from_std(listener)?;

        let parent = created.parent.try_clone()?;
        let name = created.name.clone();
        created.armed = false;

        Ok(Self {
            listener,
            parent,
            name,
            created_identity: identity,
        })
    }
}

fn split_socket_path(path: &Path) -> io::Result<(PathBuf, OsString)> {
    if !path.is_absolute() {
        return Err(invalid("control socket path must be absolute"));
    }

    let name = path
        .file_name()
        .ok_or_else(|| invalid("control socket path needs a filename"))?;

    if name.as_bytes().is_empty()
        || name.as_bytes().contains(&0)
        || name == OsStr::new(".")
        || name == OsStr::new("..")
    {
        return Err(invalid("invalid control socket filename"));
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(invalid("control socket path contains traversal"));
        }
    }

    let parent = path
        .parent()
        .ok_or_else(|| invalid("control socket path needs a parent"))?;

    Ok((parent.to_path_buf(), name.to_os_string()))
}

fn open_parent(path: &Path, uid: u32) -> io::Result<File> {
    let mut current = File::open("/")?;
    validate_ancestor(&current, uid)?;

    let components: Vec<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();

    for (index, name) in components.iter().enumerate() {
        let last = index + 1 == components.len();

        let next = match open_directory(&current, name) {
            Ok(file) => file,
            Err(error) if last && error.raw_os_error() == Some(Errno::ENOENT as i32) => {
                match mkdirat(&current, name.as_os_str(), Mode::from_bits_truncate(0o700)) {
                    Ok(()) => {}
                    Err(Errno::EEXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                open_directory(&current, name)?
            }
            Err(error) => return Err(error),
        };

        if last {
            validate_parent(&next, uid)?;
        } else {
            validate_ancestor(&next, uid)?;
        }
        current = next;
    }

    validate_parent(&current, uid)?;
    Ok(current)
}

fn open_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);

    let fd = openat2(parent, name, how).map_err(io::Error::from)?;
    Ok(File::from(fd))
}

fn validate_ancestor(directory: &File, uid: u32) -> io::Result<()> {
    let metadata = directory.metadata()?;
    let mode = metadata.mode();

    if !metadata.is_dir() || (metadata.uid() != 0 && metadata.uid() != uid) {
        return Err(invalid("untrusted control socket ancestor"));
    }

    let writable_by_others = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    if writable_by_others && !sticky {
        return Err(invalid("writable control socket ancestor lacks sticky bit"));
    }

    Ok(())
}

fn validate_parent(directory: &File, uid: u32) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(invalid(
            "control socket parent must be owned by euid and mode 0700",
        ));
    }
    Ok(())
}

fn validate_proc(parent: &File, parent_metadata: &fs::Metadata, uid: u32) -> io::Result<PathBuf> {
    let proc_self = Path::new("/proc/self");
    let proc_self_metadata = fs::metadata(proc_self)?;
    if proc_self_metadata.uid() != uid {
        return Err(invalid("proc self owner does not match euid"));
    }

    let self_target = fs::read_link(proc_self)?;
    let expected_pid = std::process::id().to_string();
    if self_target.as_os_str() != OsStr::new(&expected_pid) {
        return Err(invalid("proc self does not identify this process"));
    }

    let proc_fd_dir = File::open("/proc/self/fd")?;
    if fstatfs(&proc_fd_dir)
        .map_err(io::Error::from)?
        .filesystem_type()
        != PROC_SUPER_MAGIC
    {
        return Err(invalid("proc fd directory is not procfs"));
    }

    let capability = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let actual = fs::metadata(&capability)?;
    if !actual.is_dir()
        || actual.dev() != parent_metadata.dev()
        || actual.ino() != parent_metadata.ino()
        || actual.uid() != parent_metadata.uid()
    {
        return Err(invalid("proc fd does not identify control socket parent"));
    }

    Ok(capability)
}

fn stat_entry(parent: &File, name: &OsStr) -> io::Result<nix::sys::stat::FileStat> {
    fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io::Error::from)
}

fn verify_entry(
    parent: &File,
    name: &OsStr,
    identity: Identity,
    expected_mode: Option<u32>,
) -> io::Result<()> {
    let stat = stat_entry(parent, name)?;
    if !identity.matches(&stat) || expected_mode.is_some_and(|mode| stat.st_mode & 0o7777 != mode) {
        return Err(invalid(
            "control socket entry changed or has unsafe permissions",
        ));
    }
    Ok(())
}

fn remove_if_owned(parent: &File, name: &OsStr, identity: Identity) {
    if stat_entry(parent, name).is_ok_and(|stat| identity.matches(&stat)) {
        let _ = unlinkat(parent, name, UnlinkatFlags::NoRemoveDir);
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    #[tokio::test]
    async fn binds_long_valid_unix_path() {
        let parent = loop {
            let candidate = std::env::temp_dir().join(format!(
                "c{:x}{:x}",
                std::process::id(),
                NEXT_TEMP_NAME.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test parent: {error}"),
            }
        };
        let name_len = 106 - parent.as_os_str().as_bytes().len();
        let path = parent.join("s".repeat(name_len));
        let socket = ControlSocket::bind(&path).expect("bind valid long address");
        assert!(path.exists());
        drop(socket);
        assert!(!path.exists());
        fs::remove_dir(parent).unwrap();
    }
}
