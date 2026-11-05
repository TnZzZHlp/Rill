use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::sys::statfs::{PROC_SUPER_MAGIC, fstatfs};
use openssl::ssl::{SslContext, SslContextBuilder, SslFiletype, SslMethod};
use openssl::x509::store::X509Lookup;
use std::hash::{Hash, Hasher};

use crate::config::source::Origin;
use crate::config::{ConfigError, Result};
use crate::http_policy::compile::{ExternalBudget, PreparedPolicy};

#[derive(Debug, Eq, Hash, PartialEq)]
pub(crate) struct DirectoryMaterial {
    pub entries: Vec<(OsString, Vec<u8>)>,
}

#[derive(Debug)]
pub(crate) struct TrustMaterial {
    pub bundle: Vec<u8>,
    pub directories: Vec<DirectoryMaterial>,
    _budget: ExternalBudget,
}

pub(super) fn failure(origin: &Origin) -> ConfigError {
    ConfigError::new(
        "RESOURCE_IO",
        "upstream-trust",
        "cannot freeze system trust material",
        origin,
    )
}

fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn read_file(
    path: &Path,
    policy: &PreparedPolicy,
    budget: &mut ExternalBudget,
    origin: &Origin,
) -> Result<(Vec<u8>, Metadata, Metadata)> {
    let link = fs::symlink_metadata(path).map_err(|_| failure(origin))?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| failure(origin))?;
    let before = file.metadata().map_err(|_| failure(origin))?;
    if !before.is_file() || before.len() > 1024 * 1024 {
        return Err(failure(origin));
    }
    let len = usize::try_from(before.len()).map_err(|_| failure(origin))?;
    budget.reserve(policy, len, origin)?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).map_err(|_| failure(origin))?;
    let mut extra = [0];
    if file.read(&mut extra).map_err(|_| failure(origin))? != 0 {
        return Err(failure(origin));
    }
    let after = file.metadata().map_err(|_| failure(origin))?;
    let link_after = fs::symlink_metadata(path).map_err(|_| failure(origin))?;
    let resolved_after = fs::metadata(path).map_err(|_| failure(origin))?;
    if bytes.len() != len
        || !same(&before, &after)
        || !same(&before, &resolved_after)
        || !same(&link, &link_after)
    {
        return Err(failure(origin));
    }
    Ok((bytes, link, before))
}

fn unchanged(path: &Path, link: &Metadata, target: &Metadata, origin: &Origin) -> Result<()> {
    let current_link = fs::symlink_metadata(path).map_err(|_| failure(origin))?;
    let current_target = fs::metadata(path).map_err(|_| failure(origin))?;
    if !same(link, &current_link) || !same(target, &current_target) {
        return Err(failure(origin));
    }
    Ok(())
}

fn entries(
    path: &Path,
    policy: &PreparedPolicy,
    budget: &mut ExternalBudget,
    origin: &Origin,
) -> Result<(Vec<OsString>, usize)> {
    let mut names = Vec::new();
    let mut charged = 0_usize;
    for entry in fs::read_dir(path).map_err(|_| failure(origin))? {
        if names.len() == 4096 {
            return Err(failure(origin));
        }
        let name = entry.map_err(|_| failure(origin))?.file_name();
        let bytes = name
            .as_bytes()
            .len()
            .checked_add(2 * std::mem::size_of::<OsString>())
            .ok_or_else(|| failure(origin))?;
        budget.reserve(policy, bytes, origin)?;
        charged = charged.checked_add(bytes).ok_or_else(|| failure(origin))?;
        names.push(name);
    }
    names.sort();
    Ok((names, charged))
}

fn hashed(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 10
        || !bytes[..8]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        || bytes[8] != b'.'
    {
        return false;
    }
    let suffix = &bytes[9..];
    let digits = suffix.strip_prefix(b"r").unwrap_or(suffix);
    !digits.is_empty()
        && (digits.len() == 1 || digits[0] != b'0')
        && digits.iter().all(u8::is_ascii_digit)
}

impl TrustMaterial {
    pub(crate) fn prepare(policy: &PreparedPolicy, origin: &Origin) -> Result<Self> {
        let paths =
            super::super::upstream_tls::system_trust_paths().ok_or_else(|| failure(origin))?;
        let mut budget = ExternalBudget::default();
        let (bundle, bundle_link, bundle_target) =
            read_file(&paths.file, policy, &mut budget, origin)?;
        let mut directories = Vec::new();
        for dir in paths
            .directories
            .as_bytes()
            .split(|byte| *byte == b':')
            .filter(|s| !s.is_empty())
        {
            budget.reserve(
                policy,
                dir.len()
                    .saturating_add(2 * std::mem::size_of::<DirectoryMaterial>())
                    .saturating_add(2 * std::mem::size_of::<PathBuf>()),
                origin,
            )?;
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(dir));
            let before = fs::metadata(&path).map_err(|_| failure(origin))?;
            if !before.is_dir() {
                return Err(failure(origin));
            }
            let (names, names_charge) = entries(&path, policy, &mut budget, origin)?;
            let mut files = Vec::new();
            let mut checks = Vec::new();
            let mut checks_charge = 0_usize;
            for name in &names {
                if hashed(name) {
                    let charge = path
                        .as_os_str()
                        .as_bytes()
                        .len()
                        .checked_add(name.as_bytes().len() * 2)
                        .and_then(|n| {
                            n.checked_add(2 * std::mem::size_of::<(PathBuf, Metadata, Metadata)>())
                        })
                        .ok_or_else(|| failure(origin))?;
                    budget.reserve(policy, charge, origin)?;
                    checks_charge = checks_charge
                        .checked_add(charge)
                        .ok_or_else(|| failure(origin))?;
                    budget.reserve(
                        policy,
                        name.as_bytes().len() + 2 * std::mem::size_of::<(OsString, Vec<u8>)>(),
                        origin,
                    )?;
                    let file_path = path.join(name);
                    let (bytes, link, target) = read_file(&file_path, policy, &mut budget, origin)?;
                    checks.push((file_path, link, target));
                    files.push((name.clone(), bytes));
                }
            }
            let after = fs::metadata(&path).map_err(|_| failure(origin))?;
            let (after_names, after_charge) = entries(&path, policy, &mut budget, origin)?;
            if !same(&before, &after) || names != after_names {
                return Err(failure(origin));
            }
            for (file, link, target) in &checks {
                unchanged(file, link, target, origin)?;
            }
            drop(checks);
            drop(after_names);
            drop(names);
            budget.release(names_charge + after_charge + checks_charge);
            directories.push(DirectoryMaterial { entries: files });
        }
        unchanged(&paths.file, &bundle_link, &bundle_target, origin)?;
        let mut result = Self {
            bundle,
            directories,
            _budget: budget,
        };
        drop(result.context(policy, origin, &[], false)?);
        result._budget.release(result.bundle.len());
        let directories = std::mem::take(&mut result.directories);
        for directory in &directories {
            for (name, bytes) in &directory.entries {
                let is_crl = name.as_bytes().get(9) == Some(&b'r');
                result._budget.reserve(policy, bytes.len(), origin)?;
                let original = std::mem::replace(&mut result.bundle, bytes.clone());
                drop(result.context_inner(policy, origin, &[], false, None)?);
                result._budget.release(result.bundle.len());
                drop(result.context_inner(policy, origin, &[], false, Some(is_crl))?);
                result._budget.release(result.bundle.len() * 2);
                result.bundle = original;
            }
        }
        result.directories = directories;
        Ok(result)
    }

    pub(super) fn reserve_private_copy(
        &self,
        policy: &PreparedPolicy,
        origin: &Origin,
    ) -> Result<ExternalBudget> {
        let per_dir = 4 * std::mem::size_of::<(File, OsString, Vec<OsString>)>() + 512;
        let mut bytes = self
            .bundle
            .len()
            .checked_add(1024)
            .ok_or_else(|| failure(origin))?;
        bytes = bytes
            .checked_add(
                self.directories
                    .len()
                    .checked_mul(per_dir)
                    .ok_or_else(|| failure(origin))?,
            )
            .ok_or_else(|| failure(origin))?;
        for (name, contents) in self.directories.iter().flat_map(|dir| &dir.entries) {
            bytes = bytes
                .checked_add(contents.len())
                .and_then(|n| n.checked_add(name.as_bytes().len()))
                .and_then(|n| n.checked_add(2 * std::mem::size_of::<OsString>()))
                .ok_or_else(|| failure(origin))?;
        }
        let mut copy = ExternalBudget::default();
        copy.reserve_with_existing(policy, self._budget.bytes(), bytes, origin)?;
        Ok(copy)
    }

    pub(super) fn context(
        &mut self,
        policy: &PreparedPolicy,
        origin: &Origin,
        dirs: &[PathBuf],
        keep: bool,
    ) -> Result<SslContext> {
        self.context_inner(policy, origin, dirs, keep, None)
    }

    fn context_inner(
        &mut self,
        policy: &PreparedPolicy,
        origin: &Origin,
        dirs: &[PathBuf],
        keep: bool,
        hash_crl: Option<bool>,
    ) -> Result<SslContext> {
        if !keep {
            self._budget.reserve(policy, self.bundle.len(), origin)?;
            self._budget.reserve(policy, self.bundle.len(), origin)?;
        }
        let fd = memfd_create(
            c"rill-upstream-ca",
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )
        .map_err(|_| failure(origin))?;
        let mut file = File::from(fd);
        file.write_all(&self.bundle).map_err(|_| failure(origin))?;
        fcntl(
            &file,
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_WRITE
                    | SealFlag::F_SEAL_GROW
                    | SealFlag::F_SEAL_SHRINK
                    | SealFlag::F_SEAL_SEAL,
            ),
        )
        .map_err(|_| failure(origin))?;
        let pid = std::process::id().to_string();
        if fs::read_link("/proc/self").map_err(|_| failure(origin))? != Path::new(&pid) {
            return Err(failure(origin));
        }
        let proc_dir = File::open(format!("/proc/{pid}/fd")).map_err(|_| failure(origin))?;
        if fstatfs(&proc_dir)
            .map_err(|_| failure(origin))?
            .filesystem_type()
            != PROC_SUPER_MAGIC
        {
            return Err(failure(origin));
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let reopened = fs::metadata(&path).map_err(|_| failure(origin))?;
        if !same(&file.metadata().map_err(|_| failure(origin))?, &reopened) {
            return Err(failure(origin));
        }
        let mut context = SslContextBuilder::new(SslMethod::tls()).map_err(|_| failure(origin))?;
        if let Some(is_crl) = hash_crl {
            let lookup = context
                .cert_store_mut()
                .add_lookup(X509Lookup::file())
                .map_err(|_| failure(origin))?;
            if is_crl {
                lookup
                    .load_crl_file(&path, SslFiletype::PEM)
                    .map_err(|_| failure(origin))?;
            } else {
                lookup
                    .load_cert_file(&path, SslFiletype::PEM)
                    .map_err(|_| failure(origin))?;
            }
        } else {
            context
                .load_verify_locations(Some(&path), None)
                .map_err(|_| failure(origin))?;
        }
        if !dirs.is_empty() {
            let lookup = context
                .cert_store_mut()
                .add_lookup(X509Lookup::hash_dir())
                .map_err(|_| failure(origin))?;
            for dir in dirs {
                let path = dir
                    .to_str()
                    .filter(|path| !path.contains(':'))
                    .ok_or_else(|| failure(origin))?;
                lookup
                    .add_dir(path, SslFiletype::PEM)
                    .map_err(|_| failure(origin))?;
            }
        }
        let context = context.build();
        drop(file);
        if !keep {
            self._budget.release(self.bundle.len());
        }
        Ok(context)
    }
}

impl PartialEq for TrustMaterial {
    fn eq(&self, other: &Self) -> bool {
        self.bundle == other.bundle && self.directories == other.directories
    }
}
impl Eq for TrustMaterial {}
impl Hash for TrustMaterial {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bundle.hash(state);
        self.directories.hash(state);
    }
}
