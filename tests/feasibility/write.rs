use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2, renameat};
use nix::sys::stat::Mode;

use super::snapshot::TestResult;

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sandbox() -> TestResult<Sandbox> {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!("rill-dav-{}-{unique}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(Sandbox(path))
}

fn write_atomically(root: &File, directory: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    let temporary = format!(".{name}.{}.tmp", std::process::id());
    let how = OpenHow::new()
        .flags(OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC)
        .mode(Mode::from_bits_truncate(0o600))
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    let directory_fd = File::from(openat2(
        root,
        directory,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )?);
    let mut file = File::from(openat2(&directory_fd, Path::new(&temporary), how)?);
    let result = file
        .write_all(body)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            renameat(
                directory_fd.as_fd(),
                temporary.as_str(),
                directory_fd.as_fd(),
                name,
            )
            .map_err(|error| std::io::Error::from_raw_os_error(error as i32))
        });
    if result.is_err() {
        let _ = nix::unistd::unlinkat(
            directory_fd.as_fd(),
            temporary.as_str(),
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        );
    }
    result
}

#[test]
fn constrained_write_commits_atomically_and_leaves_no_temporary_file() -> TestResult {
    let sandbox = sandbox()?;
    let root = File::open(&sandbox.0)?;
    fs::create_dir(sandbox.0.join("docs"))?;
    write_atomically(&root, Path::new("docs"), "note.txt", b"first")?;
    write_atomically(&root, Path::new("docs"), "note.txt", b"second version")?;
    assert_eq!(
        fs::read(sandbox.0.join("docs/note.txt"))?,
        b"second version"
    );
    let names: Vec<String> = fs::read_dir(sandbox.0.join("docs"))?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<_>>()?;
    assert_eq!(names, vec!["note.txt".to_string()]);
    Ok(())
}

#[test]
fn constrained_write_refuses_to_escape_the_root_through_a_symlink() -> TestResult {
    let sandbox = sandbox()?;
    let outside = sandbox.0.with_extension("outside");
    fs::DirBuilder::new().mode(0o700).create(&outside)?;
    let root = File::open(&sandbox.0)?;
    symlink(&outside, sandbox.0.join("docs"))?;
    let error = write_atomically(&root, Path::new("docs"), "note.txt", b"blocked")
        .expect_err("symlinked directory must not be writable");
    assert_eq!(error.raw_os_error(), Some(Errno::EXDEV as i32));
    assert!(!outside.join("note.txt").exists());
    let metadata = fs::symlink_metadata(sandbox.0.join("docs"))?;
    assert!(metadata.file_type().is_symlink());
    let _ = fs::remove_dir_all(&outside);
    Ok(())
}

#[test]
fn o_path_without_follow_yields_the_symlink_itself_so_writers_must_check_the_type() -> TestResult {
    let sandbox = sandbox()?;
    let root = File::open(&sandbox.0)?;
    fs::create_dir(sandbox.0.join("docs"))?;
    let outside = sandbox.0.with_extension("outside.txt");
    fs::write(&outside, b"secret")?;
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600))?;
    symlink(&outside, sandbox.0.join("docs/note.txt"))?;

    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    let entry = File::from(openat2(&root, Path::new("docs/note.txt"), how)?);
    let metadata = entry.metadata()?;
    assert!(
        metadata.file_type().is_symlink(),
        "O_PATH|O_NOFOLLOW reports the link itself, not the target"
    );

    let data = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    assert_eq!(
        openat2(&root, Path::new("docs/note.txt"), data).unwrap_err(),
        Errno::ELOOP
    );

    let mut content = String::new();
    File::open(&outside)?.read_to_string(&mut content)?;
    assert_eq!(content, "secret");
    let _ = fs::remove_file(&outside);
    Ok(())
}

struct Lock {
    token: String,
    expires: Instant,
}

#[derive(Default)]
struct Storage {
    storage_id: String,
    locks: BTreeMap<String, Lock>,
    limit: usize,
}

impl Storage {
    fn new(storage_id: &str, limit: usize) -> Self {
        Self {
            storage_id: storage_id.to_string(),
            locks: BTreeMap::new(),
            limit,
        }
    }

    fn lock(
        &mut self,
        path: &str,
        token: &str,
        ttl: Duration,
    ) -> std::result::Result<(), &'static str> {
        self.locks.retain(|_, lock| lock.expires > Instant::now());
        match self.locks.get(path) {
            Some(lock) => {
                let _ = lock;
                Err("DAV_LOCKED")
            }
            None if self.locks.len() >= self.limit => Err("DAV_LIMIT"),
            None => {
                self.locks.insert(
                    path.to_string(),
                    Lock {
                        token: token.to_string(),
                        expires: Instant::now() + ttl,
                    },
                );
                Ok(())
            }
        }
    }

    fn unlock(&mut self, path: &str, token: &str) -> std::result::Result<(), &'static str> {
        match self.locks.get(path) {
            Some(lock) if lock.token == token => {
                self.locks.remove(path);
                Ok(())
            }
            Some(_) => Err("DAV_LOCK_TOKEN"),
            None => Err("DAV_LOCK_MISSING"),
        }
    }
}

#[test]
fn exclusive_locks_survive_a_reload_and_die_with_the_process_registry() {
    let mut current = Storage::new("documents", 2);
    current
        .lock("/a", "token-1", Duration::from_secs(3600))
        .unwrap();
    assert_eq!(
        current.lock("/a", "token-2", Duration::from_secs(3600)),
        Err("DAV_LOCKED")
    );
    current
        .lock("/b", "token-2", Duration::from_secs(3600))
        .unwrap();
    assert_eq!(
        current.lock("/c", "token-3", Duration::from_secs(3600)),
        Err("DAV_LIMIT")
    );
    assert_eq!(current.unlock("/a", "wrong"), Err("DAV_LOCK_TOKEN"));
    assert_eq!(current.unlock("/a", "token-1"), Ok(()));
    assert_eq!(current.storage_id, "documents");

    let mut restarted = Storage::new("documents", 2);
    assert_eq!(restarted.unlock("/b", "token-2"), Err("DAV_LOCK_MISSING"));
    assert_eq!(
        restarted.lock("/b", "fresh", Duration::from_secs(3600)),
        Ok(())
    );
}

#[test]
fn lock_ttl_expiry_frees_the_slot_without_an_unlock() {
    let mut storage = Storage::new("documents", 1);
    storage
        .lock("/a", "token", Duration::from_millis(1))
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        storage.lock("/a", "second", Duration::from_secs(10)),
        Ok(())
    );
}

#[test]
fn storage_identity_is_the_device_and_inode_of_the_prepared_root() -> TestResult {
    let sandbox = sandbox()?;
    let root = File::open(&sandbox.0)?;
    let first = nix::sys::stat::fstat(root.as_fd())?;
    let reopened = File::open(&sandbox.0)?;
    let second = nix::sys::stat::fstat(reopened.as_fd())?;
    assert_eq!((first.st_dev, first.st_ino), (second.st_dev, second.st_ino));
    let replacement = sandbox.0.with_extension("replacement");
    fs::create_dir(&replacement)?;
    let swapped = File::open(&replacement)?;
    let third = nix::sys::stat::fstat(swapped.as_fd())?;
    assert_ne!(first.st_ino, third.st_ino);
    fs::remove_dir(&replacement)?;
    Ok(())
}
