use super::{
    model::{DavError, Result},
    namespace,
    properties::random_name,
};
use crate::static_files::root::Root;
use nix::{
    fcntl::{OFlag, OpenHow, ResolveFlag, openat2},
    sys::stat::Mode,
};
use std::{fs::File, io, os::unix::fs::MetadataExt};

const KEY: &std::ffi::CStr = c"user.rill.webdav.private";
const VALUE: &[u8] = b"rill-dav-v1";
fn error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}
pub(crate) fn marked(file: &File) -> io::Result<bool> {
    let mut value = [0_u8; 32];
    match rustix::fs::fgetxattr(file, KEY, &mut value[..]) {
        Ok(_) | Err(rustix::io::Errno::RANGE) => Ok(true),
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => Ok(false),
        Err(failure) => Err(error(failure)),
    }
}
pub(crate) fn require(file: &File) -> Result<()> {
    let mut value = [0_u8; 32];
    match rustix::fs::fgetxattr(file, KEY, &mut value[..]) {
        Ok(length) if &value[..length] == VALUE => Ok(()),
        _ => Err(DavError::new(403, "DAV_PROPERTY_STORE")),
    }
}
pub(crate) struct NamedFile {
    file: File,
    parent: File,
    name: String,
    identity: (u64, u64),
    cleanup: bool,
    _memory: super::budget::Memory,
}
impl std::ops::Deref for NamedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl std::ops::DerefMut for NamedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}
impl NamedFile {
    pub fn keep(&mut self) {
        self.cleanup = false;
    }
    pub fn remove(&mut self) -> Result<()> {
        namespace::remove_owned(&self.parent, &self.name, self.identity, false)?;
        self.cleanup = false;
        Ok(())
    }
}
impl Drop for NamedFile {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.remove();
        }
    }
}
pub(crate) fn create(root: &Root, parent: &File, name: &str) -> Result<NamedFile> {
    let memory = super::budget::Memory::acquire(name.len() + 256)?;
    let cleanup_parent = parent.try_clone()?;
    let cleanup_name = name.to_owned();
    let file = File::from(openat2(
        parent,
        ".",
        OpenHow::new()
            .flags(OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_CLOEXEC)
            .mode(Mode::from_bits_truncate(0o600))
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )?);
    rustix::fs::fsetxattr(&file, KEY, VALUE, rustix::fs::XattrFlags::CREATE).map_err(error)?;
    file.sync_all()?;
    require(&file)?;
    let identity = namespace::identity(&file)?;
    let named = NamedFile {
        file,
        parent: cleanup_parent,
        name: cleanup_name,
        identity,
        cleanup: true,
        _memory: memory,
    };
    root.link_anonymous(&named, parent, name)?;
    Ok(named)
}
pub(crate) fn publish(file: &File) -> Result<()> {
    require(file)?;
    if file.metadata()?.nlink() != 1 {
        return Err(DavError::new(403, "DAV_ALIAS"));
    }
    clear_marker(file)
}
fn clear_marker(file: &File) -> Result<()> {
    rustix::fs::fremovexattr(file, KEY)
        .map_err(|_| DavError::new(500, "DAV_COMMIT_MARKER_UNKNOWN"))?;
    if file.sync_all().is_err() {
        let _ = rustix::fs::fsetxattr(file, KEY, VALUE, rustix::fs::XattrFlags::empty());
        let _ = file.sync_all();
        return Err(DavError::new(500, "DAV_COMMIT_MARKER_UNKNOWN"));
    }
    Ok(())
}
pub(crate) fn probe(root: &Root) -> Result<()> {
    let parent = root.reopen_checked(root.capability().try_clone()?, true)?;
    let name = format!(".rill-dav-put-{}", random_name()?);
    let mut file =
        create(root, &parent, &name).map_err(|_| DavError::new(503, "DAV_CAPABILITY"))?;
    file.remove()?;
    parent.sync_all()?;
    clear_marker(&file).map_err(|_| DavError::new(503, "DAV_CAPABILITY"))
}
