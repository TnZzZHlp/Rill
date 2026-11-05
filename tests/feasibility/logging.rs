use std::fs::{self, DirBuilder, File, OpenOptions};
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
    stop: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn start(path: PathBuf) -> TestResult<Self> {
        let (stop, stopped) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            loop {
                match stopped.recv_timeout(Duration::from_millis(50)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let _ = file.write_all(b"line\n");
                        let _ = file.flush();
                    }
                }
            }
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn a_committed_worker_keeps_writing_until_its_owner_releases_it() -> TestResult {
    let sandbox = sandbox()?;
    let path = sandbox.0.join("site.jsonl");
    File::create(&path)?;
    let _worker = Worker::start(path.clone())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        == 0
    {
        if std::time::Instant::now() >= deadline {
            return Err("worker never wrote".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[test]
fn a_rejected_candidate_never_replaces_the_previous_active_file() -> TestResult {
    let sandbox = sandbox()?;
    let path = sandbox.0.join("site.jsonl");
    File::create(&path)?.write_all(b"generation-1\n")?;
    let previous = fs::metadata(&path)?;

    let mut rejected = Candidate::prepare(sandbox.0.join("site.jsonl.rejected"));
    rejected.activate()?;
    rejected.rollback();

    let current = fs::metadata(&path)?;
    assert_eq!(fs::read(&path)?, b"generation-1\n");
    assert_eq!(
        (previous.len(), previous.modified().ok()),
        (current.len(), current.modified().ok())
    );
    assert!(!exists(&sandbox.0.join("site.jsonl.rejected")));
    Ok(())
}
