use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
        },
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use nix::{
    dir::Dir,
    errno::Errno,
    fcntl::{AtFlags, OFlag, RenameFlags, openat, renameat2},
    sys::{
        stat::{Mode, SFlag, fstatat},
        statfs::{PROC_SUPER_MAGIC, fstatfs},
    },
    unistd::{UnlinkatFlags, linkat, unlinkat},
};

use super::{
    file_config::LogConfig,
    log_budget::LogBudget,
    log_directory::{BudgetFile, LogDirectory},
};

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 4096;

fn limit() -> io::Error {
    io::Error::other("LOG_LIMIT")
}
fn capacity() -> io::Error {
    io::Error::other("LOG_CAPACITY")
}
fn stat(parent: &File, name: &OsStr) -> io::Result<Option<nix::sys::stat::FileStat>> {
    match fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::ENOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn target_definitely_absent(directory: &LogDirectory) -> bool {
    matches!(stat(&directory.parent.file, &directory.name), Ok(None))
}
fn validate_file(stat: &nix::sys::stat::FileStat) -> io::Result<u64> {
    let length = u64::try_from(stat.st_size).map_err(|_| limit())?;
    if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFREG
        || stat.st_nlink != 1
        || length > MAX_FILE_SIZE
    {
        return Err(limit());
    }
    Ok(length)
}
fn validate_existing(directory: &LogDirectory, config: &LogConfig) -> io::Result<()> {
    let parent = &directory.parent.file;
    let _credit = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
    let mut scan = Dir::openat(
        parent,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut prefix = directory.name.as_bytes().to_vec();
    prefix.push(b'.');
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .map_err(|_| limit())?;
    let mut count = 0;
    let mut size = 0u64;
    for (index, entry) in scan.iter().enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(limit());
        }
        let entry = entry.map_err(io::Error::from)?;
        let raw = entry.file_name().to_bytes();
        let Some(suffix) = raw.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let number = std::str::from_utf8(suffix)
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        if number == Some(0) {
            continue;
        }
        if number.is_none_or(|n| n > 5) || suffix[0] == b'0' {
            return Err(limit());
        }
        let name = OsStr::from_bytes(raw);
        let entry_stat = stat(parent, name)?.ok_or_else(limit)?;
        size = size
            .checked_add(validate_file(&entry_stat)?)
            .ok_or_else(limit)?;
        if now.saturating_sub(entry_stat.st_mtime) > i64::from(config.retain_days) * 86_400 {
            return Err(limit());
        }
        count += 1;
    }
    if count > usize::from(config.retain_files) {
        return Err(limit());
    }
    if let Some(active) = stat(parent, &directory.name)? {
        size = size
            .checked_add(validate_file(&active)?)
            .ok_or_else(limit)?;
    }
    if size > 6 * MAX_FILE_SIZE {
        return Err(limit());
    }
    Ok(())
}

#[derive(Debug)]
pub struct PreparedLogFile {
    directory: LogDirectory,
    config: LogConfig,
}
impl PreparedLogFile {
    pub fn prepare(config: &LogConfig) -> io::Result<Self> {
        let directory = LogDirectory::prepare(&config.file.value)?;
        validate_existing(&directory, config)?;
        rustix::fs::accessat(
            &directory.parent.file,
            ".",
            rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        if stat(&directory.parent.file, &directory.name)?.is_some() {
            rustix::fs::accessat(
                &directory.parent.file,
                directory.name.as_os_str(),
                rustix::fs::Access::WRITE_OK,
                rustix::fs::AtFlags::EACCESS | rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        }
        Ok(Self {
            directory,
            config: config.clone(),
        })
    }
    pub fn directory(&self) -> &LogDirectory {
        &self.directory
    }
    pub(crate) fn rollback_monitor(&self) -> io::Result<LogDirectory> {
        let credit = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
        let fd = openat(
            &self.directory.parent.file,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        Ok(LogDirectory {
            parent: BudgetFile::new(File::from(fd), credit),
            name: self.directory.name.clone(),
        })
    }
    pub fn revalidate(&self) -> io::Result<()> {
        validate_existing(&self.directory, &self.config)
    }

    #[cfg(test)]
    fn activate(self) -> io::Result<ActivatedLogFile> {
        self.activate_inner(true, &mut false, Arc::new(AtomicBool::new(false)))
    }
    pub(crate) fn activate_tracked(
        self,
        partial_dirty: Arc<AtomicBool>,
    ) -> Result<ActivatedLogFile, (io::Error, bool)> {
        let mut published = false;
        self.activate_inner(true, &mut published, partial_dirty)
            .map_err(|error| (error, published))
    }
    fn activate_inner(
        self,
        revalidate: bool,
        published: &mut bool,
        partial_dirty: Arc<AtomicBool>,
    ) -> io::Result<ActivatedLogFile> {
        if partial_dirty.load(Ordering::Acquire) {
            return Err(limit());
        }
        if revalidate {
            validate_existing(&self.directory, &self.config)?;
        }
        let parent = &self.directory.parent.file;
        let name = self.directory.name.as_os_str();
        let credit = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
        let flags = OFlag::O_WRONLY
            | OFlag::O_APPEND
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_NONBLOCK;
        let existing = stat(parent, name)?.is_some();
        if !revalidate && existing {
            return Err(io::Error::from_raw_os_error(Errno::EEXIST as i32));
        }
        let fd = if existing {
            openat(parent, name, flags, Mode::empty()).map_err(io::Error::from)?
        } else {
            openat(
                parent,
                ".",
                OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_APPEND | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(io::Error::from)?
        };
        let file = BudgetFile::new(File::from(fd), credit);
        let metadata = file.file.metadata()?;
        if !metadata.is_file()
            || (existing && metadata.nlink() != 1)
            || (!existing && metadata.nlink() != 0)
        {
            return Err(limit());
        }
        let identity = (metadata.dev(), metadata.ino());
        let result = ActivatedLogFile {
            directory: self.directory,
            file,
            identity,
            created: !existing,
            committed: false,
            rotation_pending: false,
            partial_write: None,
            partial_dirty,
            config: self.config,
        };
        if !existing {
            let proc_credit = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
            let pid = std::process::id().to_string();
            if std::fs::read_link("/proc/self")? != std::path::Path::new(&pid) {
                return Err(io::Error::other("untrusted procfs"));
            }
            let proc = BudgetFile::new(File::open(format!("/proc/{pid}/fd"))?, proc_credit);
            if !proc.file.metadata()?.is_dir()
                || fstatfs(&proc.file)
                    .map_err(io::Error::from)?
                    .filesystem_type()
                    != PROC_SUPER_MAGIC
            {
                return Err(io::Error::other("untrusted procfs"));
            }
            let number = result.file.file.as_raw_fd().to_string();
            linkat(
                &proc.file,
                number.as_str(),
                &result.directory.parent.file,
                result.directory.name.as_os_str(),
                AtFlags::AT_SYMLINK_FOLLOW,
            )
            .map_err(io::Error::from)?;
            *published = true;
        }
        let current =
            stat(&result.directory.parent.file, &result.directory.name)?.ok_or_else(limit)?;
        validate_file(&current)?;
        if (current.st_dev, current.st_ino) != identity {
            return Err(limit());
        }
        Ok(result)
    }
}
#[derive(Debug)]
pub struct ActivatedLogFile {
    pub directory: LogDirectory,
    pub file: BudgetFile,
    identity: (u64, u64),
    created: bool,
    committed: bool,
    rotation_pending: bool,
    partial_write: Option<(u64, u64)>,
    partial_dirty: Arc<AtomicBool>,
    config: LogConfig,
}
impl ActivatedLogFile {
    pub(crate) fn created_by_candidate(&self) -> bool {
        self.created
    }
    pub fn commit(&mut self) {
        self.committed = true;
    }
    pub fn rollback(&mut self) -> bool {
        let removed = self.remove_created();
        if removed {
            self.created = false;
        }
        removed
    }
    fn remove_created(&self) -> bool {
        if !self.created || self.committed {
            return true;
        }
        let parent = &self.directory.parent.file;
        match stat(parent, &self.directory.name) {
            Ok(None) => true,
            Ok(Some(current))
                if (current.st_dev, current.st_ino) != self.identity
                    || SFlag::from_bits_truncate(current.st_mode) != SFlag::S_IFREG =>
            {
                true
            }
            Ok(Some(_)) => match unlinkat(
                parent,
                self.directory.name.as_os_str(),
                UnlinkatFlags::NoRemoveDir,
            ) {
                Ok(()) | Err(Errno::ENOENT) => true,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }
    pub fn current_identity_matches(&self) -> io::Result<bool> {
        if self.partial_dirty.load(Ordering::Acquire) {
            return Ok(false);
        }
        Ok(
            stat(&self.directory.parent.file, &self.directory.name)?.is_some_and(|current| {
                (current.st_dev, current.st_ino) == self.identity && validate_file(&current).is_ok()
            }),
        )
    }

    pub fn write_record(
        &mut self,
        bytes: &[u8],
        policy: super::log_queue::RotationPolicy,
    ) -> io::Result<()> {
        if !self.committed
            || bytes.len() > 16 * 1024
            || !(16 * 1024..=MAX_FILE_SIZE).contains(&policy.rotate_size)
            || !(1..=5).contains(&policy.retain_files)
            || !(1..=7).contains(&policy.retain_days)
        {
            return Err(limit());
        }
        self.recover_rotation()?;
        self.recover_partial_write()?;
        self.cleanup_expired(policy.retain_days)?;
        let current = stat(&self.directory.parent.file, &self.directory.name)?.ok_or_else(limit)?;
        let size = validate_file(&current)?;
        if (current.st_dev, current.st_ino) != self.identity {
            return Err(limit());
        }
        let after = size
            .checked_add(u64::try_from(bytes.len()).map_err(|_| limit())?)
            .ok_or_else(limit)?;
        if after > policy.rotate_size {
            self.rotate(policy)?;
        }
        let start = self.file.file.metadata()?.len();
        let end = start
            .checked_add(u64::try_from(bytes.len()).map_err(|_| limit())?)
            .ok_or_else(limit)?;
        self.partial_write = Some((start, end));
        self.partial_dirty.store(true, Ordering::Release);
        match self.file.file.write_all(bytes) {
            Ok(()) => {
                self.partial_write = None;
                self.partial_dirty.store(false, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                let _ = self.recover_partial_write();
                Err(error)
            }
        }
    }

    fn recover_partial_write(&mut self) -> io::Result<()> {
        let Some((start, end)) = self.partial_write else {
            return Ok(());
        };
        let named = stat(&self.directory.parent.file, &self.directory.name)?.ok_or_else(limit)?;
        let length = validate_file(&named)?;
        let opened = self.file.file.metadata()?;
        if (named.st_dev, named.st_ino) != self.identity
            || (opened.dev(), opened.ino()) != self.identity
            || !opened.is_file()
            || opened.nlink() != 1
            || opened.len() != length
            || !(start..=end).contains(&length)
        {
            return Err(limit());
        }
        if length != start {
            self.file.file.set_len(start)?;
        }
        self.partial_write = None;
        self.partial_dirty.store(false, Ordering::Release);
        Ok(())
    }

    pub fn cleanup_expired(&mut self, retain_days: u8) -> io::Result<()> {
        if !(1..=7).contains(&retain_days) {
            return Err(limit());
        }
        let parent = &self.directory.parent.file;
        let _ticket = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
        let mut scan = Dir::openat(
            parent,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let mut prefix = self.directory.name.as_bytes().to_vec();
        prefix.push(b'.');
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .map_err(|_| limit())?;
        for (index, entry) in scan.iter().enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(limit());
            }
            let entry = entry.map_err(io::Error::from)?;
            let raw = entry.file_name().to_bytes();
            let Some(suffix) = raw.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
                continue;
            }
            let number = std::str::from_utf8(suffix)
                .ok()
                .and_then(|n| n.parse::<u64>().ok());
            if number == Some(0) {
                continue;
            }
            if number.is_none_or(|n| n > 5) || suffix[0] == b'0' {
                return Err(limit());
            }
            if self.rotation_pending && number == Some(1) {
                continue;
            }
            let name = OsStr::from_bytes(raw);
            let old = stat(parent, name)?.ok_or_else(limit)?;
            validate_file(&old)?;
            if now.saturating_sub(old.st_mtime) <= i64::from(retain_days) * 86_400 {
                continue;
            }
            let current = stat(parent, name)?.ok_or_else(limit)?;
            if (old.st_dev, old.st_ino, old.st_mode)
                != (current.st_dev, current.st_ino, current.st_mode)
            {
                return Err(limit());
            }
            validate_file(&current)?;
            unlinkat(parent, name, UnlinkatFlags::NoRemoveDir).map_err(io::Error::from)?;
        }
        Ok(())
    }

    fn recover_rotation(&mut self) -> io::Result<()> {
        if !self.rotation_pending {
            return Ok(());
        }
        let parent = &self.directory.parent.file;
        let mut first = self.directory.name.as_bytes().to_vec();
        first.extend_from_slice(b".1");
        let prior = stat(parent, OsStr::from_bytes(&first))?.ok_or_else(limit)?;
        validate_file(&prior)?;
        if (prior.st_dev, prior.st_ino) != self.identity {
            return Err(limit());
        }
        renameat2(
            parent,
            OsStr::from_bytes(&first),
            parent,
            self.directory.name.as_os_str(),
            RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        self.rotation_pending = false;
        Ok(())
    }

    fn rotate(&mut self, policy: super::log_queue::RotationPolicy) -> io::Result<()> {
        let parent = &self.directory.parent.file;
        let ticket = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
        let mut scan = Dir::openat(
            parent,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let mut prefix = self.directory.name.as_bytes().to_vec();
        prefix.push(b'.');
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .map_err(|_| limit())?;
        let mut entries = Vec::new();
        for (index, entry) in scan.iter().enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(limit());
            }
            let entry = entry.map_err(io::Error::from)?;
            let raw = entry.file_name().to_bytes();
            let Some(suffix) = raw.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
                continue;
            }
            let number = std::str::from_utf8(suffix)
                .ok()
                .and_then(|n| n.parse::<u8>().ok());
            if number == Some(0) {
                continue;
            }
            let number = number
                .filter(|n| *n <= 5 && suffix[0] != b'0')
                .ok_or_else(limit)?;
            let name = OsString::from_vec(raw.to_vec());
            let metadata = stat(parent, &name)?.ok_or_else(limit)?;
            validate_file(&metadata)?;
            entries.push((number, name, metadata));
        }
        drop(scan);
        drop(ticket);
        entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        for (number, name, metadata) in entries {
            let actual = stat(parent, &name)?.ok_or_else(limit)?;
            if (metadata.st_dev, metadata.st_ino, metadata.st_mode)
                != (actual.st_dev, actual.st_ino, actual.st_mode)
            {
                return Err(limit());
            }
            if number >= policy.retain_files
                || now.saturating_sub(metadata.st_mtime) > i64::from(policy.retain_days) * 86_400
            {
                unlinkat(parent, name.as_os_str(), UnlinkatFlags::NoRemoveDir)
                    .map_err(io::Error::from)?;
            } else {
                let mut target = prefix.clone();
                target.extend_from_slice((number + 1).to_string().as_bytes());
                renameat2(
                    parent,
                    name.as_os_str(),
                    parent,
                    OsStr::from_bytes(&target),
                    RenameFlags::RENAME_NOREPLACE,
                )
                .map_err(io::Error::from)?;
            }
        }
        let active = stat(parent, &self.directory.name)?.ok_or_else(limit)?;
        validate_file(&active)?;
        if (active.st_dev, active.st_ino) != self.identity {
            return Err(limit());
        }
        let mut first = prefix;
        first.push(b'1');
        renameat2(
            parent,
            self.directory.name.as_os_str(),
            parent,
            OsStr::from_bytes(&first),
            RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(io::Error::from)?;
        self.rotation_pending = true;
        let ticket = LogBudget::process().reserve_fd().ok_or_else(capacity)?;
        let parent_fd = openat(
            parent,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let directory = LogDirectory {
            parent: BudgetFile::new(File::from(parent_fd), ticket),
            name: self.directory.name.clone(),
        };
        let mut config = self.config.clone();
        config.retain_days = policy.retain_days;
        config.retain_files = policy.retain_files;
        let prepared = PreparedLogFile { directory, config };
        let mut replacement =
            prepared.activate_inner(false, &mut false, Arc::clone(&self.partial_dirty))?;
        replacement.commit();
        let old_identity = self.identity;
        *self = replacement;
        if now.saturating_sub(active.st_mtime) > i64::from(policy.retain_days) * 86_400 {
            let parent = &self.directory.parent.file;
            let rotated = stat(parent, OsStr::from_bytes(&first))?.ok_or_else(limit)?;
            if (rotated.st_dev, rotated.st_ino) != old_identity {
                return Err(limit());
            }
            unlinkat(
                parent,
                OsStr::from_bytes(&first),
                UnlinkatFlags::NoRemoveDir,
            )
            .map_err(io::Error::from)?;
        }
        Ok(())
    }
}
impl Drop for ActivatedLogFile {
    fn drop(&mut self) {
        let _ = self.remove_created();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::source::Origin;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    #[test]
    #[ignore = "requires a private 1 MiB tmpfs via RILL_LOG_ENOSPC_DIR"]
    fn enospc_partial_record_is_rolled_back_before_next_record() {
        let root = std::path::PathBuf::from(
            std::env::var_os("RILL_LOG_ENOSPC_DIR").expect("isolated tmpfs required"),
        );
        let target = root.join("access.log");
        let text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            target.display()
        );
        let document = kdl::KdlDocument::parse_v2(&text).unwrap();
        let config = crate::observability::file_config::parse_access_log(
            &document.nodes()[0],
            &Origin::file(Path::new("/tmp/config.kdl")),
        )
        .unwrap();
        let mut file = PreparedLogFile::prepare(&config)
            .unwrap()
            .activate()
            .unwrap();
        file.commit();
        let filler_path = root.join("filler");
        let mut filler = File::create(&filler_path).unwrap();
        let block = [b'f'; 4096];
        loop {
            if filler.write_all(&block).is_err() {
                break;
            }
        }
        let used = filler.metadata().unwrap().len();
        assert!(used >= 4096);
        filler.set_len(used - 4096).unwrap();
        let probe_path = root.join("probe");
        let mut probe = File::create(&probe_path).unwrap();
        let record = vec![b'x'; 16384];
        let partial = probe.write(&record).unwrap();
        assert!(partial > 0 && partial < record.len());
        drop(probe);
        fs::remove_file(&probe_path).unwrap();
        let policy = super::super::log_queue::RotationPolicy::from(&config);
        assert!(file.write_record(&record, policy).is_err());
        assert_eq!(fs::metadata(&target).unwrap().len(), 0);
        drop(filler);
        fs::remove_file(&filler_path).unwrap();
        file.write_record(b"{\"ok\":true}\n", policy).unwrap();
        let contents = fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed["ok"], true);
        drop(file);
        fs::remove_file(target).unwrap();
    }

    #[test]
    fn partial_write_recovery_refuses_foreign_file_and_retries() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rill-log-partial-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.join("access.log");
        let text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            target.display()
        );
        let document = kdl::KdlDocument::parse_v2(&text).unwrap();
        let config = crate::observability::file_config::parse_access_log(
            &document.nodes()[0],
            &Origin::file(Path::new("/tmp/config.kdl")),
        )
        .unwrap();
        let mut file = PreparedLogFile::prepare(&config)
            .unwrap()
            .activate()
            .unwrap();
        file.commit();
        let policy = super::super::log_queue::RotationPolicy::from(&config);
        file.write_record(b"base\n", policy).unwrap();
        file.partial_write = Some((5, 16));
        File::options()
            .append(true)
            .open(&target)
            .unwrap()
            .write_all(b"partial")
            .unwrap();
        let moved = directory.join("moved");
        fs::rename(&target, &moved).unwrap();
        fs::write(&target, b"foreign").unwrap();
        assert!(file.write_record(b"good\n", policy).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"foreign");
        fs::remove_file(&target).unwrap();
        fs::rename(&moved, &target).unwrap();
        file.write_record(b"good\n", policy).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"base\ngood\n");
        drop(file);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn idle_worker_removes_expired_rotation_without_requests() {
        use std::{
            sync::{Arc, Mutex},
            time::Duration,
        };
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rill-log-age-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.join("access.log");
        let text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            target.display()
        );
        let document = kdl::KdlDocument::parse_v2(&text).unwrap();
        let config = crate::observability::file_config::parse_access_log(
            &document.nodes()[0],
            &Origin::file(Path::new("/tmp/config.kdl")),
        )
        .unwrap();
        let mut file = PreparedLogFile::prepare(&config)
            .unwrap()
            .activate()
            .unwrap();
        file.commit();
        let rotated = directory.join("access.log.1");
        fs::write(&rotated, b"old\n").unwrap();
        File::options()
            .write(true)
            .open(&rotated)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(2 * 86400)),
            )
            .unwrap();
        file.rotation_pending = true;
        file.cleanup_expired(1).unwrap();
        assert!(rotated.exists());
        file.rotation_pending = false;
        let file = Arc::new(Mutex::new(file));
        let maintenance_file = Arc::clone(&file);
        let budget = LogBudget::process();
        let (mut worker, _queue) =
            crate::observability::log_worker::LogWorker::start_reserved_with_maintenance(
                budget.reserve_worker().unwrap(),
                budget.reserve_site().unwrap(),
                |_| {},
                move || {
                    maintenance_file.lock().unwrap().cleanup_expired(1).unwrap();
                },
                Duration::from_millis(20),
                (),
            )
            .unwrap();
        worker.commit();
        for _ in 0..100 {
            if !rotated.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!rotated.exists());
        assert!(worker.shutdown(Duration::from_secs(2)));
        drop(file);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn committed_writer_rotates_and_retains_by_record_policy() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rill-log-rotate-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.join("access.log");
        let text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            target.display()
        );
        let doc = kdl::KdlDocument::parse_v2(&text).unwrap();
        let config = crate::observability::file_config::parse_access_log(
            &doc.nodes()[0],
            &Origin::file(Path::new("/tmp/config.kdl")),
        )
        .unwrap();
        let mut file = PreparedLogFile::prepare(&config)
            .unwrap()
            .activate()
            .unwrap();
        file.commit();
        let policy = super::super::log_queue::RotationPolicy::from(&config);
        let first = vec![b'a'; 16384];
        let second = vec![b'b'; 16384];
        file.write_record(&first, policy).unwrap();
        let rotated = directory.join("access.log.1");
        fs::rename(&target, &rotated).unwrap();
        file.rotation_pending = true;
        fs::write(&target, b"foreign").unwrap();
        assert!(file.write_record(b"x", policy).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"foreign");
        fs::remove_file(&target).unwrap();
        file.write_record(&second, policy).unwrap();
        assert_eq!(fs::read(directory.join("access.log.1")).unwrap(), first);
        assert_eq!(fs::read(&target).unwrap(), second);
        file.write_record(b"c", policy).unwrap();
        assert_eq!(fs::read(directory.join("access.log.1")).unwrap(), second);
        assert_eq!(fs::read(&target).unwrap(), b"c");
        assert!(!directory.join("access.log.2").exists());
        drop(file);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activation_rolls_back_only_created_file_and_rejects_unsafe_rotations() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rill-log-file-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let config_text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            directory.join("access.log").display()
        );
        let document = kdl::KdlDocument::parse_v2(&config_text).unwrap();
        let config = crate::observability::file_config::parse_access_log(
            &document.nodes()[0],
            &Origin::file(Path::new("/tmp/config.kdl")),
        )
        .unwrap();
        let target = directory.join("access.log");
        let prepared = PreparedLogFile::prepare(&config).unwrap();
        assert!(!target.exists());
        drop(prepared.activate().unwrap());
        assert!(!target.exists());
        let activated = PreparedLogFile::prepare(&config)
            .unwrap()
            .activate()
            .unwrap();
        let moved = directory.join("moved");
        fs::rename(&target, &moved).unwrap();
        fs::write(&target, b"replacement").unwrap();
        drop(activated);
        assert_eq!(fs::read(&target).unwrap(), b"replacement");
        fs::remove_file(&moved).unwrap();
        fs::write(&target, b"existing").unwrap();
        drop(
            PreparedLogFile::prepare(&config)
                .unwrap()
                .activate()
                .unwrap(),
        );
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        if nix::unistd::geteuid().as_raw() != 0 {
            fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
            assert!(PreparedLogFile::prepare(&config).is_err());
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::hard_link(&target, directory.join("alias")).unwrap();
        assert!(PreparedLogFile::prepare(&config).is_err());
        fs::remove_file(directory.join("alias")).unwrap();
        let prepared = PreparedLogFile::prepare(&config).unwrap();
        symlink(&target, directory.join("access.log.1")).unwrap();
        assert!(prepared.activate().is_err());
        assert!(PreparedLogFile::prepare(&config).is_err());
        fs::remove_file(directory.join("access.log.1")).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_len(MAX_FILE_SIZE + 1)
            .unwrap();
        assert!(PreparedLogFile::prepare(&config).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
