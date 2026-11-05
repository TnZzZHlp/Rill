use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::snapshot::TestResult;

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sandbox() -> TestResult<Sandbox> {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!("rill-log-{}-{unique}", std::process::id()));
    DirBuilder::new().mode(0o700).create(&path)?;
    Ok(Sandbox(path))
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    Prepared,
    Active,
    RolledBack,
}

struct Candidate {
    path: PathBuf,
    state: State,
    created: bool,
}

impl Candidate {
    fn prepare(path: PathBuf) -> Self {
        Self {
            path,
            state: State::Prepared,
            created: false,
        }
    }

    fn activate(&mut self) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(b"")?;
        self.created = true;
        self.state = State::Active;
        Ok(())
    }

    fn rollback(&mut self) {
        if self.created {
            let _ = fs::remove_file(&self.path);
        }
        self.state = State::RolledBack;
    }
}

fn exists(path: &PathBuf) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[test]
fn candidate_preparation_creates_no_file() -> TestResult {
    let sandbox = sandbox()?;
    let path = sandbox.0.join("access.jsonl");
    let candidate = Candidate::prepare(path.clone());
    assert_eq!(candidate.state, State::Prepared);
    assert!(!exists(&path), "prepare must not create the log file");
    let names: Vec<PathBuf> = fs::read_dir(&sandbox.0)?
        .map(|entry| Ok::<PathBuf, std::io::Error>(entry?.path()))
        .collect::<Result<_, _>>()?;
    assert!(names.is_empty(), "prepare left artifacts: {names:?}");
    Ok(())
}

#[test]
fn mid_activation_failure_rolls_back_every_candidate_and_keeps_the_previous_file() -> TestResult {
    let sandbox = sandbox()?;
    let previous = sandbox.0.join("previous.jsonl");
    let committed = sandbox.0.join("new.jsonl");
    let blocked = sandbox.0.join("blocked.jsonl");
    fs::write(&previous, b"previous\n")?;
    let before = fs::metadata(&previous)?;
    fs::create_dir(&blocked)?;

    let mut candidates = vec![
        Candidate::prepare(committed.clone()),
        Candidate::prepare(blocked.clone()),
    ];
    let mut activated = 0usize;
    for candidate in &mut candidates {
        if candidate.activate().is_err() {
            for candidate in &mut candidates {
                candidate.rollback();
            }
            break;
        }
        activated += 1;
    }
    assert_eq!(
        activated, 1,
        "targeting a directory must fail only after an earlier candidate committed"
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.state == State::RolledBack),
        "every candidate, including the committed one, must be rolled back"
    );
    assert!(!exists(&committed), "rolled-back candidate must be removed");
    assert_eq!(fs::metadata(&previous)?.ino(), before.ino());
    assert_eq!(fs::read(&previous)?, b"previous\n");
    assert!(blocked.is_dir());
    Ok(())
}

struct Worker {
    sender: std::sync::mpsc::Sender<(Vec<u8>, std::sync::mpsc::Sender<std::io::Result<()>>)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn start(path: &std::path::Path, closed: std::sync::mpsc::Sender<()>) -> std::io::Result<Self> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        let (sender, receiver) =
            std::sync::mpsc::channel::<(Vec<u8>, std::sync::mpsc::Sender<std::io::Result<()>>)>();
        let handle = std::thread::spawn(move || {
            for (bytes, ack) in receiver {
                let result = file.write_all(&bytes).and_then(|()| file.flush());
                let _ = ack.send(result);
            }
            drop(file);
            let _ = closed.send(());
        });
        Ok(Self {
            sender,
            handle: Some(handle),
        })
    }
    fn write(&self, bytes: &[u8]) -> TestResult {
        let (tx, rx) = std::sync::mpsc::channel();
        self.sender.send((bytes.to_vec(), tx))?;
        rx.recv_timeout(Duration::from_secs(5))??;
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let (replacement, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.sender, replacement));
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

#[test]
fn worker_publication_reuse_failure_and_old_request_lifetime() -> TestResult {
    use arc_swap::ArcSwap;
    use std::sync::{Arc, mpsc};
    struct Snapshot {
        worker: Arc<Worker>,
    }
    let sandbox = sandbox()?;
    let path = sandbox.0.join("active.jsonl");
    let (closed_tx, closed_rx) = mpsc::channel();
    let worker = Arc::new(Worker::start(&path, closed_tx)?);
    worker.write(b"initial\n")?;
    let inode = fs::metadata(&path)?.ino();
    let active = ArcSwap::from_pointee(Snapshot {
        worker: Arc::clone(&worker),
    });
    drop(worker);
    let old_request = active.load_full();
    let prepared = Snapshot {
        worker: Arc::clone(&old_request.worker),
    };
    assert_eq!(fs::read_dir(&sandbox.0)?.count(), 1);
    active.store(Arc::new(prepared));
    let reused = active.load_full();
    assert!(Arc::ptr_eq(&old_request.worker, &reused.worker));
    assert_eq!(fs::metadata(&path)?.ino(), inode);
    let new1 = sandbox.0.join("new1");
    let new2 = sandbox.0.join("new2");
    let blocked = sandbox.0.join("blocked");
    fs::create_dir(&blocked)?;
    let mut candidates = vec![
        Candidate::prepare(new1.clone()),
        Candidate::prepare(new2.clone()),
        Candidate::prepare(blocked.clone()),
    ];
    let mut count = 0;
    for candidate in &mut candidates {
        if candidate.activate().is_err() {
            break;
        }
        count += 1;
    }
    assert_eq!(count, 2);
    for candidate in &mut candidates {
        candidate.rollback();
    }
    assert!(!new1.exists() && !new2.exists());
    assert!(Arc::ptr_eq(&reused, &active.load_full()));
    assert_eq!(fs::metadata(&path)?.ino(), inode);
    assert_eq!(fs::read(&path)?, b"initial\n");
    let (new_closed_tx, _new_closed_rx) = mpsc::channel();
    let new_worker = Arc::new(Worker::start(
        &sandbox.0.join("replacement"),
        new_closed_tx,
    )?);
    active.store(Arc::new(Snapshot { worker: new_worker }));
    drop(reused);
    old_request.worker.write(b"old request\n")?;
    assert_eq!(fs::read(&path)?, b"initial\nold request\n");
    assert!(closed_rx.try_recv().is_err());
    drop(old_request);
    closed_rx.recv_timeout(Duration::from_secs(5))?;
    Ok(())
}
