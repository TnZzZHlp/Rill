use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
};

use nix::{
    fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2},
    sys::stat::Mode,
    unistd::geteuid,
};

use super::log_budget::{Credit, LogBudget};

#[derive(Debug)]
pub struct BudgetFile {
    pub file: File,
    _credit: Credit,
}
impl BudgetFile {
    pub fn new(file: File, credit: Credit) -> Self {
        Self {
            file,
            _credit: credit,
        }
    }
}

#[derive(Debug)]
pub struct LogDirectory {
    pub parent: BudgetFile,
    pub name: OsString,
}

fn invalid() -> io::Error {
    io::Error::other("unsafe log directory")
}
fn credit() -> io::Result<Credit> {
    LogBudget::process()
        .reserve_fd()
        .ok_or_else(|| io::Error::other("LOG_CAPACITY"))
}
fn check_ancestor(file: &File, uid: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || (metadata.uid() != 0 && metadata.uid() != uid)
        || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
    {
        return Err(invalid());
    }
    Ok(())
}
fn open_directory(parent: &File, component: &OsStr) -> io::Result<BudgetFile> {
    let ticket = credit()?;
    let fd = openat2(
        parent,
        component,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )
    .map_err(io::Error::from)?;
    Ok(BudgetFile::new(File::from(fd), ticket))
}
impl LogDirectory {
    pub fn prepare(path: &Path) -> io::Result<Self> {
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || raw.ends_with(b"/")
            || raw.ends_with(b"/.")
            || raw.ends_with(b"/..")
        {
            return Err(invalid());
        }
        let name = path.file_name().ok_or_else(invalid)?;
        if name.as_bytes().is_empty() || name.as_bytes().contains(&0) {
            return Err(invalid());
        }
        let parent = path.parent().ok_or_else(invalid)?;
        if parent
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(invalid());
        }
        let uid = geteuid().as_raw();
        let ticket = credit()?;
        let fd = open(
            Path::new("/"),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let mut current = BudgetFile::new(File::from(fd), ticket);
        check_ancestor(&current.file, uid)?;
        for component in parent.components() {
            if let Component::Normal(component) = component {
                let next = open_directory(&current.file, component)?;
                check_ancestor(&next.file, uid)?;
                current = next;
            }
        }
        let metadata = current.file.metadata()?;
        if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
            return Err(invalid());
        }
        Ok(Self {
            parent: current,
            name: name.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn fd_saturation_rejects_directory_prepare_without_side_effects() {
        const CHILD: &str = "RILL_LOG_FD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "observability::log_directory::tests::fd_saturation_rejects_directory_prepare_without_side_effects", "--test-threads=1"])
                .env(CHILD, "1").status().unwrap();
            assert!(status.success());
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rill-log-fd-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut handles = Vec::new();
        while let Some(credit) = LogBudget::process().reserve_fd() {
            handles.push(BudgetFile::new(File::open("/dev/null").unwrap(), credit));
        }
        assert_eq!(handles.len(), 256);
        let target = root.join("log");
        assert_eq!(
            LogDirectory::prepare(&target).unwrap_err().to_string(),
            "LOG_CAPACITY"
        );
        assert!(!target.exists());
        drop(handles.split_off(handles.len() - 4));
        assert!(LogDirectory::prepare(&target).is_ok());
        drop(handles);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requires_existing_private_directory_and_rejects_symlinks() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rill-log-directory-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.join("access.log");
        assert!(LogDirectory::prepare(&target).is_ok());
        assert!(LogDirectory::prepare(Path::new(&format!("{}/.", root.display()))).is_err());
        let parent = root.join("ancestor");
        let private = parent.join("private");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(LogDirectory::prepare(&private.join("access.log")).is_err());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(LogDirectory::prepare(&private.join("access.log")).is_ok());
        assert!(!target.exists());
        assert!(LogDirectory::prepare(&root.join("missing/access.log")).is_err());
        symlink(&root, root.join("alias")).unwrap();
        assert!(LogDirectory::prepare(&root.join("alias/access.log")).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(LogDirectory::prepare(&target).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
