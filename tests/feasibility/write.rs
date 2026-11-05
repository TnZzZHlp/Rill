use arc_swap::ArcSwap;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
    _storage_id: String,
    locks: BTreeMap<String, Lock>,
    limit: usize,
}

impl Storage {
    fn new(storage_id: &str, limit: usize) -> Self {
        Self {
            _storage_id: storage_id.to_string(),
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

struct StorageRegistry {
    identity: (u64, u64),
    locks: Mutex<Storage>,
    namespace: Mutex<()>,
}
struct DavSnapshot {
    registry: Arc<StorageRegistry>,
}
impl DavSnapshot {
    fn prepare(root: &File, registry: &Arc<StorageRegistry>) -> TestResult<Self> {
        let stat = nix::sys::stat::fstat(root.as_fd())?;
        if (stat.st_dev, stat.st_ino) != registry.identity {
            return Err("RESTART_REQUIRED".into());
        }
        Ok(Self {
            registry: Arc::clone(registry),
        })
    }
}

#[test]
fn locks_survive_snapshot_publication_and_root_replacement_is_rejected() -> TestResult {
    let sandbox = sandbox()?;
    let root = File::open(&sandbox.0)?;
    let stat = nix::sys::stat::fstat(root.as_fd())?;
    let identity = (stat.st_dev, stat.st_ino);
    let registry = Arc::new(StorageRegistry {
        identity,
        locks: Mutex::new(Storage::new("documents", 2)),
        namespace: Mutex::new(()),
    });
    let active = ArcSwap::from_pointee(DavSnapshot::prepare(&root, &registry)?);
    let old = active.load_full();
    old.registry
        .locks
        .lock()
        .unwrap()
        .lock("/a", "token", Duration::from_secs(60))
        .unwrap();
    let candidate = DavSnapshot::prepare(&File::open(&sandbox.0)?, &registry)?;
    active.store(Arc::new(candidate));
    let current = active.load_full();
    assert!(!Arc::ptr_eq(&old, &current));
    assert!(Arc::ptr_eq(&old.registry, &current.registry));
    assert_eq!(
        current.registry.locks.lock().unwrap().unlock("/a", "wrong"),
        Err("DAV_LOCK_TOKEN")
    );
    fs::create_dir(sandbox.0.join("replacement"))?;
    let rejected = DavSnapshot::prepare(&File::open(sandbox.0.join("replacement"))?, &registry)
        .err()
        .ok_or("changed storage root was accepted")?;
    assert_eq!(rejected.to_string(), "RESTART_REQUIRED");
    assert!(Arc::ptr_eq(&current, &active.load_full()));
    assert_eq!(
        current.registry.locks.lock().unwrap().unlock("/a", "token"),
        Ok(())
    );
    Ok(())
}

#[test]
fn exclusive_locks_enforce_ownership_capacity_and_restart_reset() {
    let mut current = Storage::new("documents", 2);
    current
        .lock("/a", "token-1", Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        current.lock("/a", "token-2", Duration::from_secs(60)),
        Err("DAV_LOCKED")
    );
    current
        .lock("/b", "token-2", Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        current.lock("/c", "token-3", Duration::from_secs(60)),
        Err("DAV_LIMIT")
    );
    assert_eq!(current.unlock("/a", "wrong"), Err("DAV_LOCK_TOKEN"));
    assert_eq!(current.unlock("/a", "token-1"), Ok(()));
    let mut restarted = Storage::new("documents", 2);
    assert_eq!(restarted.unlock("/b", "token-2"), Err("DAV_LOCK_MISSING"));
    assert_eq!(
        restarted.lock("/b", "fresh", Duration::from_secs(60)),
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

#[test]
fn held_descriptor_observes_old_inode_after_replacement() -> TestResult {
    let sandbox = sandbox()?;
    fs::create_dir(sandbox.0.join("docs"))?;
    fs::write(sandbox.0.join("docs/note"), b"old")?;
    let mut held = File::open(sandbox.0.join("docs/note"))?;
    write_atomically(&File::open(&sandbox.0)?, Path::new("docs"), "note", b"new")?;
    let mut old = Vec::new();
    held.read_to_end(&mut old)?;
    assert_eq!(old, b"old");
    assert_eq!(fs::read(sandbox.0.join("docs/note"))?, b"new");
    Ok(())
}

fn revalidate_parent(root: &File, held: &File, path: &Path) -> TestResult {
    let reopened = File::from(openat2(
        root,
        path,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_DIRECTORY)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_MAGICLINKS),
    )?);
    let before = nix::sys::stat::fstat(held.as_fd())?;
    let after = nix::sys::stat::fstat(reopened.as_fd())?;
    if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino) {
        return Err("parent identity changed".into());
    }
    Ok(())
}

#[test]
fn moved_parent_identity_is_rejected_before_write() -> TestResult {
    let sandbox = sandbox()?;
    let root_path = sandbox.0.join("root");
    fs::create_dir(&root_path)?;
    fs::create_dir(root_path.join("docs"))?;
    let root = File::open(&root_path)?;
    let held = File::open(root_path.join("docs"))?;
    fs::rename(root_path.join("docs"), sandbox.0.join("moved"))?;
    symlink(sandbox.0.join("moved"), root_path.join("docs"))?;
    assert!(revalidate_parent(&root, &held, Path::new("docs")).is_err());
    fs::remove_file(root_path.join("docs"))?;
    fs::create_dir(root_path.join("docs"))?;
    assert!(revalidate_parent(&root, &held, Path::new("docs")).is_err());
    assert!(held.metadata()?.is_dir());
    assert!(!sandbox.0.join("moved/note").exists());
    Ok(())
}

#[test]
fn cancelled_or_failed_temporary_write_preserves_target() -> TestResult {
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let sandbox = sandbox()?;
    let target = sandbox.0.join("note");
    fs::write(&target, b"old")?;
    for fail in [false, true] {
        let temp = Temporary(sandbox.0.join(".note.tmp"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp.0)?;
        file.write_all(b"partial")?;
        if fail {
            drop(file);
            assert!(fs::rename(&temp.0, sandbox.0.join("missing/note")).is_err());
        }
        drop(temp);
        assert_eq!(fs::read(&target)?, b"old");
        assert_eq!(fs::read_dir(&sandbox.0)?.count(), 1);
    }
    Ok(())
}

#[test]
fn namespace_guard_serializes_move_across_snapshot_publication() -> TestResult {
    use std::sync::mpsc;
    let sandbox = sandbox()?;
    fs::create_dir(sandbox.0.join("docs"))?;
    let root = File::open(&sandbox.0)?;
    let stat = nix::sys::stat::fstat(root.as_fd())?;
    let registry = Arc::new(StorageRegistry {
        identity: (stat.st_dev, stat.st_ino),
        locks: Mutex::new(Storage::new("documents", 2)),
        namespace: Mutex::new(()),
    });
    let active = ArcSwap::from_pointee(DavSnapshot::prepare(&root, &registry)?);
    let old = active.load_full();
    let guard = old.registry.namespace.lock().unwrap();
    let parent = File::open(sandbox.0.join("docs"))?;
    active.store(Arc::new(DavSnapshot::prepare(&root, &registry)?));
    let newer = active.load_full();
    let path = sandbox.0.clone();
    let (attempted_tx, attempted_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        assert!(newer.registry.namespace.try_lock().is_err());
        attempted_tx.send(()).unwrap();
        let _guard = newer.registry.namespace.lock().unwrap();
        fs::rename(path.join("docs"), path.join("moved")).unwrap();
        done_tx.send(()).unwrap();
    });
    attempted_rx.recv_timeout(Duration::from_secs(5))?;
    assert!(done_rx.try_recv().is_err());
    revalidate_parent(&root, &parent, Path::new("docs"))?;
    write_atomically(&root, Path::new("docs"), "note", b"committed")?;
    drop(guard);
    done_rx.recv_timeout(Duration::from_secs(5))?;
    thread.join().unwrap();
    assert_eq!(fs::read(sandbox.0.join("moved/note"))?, b"committed");
    Ok(())
}
