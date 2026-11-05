use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstatat, mkdirat};
use nix::sys::statfs::{PROC_SUPER_MAGIC, fstatfs};
use nix::unistd::{UnlinkatFlags, geteuid, unlinkat};
use openssl::ssl::SslContext;

use super::material::{TrustMaterial, failure};
use crate::config::Result;
use crate::config::source::Origin;
use crate::http_policy::compile::{ExternalBudget, PreparedPolicy};

#[derive(Debug)]
pub(crate) struct TrustStore {
    pub context: Arc<SslContext>,
    pub material: Arc<TrustMaterial>,
    _directory: PrivateDirectory,
}

#[derive(Debug)]
struct PrivateDirectory {
    parent: File,
    root: File,
    name: OsString,
    dev: u64,
    ino: u64,
    children: Vec<(File, OsString, Vec<OsString>)>,
    budget: Option<ExternalBudget>,
}

struct PendingDirectory<'a> {
    parent: &'a File,
    name: &'a std::ffi::OsStr,
    dev: u64,
    ino: u64,
    armed: bool,
    budget: Option<ExternalBudget>,
}

impl Drop for PendingDirectory<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let removed =
            fstatat(self.parent, self.name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFDIR
                    && stat.st_uid == geteuid().as_raw()
                    && stat.st_dev == self.dev
                    && stat.st_ino == self.ino
            }) && unlinkat(self.parent, self.name, UnlinkatFlags::RemoveDir).is_ok();
        if !removed && let Some(budget) = self.budget.take() {
            std::mem::forget(budget);
        }
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let mut clean = true;
        for (dir, name, files) in &self.children {
            for file in files {
                clean &= unlinkat(dir, file.as_os_str(), UnlinkatFlags::NoRemoveDir).is_ok();
            }
            clean &= unlinkat(&self.root, name.as_os_str(), UnlinkatFlags::RemoveDir).is_ok();
        }
        let owned = fstatat(
            &self.parent,
            self.name.as_os_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .is_ok_and(|stat| {
            SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFDIR
                && stat.st_uid == geteuid().as_raw()
                && stat.st_dev == self.dev
                && stat.st_ino == self.ino
        });
        clean &= owned
            && unlinkat(
                &self.parent,
                self.name.as_os_str(),
                UnlinkatFlags::RemoveDir,
            )
            .is_ok();
        if !clean && let Some(budget) = self.budget.take() {
            std::mem::forget(budget);
        }
    }
}

fn trusted_parent(path: &Path) -> std::io::Result<File> {
    if !path.is_absolute() {
        return Err(std::io::Error::other(
            "temporary directory must be absolute",
        ));
    }
    let mut current = File::open("/")?;
    let validate = |file: &File| -> std::io::Result<()> {
        let m = file.metadata()?;
        if !m.is_dir()
            || (m.uid() != 0 && m.uid() != geteuid().as_raw())
            || (m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0)
        {
            return Err(std::io::Error::other("untrusted temporary directory"));
        }
        Ok(())
    };
    validate(&current)?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                current = File::from(
                    openat(
                        &current,
                        name,
                        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?,
                );
                validate(&current)?;
            }
            _ => return Err(std::io::Error::other("invalid temporary directory")),
        }
    }
    Ok(current)
}

impl TrustStore {
    pub(crate) fn prepare(
        mut material: TrustMaterial,
        policy: &PreparedPolicy,
        origin: &Origin,
    ) -> Result<Arc<Self>> {
        let copy_budget = material.reserve_private_copy(policy, origin)?;
        let parent = trusted_parent(&std::env::temp_dir()).map_err(|_| failure(origin))?;
        let proc_fd = File::open("/proc/self/fd").map_err(|_| failure(origin))?;
        if fs::read_link("/proc/self").map_err(|_| failure(origin))?
            != Path::new(&std::process::id().to_string())
            || fstatfs(&proc_fd)
                .map_err(|_| failure(origin))?
                .filesystem_type()
                != PROC_SUPER_MAGIC
        {
            return Err(failure(origin));
        }
        let parent_capability = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let actual = fs::metadata(&parent_capability).map_err(|_| failure(origin))?;
        let expected = parent.metadata().map_err(|_| failure(origin))?;
        if !actual.is_dir()
            || actual.dev() != expected.dev()
            || actual.ino() != expected.ino()
            || actual.uid() != expected.uid()
        {
            return Err(failure(origin));
        }
        let template = parent_capability.join("rill-trust-XXXXXX");
        let created = nix::unistd::mkdtemp(&template).map_err(|_| failure(origin))?;
        let name = created
            .file_name()
            .ok_or_else(|| failure(origin))?
            .to_owned();
        let created_stat = match fstatat(&parent, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(_) => {
                std::mem::forget(copy_budget);
                return Err(failure(origin));
            }
        };
        if SFlag::from_bits_truncate(created_stat.st_mode) != SFlag::S_IFDIR
            || created_stat.st_uid != geteuid().as_raw()
        {
            std::mem::forget(copy_budget);
            return Err(failure(origin));
        }
        let mut pending = PendingDirectory {
            parent: &parent,
            name: &name,
            dev: created_stat.st_dev,
            ino: created_stat.st_ino,
            armed: true,
            budget: Some(copy_budget),
        };
        let root = File::from(
            openat(
                &parent,
                name.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| failure(origin))?,
        );
        let metadata = root.metadata().map_err(|_| failure(origin))?;
        if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o7777 != 0o700 {
            return Err(failure(origin));
        }
        if metadata.dev() != pending.dev || metadata.ino() != pending.ino {
            return Err(failure(origin));
        }
        pending.armed = false;
        let budget = pending.budget.take();
        drop(pending);
        let mut directory = PrivateDirectory {
            parent,
            root,
            name,
            dev: metadata.dev(),
            ino: metadata.ino(),
            children: Vec::new(),
            budget,
        };
        let mut paths = Vec::new();
        for (index, source) in material.directories.iter().enumerate() {
            let name = OsString::from(index.to_string());
            mkdirat(
                &directory.root,
                name.as_os_str(),
                Mode::from_bits_truncate(0o700),
            )
            .map_err(|_| failure(origin))?;
            let sub = match openat(
                &directory.root,
                name.as_os_str(),
                OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => File::from(fd),
                Err(_) => {
                    if unlinkat(&directory.root, name.as_os_str(), UnlinkatFlags::RemoveDir)
                        .is_err()
                        && let Some(budget) = directory.budget.take()
                    {
                        std::mem::forget(budget);
                    }
                    return Err(failure(origin));
                }
            };
            directory.children.push((sub, name.clone(), Vec::new()));
            let (sub, _, files) = directory
                .children
                .last_mut()
                .ok_or_else(|| failure(origin))?;
            for (name, bytes) in &source.entries {
                let mut file = File::from(
                    openat(
                        &*sub,
                        name.as_os_str(),
                        OFlag::O_WRONLY
                            | OFlag::O_CREAT
                            | OFlag::O_EXCL
                            | OFlag::O_NOFOLLOW
                            | OFlag::O_CLOEXEC,
                        Mode::from_bits_truncate(0o600),
                    )
                    .map_err(|_| failure(origin))?,
                );
                files.push(name.clone());
                file.write_all(bytes).map_err(|_| failure(origin))?;
            }
            paths.push(PathBuf::from(format!(
                "/proc/self/fd/{}/{}",
                directory.root.as_raw_fd(),
                index
            )));
        }
        let context = material.context(policy, origin, &paths, true)?;
        Ok(Arc::new(Self {
            context: Arc::new(context),
            material: Arc::new(material),
            _directory: directory,
        }))
    }
}

impl PartialEq for TrustStore {
    fn eq(&self, other: &Self) -> bool {
        self.material == other.material
    }
}
impl Eq for TrustStore {}
impl std::hash::Hash for TrustStore {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.material.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::prepare::{ConfigurationOnly, build};

    #[test]
    fn private_directory_drop_cleans_and_releases_budget_when_fds_exhausted() {
        const CHILD: &str = "RILL_FD_EXHAUSTION_TEST";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new("sh")
                .args(["-c", "ulimit -n 64; exec \"$1\" --exact proxy::trust::store::tests::private_directory_drop_cleans_and_releases_budget_when_fds_exhausted", "sh"])
                .arg(std::env::current_exe().expect("test executable"))
                .env(CHILD, "1")
                .status()
                .expect("child process");
            assert!(result.success());
            return;
        }
        let base = std::env::temp_dir().join(format!("rill-fd-cleanup-{}", std::process::id()));
        fs::create_dir(&base).expect("base directory");
        let config = base.join("config.kdl");
        fs::write(&config, "server { http \"127.0.0.1:8080\"; }\nsite \"app.test\" { chain { rule { respond 200; }; }; }").expect("config");
        let prepared = build(&config, &ConfigurationOnly).expect("configuration");
        let origin = Origin::file(&config);
        let mut budget = ExternalBudget::default();
        budget
            .reserve(&prepared.resources.policy, 7 * 1024 * 1024, &origin)
            .expect("reserve");
        let parent = File::open(&base).expect("parent");
        let path = base.join("private");
        fs::create_dir(&path).expect("private directory");
        let root = File::open(&path).expect("root");
        let metadata = root.metadata().expect("metadata");
        let directory = PrivateDirectory {
            parent,
            root,
            name: OsString::from("private"),
            dev: metadata.dev(),
            ino: metadata.ino(),
            children: Vec::new(),
            budget: Some(budget),
        };
        let mut held = Vec::new();
        loop {
            match File::open("/dev/null") {
                Ok(file) => held.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EMFILE));
                    break;
                }
            }
        }
        drop(directory);
        assert!(!path.exists(), "directory survived FD exhaustion");
        let mut permits = Vec::new();
        for _ in 0..9 {
            let mut permit = ExternalBudget::default();
            permit
                .reserve(&prepared.resources.policy, 7 * 1024 * 1024, &origin)
                .expect("released process budget");
            permits.push(permit);
        }
        drop(held);
        fs::remove_dir_all(&base).expect("cleanup");
    }
}
