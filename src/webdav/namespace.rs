use super::{
    budget::Memory,
    model::{DavError, Result},
    path::DavPath,
    registry::StorageHandle,
    visibility,
};
use crate::static_files::root::Root;
use nix::{
    fcntl::{OFlag, OpenHow, ResolveFlag, openat2, renameat},
    sys::stat::Mode,
};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
    mount: u64,
    kind: u32,
}
impl FileIdentity {
    fn read(root: &Root, file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mount: root.mount_id(file)?,
            kind: metadata.mode() & 0o170000,
        })
    }
}
struct Directory {
    file: File,
    reader: Option<File>,
    cookie: Option<u64>,
    complete: bool,
    entries: BTreeMap<Vec<u8>, FileIdentity>,
}
struct Proof {
    directories: BTreeMap<FileIdentity, Directory>,
    remaining: usize,
    memory: Memory,
}
pub(crate) struct NamespaceSession {
    storage: Arc<StorageHandle>,
    proof: Mutex<Proof>,
    _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
}
impl NamespaceSession {
    pub fn new(
        storage: Arc<StorageHandle>,
        guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    ) -> Result<Self> {
        drop(visibility::read());
        Ok(Self {
            storage,
            proof: Mutex::new(Proof {
                directories: BTreeMap::new(),
                remaining: 4095,
                memory: Memory::acquire(1024)?,
            }),
            _guard: guard,
        })
    }
    pub fn root(&self) -> &Root {
        &self.storage.root
    }
    pub fn handle(&self) -> &StorageHandle {
        &self.storage
    }
    pub fn mount_id(&self, file: &File) -> std::io::Result<u64> {
        self.root().mount_id(file)
    }
    fn directory(&self, proof: &mut Proof, file: &File) -> Result<FileIdentity> {
        let key = FileIdentity::read(self.root(), file)?;
        if key.kind != 0o040000 {
            return Err(DavError::new(409, "DAV_PARENT"));
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = proof.directories.entry(key) {
            proof.memory.reserve(1024)?;
            let parent = file.try_clone()?;
            let read = self.root().reopen_checked(file.try_clone()?, true)?;
            entry.insert(Directory {
                file: parent,
                reader: Some(read),
                cookie: None,
                complete: false,
                entries: BTreeMap::new(),
            });
        }
        Ok(key)
    }
    fn scan(&self, proof: &mut Proof, key: FileIdentity, wanted: Option<&[u8]>) -> Result<()> {
        let directory = proof
            .directories
            .get_mut(&key)
            .ok_or_else(DavError::invalid)?;
        if wanted.is_some_and(|name| directory.entries.contains_key(name)) {
            return Ok(());
        }
        while !directory.complete {
            let reader = directory.reader.as_ref().ok_or_else(DavError::invalid)?;
            if let Some(cookie) = directory.cookie {
                rustix::fs::seek(reader, rustix::fs::SeekFrom::Start(cookie))
                    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
            }
            let mut buffer = [std::mem::MaybeUninit::<u8>::uninit(); 8192];
            let mut iterator = rustix::fs::RawDir::new(reader, &mut buffer);
            let mut found = false;
            loop {
                let Some(entry) = iterator.next() else {
                    directory.complete = true;
                    break;
                };
                let entry = entry
                    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
                directory.cookie = Some(entry.next_entry_cookie());
                let bytes = entry.file_name().to_bytes();
                if matches!(bytes, b"." | b"..") {
                    continue;
                }
                proof.remaining = proof.remaining.checked_sub(1).ok_or_else(DavError::limit)?;
                proof.memory.reserve(bytes.len() + 512)?;
                let file = constrained(
                    &directory.file,
                    Path::new(std::ffi::OsStr::from_bytes(bytes)),
                )?;
                let identity = FileIdentity::read(self.root(), &file)?;
                directory.entries.insert(bytes.to_vec(), identity);
                if wanted == Some(bytes) {
                    found = true;
                    break;
                }
            }
            if directory.complete {
                directory.reader = None;
            }
            if found {
                break;
            }
        }
        Ok(())
    }
    fn open(&self, path: &DavPath) -> Result<File> {
        if path
            .relative()
            .components()
            .any(|part| visibility::reserved(part.as_os_str()))
        {
            return Err(DavError::new(403, "DAV_PRIVATE"));
        }
        if path
            .key()
            .split('/')
            .filter(|part| !part.is_empty())
            .count()
            > 16
        {
            return Err(DavError::limit());
        }
        let mut current = self.root().capability().try_clone()?;
        let mut proof = self
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for part in path.key().split('/').filter(|part| !part.is_empty()) {
            let parent = self.directory(&mut proof, &current)?;
            self.scan(&mut proof, parent, Some(part.as_bytes()))?;
            let expected = proof
                .directories
                .get(&parent)
                .and_then(|directory| directory.entries.get(part.as_bytes()))
                .copied();
            let file = constrained(&current, Path::new(part))?;
            let actual = FileIdentity::read(self.root(), &file)?;
            if expected != Some(actual) {
                return Err(DavError::new(403, "DAV_ALIAS"));
            }
            checked_type(self.root(), &file)?;
            current = file;
        }
        if path.is_root() {
            checked_type(self.root(), &current)?;
        }
        Ok(current)
    }
    pub fn validate_tags(&self) -> Result<()> {
        let proof = self
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for directory in proof.directories.values() {
            for (name, expected) in &directory.entries {
                if visibility::reserved(std::ffi::OsStr::from_bytes(name))
                    || !matches!(expected.kind, 0o040000 | 0o100000)
                {
                    continue;
                }
                let file = constrained(
                    &directory.file,
                    Path::new(std::ffi::OsStr::from_bytes(name)),
                )?;
                if FileIdentity::read(self.root(), &file)? != *expected {
                    return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
                }
                checked_type(self.root(), &file)?;
            }
        }
        Ok(())
    }
    fn names(&self, file: &File) -> Result<Vec<Vec<u8>>> {
        let mut proof = self
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = self.directory(&mut proof, file)?;
        self.scan(&mut proof, key, None)?;
        let charge = proof.directories[&key]
            .entries
            .keys()
            .try_fold(0_usize, |total, name| {
                total.checked_add(name.len() + 2 * std::mem::size_of::<Vec<u8>>())
            })
            .ok_or_else(DavError::limit)?;
        proof.memory.reserve(charge)?;
        let mut names = Vec::new();
        names
            .try_reserve_exact(proof.directories[&key].entries.len())
            .map_err(|_| DavError::limit())?;
        names.extend(proof.directories[&key].entries.keys().cloned());
        Ok(names)
    }
    fn reserve_create(&self, bytes: usize) -> Result<()> {
        let mut proof = self
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = proof.remaining.checked_sub(1).ok_or_else(DavError::limit)?;
        proof.memory.reserve(bytes)?;
        proof.remaining = remaining;
        Ok(())
    }
    fn created(&self, parent: &File, name: &str, file: File, directory: bool) -> Result<()> {
        let parent_key = FileIdentity::read(self.root(), parent)?;
        let key = FileIdentity::read(self.root(), &file)?;
        let mut proof = self
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = proof.directories.get_mut(&parent_key) {
            parent.entries.insert(name.as_bytes().to_vec(), key);
        }
        if directory {
            proof.directories.insert(
                key,
                Directory {
                    file,
                    reader: None,
                    cookie: None,
                    complete: true,
                    entries: BTreeMap::new(),
                },
            );
        }
        Ok(())
    }
}
fn constrained(parent: &File, name: &Path) -> Result<File> {
    Ok(File::from(openat2(
        parent,
        name,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_XDEV,
            ),
    )?))
}
fn checked_type(root: &Root, file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if (!metadata.is_dir() && !metadata.is_file()) || (metadata.is_file() && metadata.nlink() != 1)
    {
        return Err(DavError::new(403, "DAV_ALIAS"));
    }
    if visibility::denied(file, &visibility::read())? {
        return Err(DavError::new(403, "DAV_PRIVATE"));
    }
    if metadata.is_dir() {
        if root.private_directory(file)? {
            return Err(DavError::new(403, "DAV_PRIVATE"));
        }
    } else {
        let read = root.reopen_checked(file.try_clone()?, false)?;
        if super::private_file::marked(&read)? {
            return Err(DavError::new(403, "DAV_PRIVATE"));
        }
    }
    Ok(())
}
pub fn open(namespace: &NamespaceSession, path: &DavPath) -> Result<File> {
    namespace.open(path)
}
pub fn metadata(namespace: &NamespaceSession, path: &DavPath) -> Result<Option<Metadata>> {
    match open(namespace, path) {
        Ok(file) => Ok(Some(file.metadata()?)),
        Err(error) if error.status == 404 => Ok(None),
        Err(error) => Err(error),
    }
}
pub fn parent(namespace: &NamespaceSession, path: &DavPath) -> Result<File> {
    if path.is_root() {
        return Err(DavError::new(403, "DAV_ROOT"));
    }
    Ok(namespace
        .root()
        .reopen_checked(open(namespace, &path.parent())?, true)?)
}
pub fn revalidate(namespace: &NamespaceSession, path: &DavPath, parent: &File) -> Result<()> {
    if FileIdentity::read(namespace.root(), &open(namespace, &path.parent())?)?
        != FileIdentity::read(namespace.root(), parent)?
    {
        return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
    }
    Ok(())
}
pub fn readable_or_directory(namespace: &NamespaceSession, path: &DavPath) -> Result<()> {
    let file = open(namespace, path)?;
    if file.metadata()?.is_file() {
        namespace.root().reopen_checked(file, false)?;
    }
    Ok(())
}
pub fn readable(namespace: &NamespaceSession, path: &DavPath) -> Result<File> {
    Ok(namespace
        .root()
        .reopen_checked(open(namespace, path)?, false)?)
}
pub fn writable_metadata(namespace: &NamespaceSession, path: &DavPath) -> Result<()> {
    let file = open(namespace, path)?;
    if file.metadata()?.is_dir() {
        check_writable_directory(&file)
    } else {
        writable(namespace, path)
    }
}
pub fn check_writable_directory(directory: &File) -> Result<()> {
    nix::unistd::faccessat(
        directory,
        ".",
        nix::unistd::AccessFlags::R_OK
            | nix::unistd::AccessFlags::W_OK
            | nix::unistd::AccessFlags::X_OK,
        nix::fcntl::AtFlags::AT_EACCESS,
    )?;
    Ok(())
}
pub fn writable(namespace: &NamespaceSession, path: &DavPath) -> Result<()> {
    let before = open(namespace, path)?;
    if !before.metadata()?.is_file() {
        return Err(DavError::new(405, "DAV_COLLECTION"));
    }
    let file = File::from(openat2(
        namespace.root().capability(),
        path.relative(),
        OpenHow::new()
            .flags(OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_XDEV,
            ),
    )?);
    if FileIdentity::read(namespace.root(), &before)?
        != FileIdentity::read(namespace.root(), &file)?
    {
        return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
    }
    Ok(())
}
pub fn etag(metadata: &Metadata) -> String {
    crate::static_files::validator::weak_etag(
        metadata,
        crate::static_files::representation::Encoding::Identity,
    )
}
pub fn children(
    namespace: &NamespaceSession,
    path: &DavPath,
    memory: &mut Memory,
) -> Result<Vec<DavPath>> {
    let directory = open(namespace, path)?;
    let mut children = Vec::new();
    for bytes in namespace.names(&directory)? {
        if visibility::reserved(std::ffi::OsStr::from_bytes(&bytes)) {
            continue;
        }
        let name = std::str::from_utf8(&bytes).map_err(|_| DavError::new(403, "DAV_NAME"))?;
        memory.reserve(
            3 * (path.key().len() + name.len() + 1)
                + 4 * std::mem::size_of::<DavPath>()
                + 2 * std::mem::size_of::<usize>(),
        )?;
        let child = path.child(name)?;
        open(namespace, &child)?;
        children
            .try_reserve_exact(1)
            .map_err(|_| DavError::limit())?;
        children.push(child);
    }
    Ok(children)
}
pub struct Traversal {
    paths: Vec<DavPath>,
    _memory: Memory,
}
impl std::ops::Deref for Traversal {
    type Target = Vec<DavPath>;
    fn deref(&self) -> &Self::Target {
        &self.paths
    }
}
impl<'a> IntoIterator for &'a Traversal {
    type Item = &'a DavPath;
    type IntoIter = std::slice::Iter<'a, DavPath>;
    fn into_iter(self) -> Self::IntoIter {
        self.paths.iter()
    }
}
pub fn walk(namespace: &NamespaceSession, path: &DavPath, depth: usize) -> Result<Traversal> {
    let mut memory = Memory::acquire(3 * path.key().len() + 4 * std::mem::size_of::<DavPath>())?;
    let mut queue = vec![(path.clone(), 0_usize)];
    let mut paths = Vec::new();
    while let Some((path, level)) = queue.pop() {
        let file = open(namespace, &path)?;
        if file.metadata()?.is_dir() && level < depth {
            let children = children(namespace, &path, &mut memory)?;
            if level >= 16 && !children.is_empty() {
                return Err(DavError::limit());
            }
            for child in children {
                queue.try_reserve_exact(1).map_err(|_| DavError::limit())?;
                queue.push((child, level + 1));
            }
        }
        paths.try_reserve_exact(1).map_err(|_| DavError::limit())?;
        paths.push(path);
    }
    Ok(Traversal {
        paths,
        _memory: memory,
    })
}
pub fn mkdir(namespace: &NamespaceSession, parent: &File, name: &str) -> Result<()> {
    namespace.reserve_create(name.len() + 2048)?;
    nix::sys::stat::mkdirat(parent, name, Mode::from_bits_truncate(0o755))?;
    namespace.created(parent, name, constrained(parent, Path::new(name))?, true)
}
pub fn remove_entry(
    namespace: &NamespaceSession,
    parent: &File,
    name: &str,
    directory: bool,
) -> Result<()> {
    let key = FileIdentity::read(namespace.root(), parent)?;
    remove(parent, name, directory)?;
    if let Some(directory) = namespace
        .proof
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .directories
        .get_mut(&key)
    {
        directory.entries.remove(name.as_bytes());
    }
    Ok(())
}
pub fn reserve_create(namespace: &NamespaceSession, name: &str) -> Result<()> {
    namespace.reserve_create(name.len() + 512)
}
pub fn created_file(
    namespace: &NamespaceSession,
    parent: &File,
    name: &str,
    file: &File,
) -> Result<()> {
    namespace.created(parent, name, file.try_clone()?, false)
}
pub struct Rename<'a> {
    namespace: &'a NamespaceSession,
    source: &'a File,
    source_name: &'a str,
    target: &'a File,
    target_name: Vec<u8>,
    source_key: FileIdentity,
    target_key: FileIdentity,
    entry: FileIdentity,
}
pub fn prepare_rename<'a>(
    namespace: &'a NamespaceSession,
    source: &'a File,
    source_name: &'a str,
    target: &'a File,
    target_name: &str,
    file: &File,
) -> Result<Rename<'a>> {
    namespace.reserve_create(target_name.len() + 512)?;
    Ok(Rename {
        namespace,
        source,
        source_name,
        target,
        target_name: target_name.as_bytes().to_vec(),
        source_key: FileIdentity::read(namespace.root(), source)?,
        target_key: FileIdentity::read(namespace.root(), target)?,
        entry: FileIdentity::read(namespace.root(), file)?,
    })
}
impl Rename<'_> {
    pub fn commit(self) -> Result<()> {
        renameat(
            self.source,
            self.source_name,
            self.target,
            std::ffi::OsStr::from_bytes(&self.target_name),
        )?;
        let mut proof = self
            .namespace
            .proof
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(directory) = proof.directories.get_mut(&self.source_key) {
            directory.entries.remove(self.source_name.as_bytes());
        }
        if let Some(directory) = proof.directories.get_mut(&self.target_key) {
            directory.entries.insert(self.target_name, self.entry);
        }
        Ok(())
    }
}
pub fn remove(parent: &File, name: &str, directory: bool) -> Result<()> {
    nix::unistd::unlinkat(
        parent,
        name,
        if directory {
            nix::unistd::UnlinkatFlags::RemoveDir
        } else {
            nix::unistd::UnlinkatFlags::NoRemoveDir
        },
    )?;
    Ok(())
}
pub fn remove_owned(
    parent: &File,
    name: &str,
    expected: (u64, u64),
    directory: bool,
) -> Result<()> {
    let current = nix::sys::stat::fstatat(parent, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if (current.st_dev, current.st_ino) != expected
        || (current.st_mode & 0o170000 == 0o040000) != directory
    {
        return Err(DavError::new(409, "DAV_NAMESPACE_CHANGED"));
    }
    remove(parent, name, directory)
}
pub fn identity(file: &File) -> Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
pub fn overlaps(left: &Root, right: &Root) -> Result<bool> {
    if identity(left.capability())?.0 == identity(right.capability())?.0
        && left.mount_id(left.capability())? != right.mount_id(right.capability())?
    {
        return Ok(true);
    }
    Ok(ancestor(left.capability(), right.capability())?
        || ancestor(right.capability(), left.capability())?)
}
pub fn ancestor(ancestor: &File, child: &File) -> Result<bool> {
    let wanted = identity(ancestor)?;
    let mut current = child.try_clone()?;
    for _ in 0..4096 {
        let here = identity(&current)?;
        if here == wanted {
            return Ok(true);
        }
        let parent = File::from(nix::fcntl::openat(
            &current,
            Path::new(".."),
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?);
        if identity(&parent)? == here {
            return Ok(false);
        }
        current = parent;
    }
    Err(DavError::new(403, "DAV_ROOT_OVERLAP"))
}
