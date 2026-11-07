use std::{
    collections::HashMap,
    io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::{
    file_config::{FileLogRecord, LogConfig},
    log_budget::{Credit, LogBudget, SiteBudget},
    log_file::{ActivatedLogFile, PreparedLogFile, target_definitely_absent},
    log_queue::{LogDropped, LogQueue, RotationPolicy},
    log_worker::LogWorker,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    device: u64,
    inode: u64,
    name: Vec<u8>,
}
impl Key {
    fn from_file(file: &PreparedLogFile) -> io::Result<Self> {
        let directory = file.directory();
        let meta = directory.parent.file.metadata()?;
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            name: directory.name.as_bytes().to_vec(),
        })
    }
    fn conflicts(&self, other: &Self) -> bool {
        if self.device != other.device || self.inode != other.inode {
            return false;
        }
        fn rotation(base: &[u8], other: &[u8]) -> bool {
            other
                .strip_prefix(base)
                .and_then(|s| s.strip_prefix(b"."))
                .is_some_and(|s| {
                    !s.is_empty()
                        && s.iter().all(u8::is_ascii_digit)
                        && s.iter().any(|c| *c != b'0')
                })
        }
        self.name == other.name
            || rotation(&self.name, &other.name)
            || rotation(&other.name, &self.name)
    }
}

struct Lookup {
    sink: Option<Arc<LogSink>>,
    disk: Option<Arc<Credit>>,
    partial_dirty: Option<Arc<AtomicBool>>,
}

struct Entry {
    sink: Weak<LogSink>,
    worker: Weak<()>,
    disk: Arc<Credit>,
    partial_dirty: Arc<AtomicBool>,
    published: bool,
}
struct Inner {
    entries: Mutex<HashMap<Key, Entry>>,
    activation: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
pub struct LogRegistry {
    inner: Arc<Inner>,
}
impl LogRegistry {
    fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                activation: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
        }
    }
}
impl LogRegistry {
    pub fn process() -> &'static Self {
        static REGISTRY: OnceLock<LogRegistry> = OnceLock::new();
        REGISTRY.get_or_init(Self::new)
    }
    pub fn check(configs: &[LogConfig]) -> crate::config::Result<()> {
        let mut keys: Vec<(Key, &crate::config::source::Origin)> =
            Vec::with_capacity(configs.len().min(64));
        for (index, config) in configs.iter().enumerate() {
            if index >= 64 {
                return Err(config_error(&capacity(), &config.origin));
            }
            let file = PreparedLogFile::prepare(config)
                .map_err(|error| config_error(&error, &config.origin))?;
            let key =
                Key::from_file(&file).map_err(|error| config_error(&error, &config.origin))?;
            if let Some((_, previous)) = keys.iter().find(|(previous, _)| previous.conflicts(&key))
            {
                return Err(crate::config::ConfigError::new(
                    "LOG_LIMIT",
                    "access-log",
                    "conflicting file log targets",
                    &config.origin,
                )
                .related(previous));
            }
            keys.push((key, &config.origin));
        }
        Ok(())
    }
    pub fn prepare(&self, configs: &[LogConfig]) -> io::Result<PreparedLogSinks> {
        if configs.len() > 64 {
            return Err(io::Error::other("LOG_CAPACITY"));
        }
        let mut items = Vec::with_capacity(configs.len());
        let mut keys: Vec<Key> = Vec::with_capacity(configs.len());
        for config in configs {
            let file = PreparedLogFile::prepare(config)?;
            let key = Key::from_file(&file)?;
            if keys.iter().any(|previous| previous.conflicts(&key)) {
                return Err(io::Error::other("LOG_LIMIT"));
            }
            keys.push(key.clone());
            let lookup = self.lookup(&key)?;
            let reuse = lookup.sink;
            let partial_dirty = lookup
                .partial_dirty
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
            let disk = match lookup.disk {
                Some(disk) => disk,
                None => Arc::new(
                    LogBudget::process()
                        .reserve_disk_target()
                        .ok_or_else(capacity)?,
                ),
            };
            let reserved = if reuse.is_none() {
                let budget = LogBudget::process();
                Some(Reserved {
                    site: budget.reserve_site().ok_or_else(capacity)?,
                    sink: budget.reserve_sink().ok_or_else(capacity)?,
                    worker: budget.reserve_worker().ok_or_else(capacity)?,
                })
            } else {
                None
            };
            items.push(PreparedItem {
                key,
                file,
                reuse,
                reserved,
                retain_days: config.retain_days,
                disk,
                partial_dirty,
            });
        }
        Ok(PreparedLogSinks {
            registry: self.clone(),
            items,
        })
    }
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout.min(Duration::from_secs(5));
        let (sinks, workers): (Vec<_>, Vec<_>) = {
            let entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                entries
                    .values()
                    .filter_map(|entry| entry.sink.upgrade())
                    .collect(),
                entries.values().map(|entry| entry.worker.clone()).collect(),
            )
        };
        let mut success = true;
        for sink in &sinks {
            if let Ok(worker) = sink.worker.try_lock() {
                worker.request_stop();
            } else {
                success = false;
            }
        }
        for sink in sinks {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Ok(mut worker) = sink.worker.try_lock() {
                success &= worker.shutdown(remaining);
            } else {
                success = false;
            }
        }
        while workers.iter().any(|worker| worker.strong_count() != 0) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        success
    }
    fn lookup(&self, key: &Key) -> io::Result<Lookup> {
        let mut entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, entry| {
            entry.published || entry.sink.strong_count() != 0 || entry.worker.strong_count() != 0
        });
        for (other, entry) in entries.iter() {
            if !other.conflicts(key) {
                continue;
            }
            if other != key {
                return Err(io::Error::other("LOG_LIMIT"));
            }
            if !entry.published {
                return Err(io::Error::other("BUSY"));
            }
            if entry.worker.strong_count() == 0 && entry.sink.strong_count() == 0 {
                return Ok(Lookup {
                    sink: None,
                    disk: Some(Arc::clone(&entry.disk)),
                    partial_dirty: Some(Arc::clone(&entry.partial_dirty)),
                });
            }
            if entry.worker.strong_count() == 0 {
                return Err(io::Error::other("BUSY"));
            }
            return entry
                .sink
                .upgrade()
                .map(|sink| Lookup {
                    sink: Some(sink),
                    disk: Some(Arc::clone(&entry.disk)),
                    partial_dirty: Some(Arc::clone(&entry.partial_dirty)),
                })
                .ok_or_else(|| io::Error::other("BUSY"));
        }
        Ok(Lookup {
            sink: None,
            disk: None,
            partial_dirty: None,
        })
    }
}
pub fn config_error(
    error: &io::Error,
    origin: &crate::config::source::Origin,
) -> crate::config::ConfigError {
    let code = match error.to_string().as_str() {
        "LOG_LIMIT" => "LOG_LIMIT",
        "LOG_CAPACITY" => "LOG_CAPACITY",
        "BUSY" => "BUSY",
        _ => "RESOURCE_IO",
    };
    crate::config::ConfigError::new(code, "access-log", "cannot prepare file logging", origin)
}
fn capacity() -> io::Error {
    io::Error::other("LOG_CAPACITY")
}
struct Reserved {
    site: Arc<SiteBudget>,
    sink: Credit,
    worker: Credit,
}
struct PreparedItem {
    key: Key,
    file: PreparedLogFile,
    reuse: Option<Arc<LogSink>>,
    reserved: Option<Reserved>,
    retain_days: u8,
    disk: Arc<Credit>,
    partial_dirty: Arc<AtomicBool>,
}
impl std::fmt::Debug for LogSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSink").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for PreparedLogSinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedLogSinks")
            .field("count", &self.items.len())
            .finish()
    }
}
impl std::fmt::Debug for ActivatedLogSinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivatedLogSinks")
            .field("count", &self.sinks.len())
            .finish()
    }
}

pub struct PreparedLogSinks {
    registry: LogRegistry,
    items: Vec<PreparedItem>,
}

pub struct LogSink {
    queue: LogQueue,
    worker: Mutex<LogWorker>,
    file: Arc<Mutex<ActivatedLogFile>>,
    _credit: Arc<Credit>,
    dropped: Arc<AtomicU64>,
    retain_days: Arc<AtomicU8>,
}
impl LogSink {
    pub fn submit(
        &self,
        record: &FileLogRecord<'_>,
        policy: RotationPolicy,
    ) -> Result<(), LogDropped> {
        let result = self.queue.submit(record, policy);
        if result.is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown(timeout)
    }
}

pub struct ActivatedLogSinks {
    registry: LogRegistry,
    sinks: Vec<Arc<LogSink>>,
    created: Vec<(Key, Arc<LogSink>, Option<Entry>)>,
    policies: Vec<(Arc<LogSink>, u8)>,
    _gate: tokio::sync::OwnedSemaphorePermit,
    committed: bool,
}
impl PreparedLogSinks {
    pub fn activate(self) -> io::Result<ActivatedLogSinks> {
        let gate = Arc::clone(&self.registry.inner.activation)
            .try_acquire_owned()
            .map_err(|_| io::Error::other("BUSY"))?;
        let mut result = ActivatedLogSinks {
            registry: self.registry,
            sinks: Vec::new(),
            created: Vec::new(),
            policies: Vec::new(),
            _gate: gate,
            committed: false,
        };
        for item in self.items {
            let known = result.registry.lookup(&item.key)?.sink.or(item.reuse);
            if let Some(sink) = known {
                let file = sink.file.try_lock().map_err(|_| io::Error::other("BUSY"))?;
                if !file.current_identity_matches()? {
                    return Err(io::Error::other("LOG_LIMIT"));
                }
                item.file.revalidate()?;
                drop(file);
                result.policies.push((Arc::clone(&sink), item.retain_days));
                result.sinks.push(sink);
                continue;
            }
            let reserved = item.reserved.ok_or_else(capacity)?;
            {
                let entries = result
                    .registry
                    .inner
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !entries.contains_key(&item.key) && entries.len() >= 64 {
                    return Err(capacity());
                }
            }
            let monitor = item.file.rollback_monitor()?;
            let activated_file = match item.file.activate_tracked(Arc::clone(&item.partial_dirty)) {
                Ok(file) => file,
                Err((error, published)) => {
                    if published {
                        result.retain_uncertain(
                            &item.key,
                            &item.disk,
                            &item.partial_dirty,
                            &monitor,
                        );
                    }
                    return Err(error);
                }
            };
            let created_file = activated_file.created_by_candidate();
            let file = Arc::new(Mutex::new(activated_file));
            let output = Arc::clone(&file);
            let sink_credit = Arc::new(reserved.sink);
            let worker_credit = Arc::clone(&sink_credit);
            let lease = Arc::new(());
            let worker_lease = Arc::clone(&lease);
            let lifetime_lease = Arc::clone(&lease);
            let lifetime_credit = Arc::clone(&sink_credit);
            let dropped = Arc::new(AtomicU64::new(0));
            let worker_dropped = Arc::clone(&dropped);
            let retain_days = Arc::new(AtomicU8::new(item.retain_days));
            let maintenance_days = Arc::clone(&retain_days);
            let maintenance_file = Arc::clone(&file);
            let mut reported = 0;
            let mut last_diagnostic = None::<Instant>;
            let started = LogWorker::start_reserved_with_maintenance(
                reserved.worker,
                reserved.site,
                move |record| {
                    let _hold = (&worker_credit, &worker_lease);
                    let result = output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .write_record(&record.bytes, record.rotation);
                    if result.is_err() {
                        worker_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    let total = worker_dropped.load(Ordering::Relaxed);
                    if total != reported
                        && last_diagnostic
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
                    {
                        use std::io::Write;
                        let _ = writeln!(io::stderr().lock(), "LOG_DROPPED");
                        last_diagnostic = Some(Instant::now());
                        reported = total;
                    }
                },
                move || {
                    let result = maintenance_file
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .cleanup_expired(maintenance_days.load(Ordering::Relaxed));
                    if result.is_err() {
                        use std::io::Write;
                        let _ = writeln!(io::stderr().lock(), "LOG_LIMIT");
                    }
                },
                Duration::from_secs(60),
                (lifetime_credit, lifetime_lease),
            );
            let (worker, queue) = match started {
                Ok(started) => started,
                Err(error) => {
                    drop(file);
                    if created_file {
                        result.retain_uncertain(
                            &item.key,
                            &item.disk,
                            &item.partial_dirty,
                            &monitor,
                        );
                    }
                    return Err(error);
                }
            };
            drop(monitor);
            let sink = Arc::new(LogSink {
                queue,
                worker: Mutex::new(worker),
                file,
                _credit: sink_credit,
                dropped,
                retain_days,
            });
            let previous = result
                .registry
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    item.key.clone(),
                    Entry {
                        sink: Arc::downgrade(&sink),
                        worker: Arc::downgrade(&lease),
                        disk: item.disk,
                        partial_dirty: item.partial_dirty,
                        published: false,
                    },
                );
            result.created.push((item.key, Arc::clone(&sink), previous));
            result.policies.push((Arc::clone(&sink), item.retain_days));
            result.sinks.push(sink);
        }
        Ok(result)
    }
}
impl ActivatedLogSinks {
    fn retain_uncertain(
        &self,
        key: &Key,
        disk: &Arc<Credit>,
        partial_dirty: &Arc<AtomicBool>,
        monitor: &super::log_directory::LogDirectory,
    ) {
        if target_definitely_absent(monitor) {
            return;
        }
        let mut entries = self
            .registry
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.entry(key.clone()).or_insert_with(|| Entry {
            sink: Weak::new(),
            worker: Weak::new(),
            disk: Arc::clone(disk),
            partial_dirty: Arc::clone(partial_dirty),
            published: true,
        });
    }
    pub fn sinks(&self) -> &[Arc<LogSink>] {
        &self.sinks
    }
    pub fn commit(mut self) {
        for (sink, days) in &self.policies {
            sink.retain_days.store(*days, Ordering::Release);
        }
        for (_, sink, _) in &self.created {
            sink.file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .commit();
        }
        {
            let mut entries = self
                .registry
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (key, _, _) in &self.created {
                if let Some(entry) = entries.get_mut(key) {
                    entry.published = true;
                }
            }
        }
        for (_, sink, _) in &self.created {
            sink.worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .commit();
        }
        self.committed = true;
    }
}
impl Drop for ActivatedLogSinks {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut removed = Vec::with_capacity(self.created.len());
        for (_, sink, _) in &self.created {
            let _ = sink
                .worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .shutdown(Duration::from_secs(5));
            removed.push(
                sink.file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .rollback(),
            );
        }
        let mut entries = self
            .registry
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ((key, _, previous), removed) in self.created.iter_mut().zip(removed) {
            if let Some(previous) = previous.take() {
                entries.insert(key.clone(), previous);
            } else if removed {
                entries.remove(key);
            } else if let Some(entry) = entries.get_mut(key) {
                entry.published = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::source::Origin,
        observability::file_config::{LogFields, parse_access_log},
    };
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn config(path: &Path) -> LogConfig {
        let text = format!(
            "access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1",
            path.display()
        );
        let doc = kdl::KdlDocument::parse_v2(&text).unwrap();
        parse_access_log(&doc.nodes()[0], &Origin::file(Path::new("/tmp/config.kdl"))).unwrap()
    }

    #[test]
    fn blocked_file_worker_shutdown_emits_timeout_without_extending_budget() {
        const CHILD: &str = "RILL_LOG_SHUTDOWN_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("rill-log-shutdown-{}-{unique}", std::process::id()));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "observability::log_registry::tests::blocked_file_worker_shutdown_emits_timeout_without_extending_budget", "--test-threads=1"])
                .env(CHILD, "1").env("RILL_LOG_SHUTDOWN_TEST_DIR", &root).output().unwrap();
            fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("\"event\":\"LOG_TIMEOUT\""));
            return;
        }
        let root =
            std::path::PathBuf::from(std::env::var_os("RILL_LOG_SHUTDOWN_TEST_DIR").unwrap());
        let path = root.join("access.log");
        let config = config(&path);
        let owner = crate::observability::owner::Observability::new().unwrap();
        let active = LogRegistry::process()
            .prepare(std::slice::from_ref(&config))
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&active.sinks()[0]);
        active.commit();
        let _stalled = sink.file.lock().unwrap();
        let record = FileLogRecord {
            fields: LogFields::default(),
            timestamp: 1,
            site: Some("site"),
            route: None,
            method: "GET",
            status: Some(200),
            duration_ms: 0,
            bytes: None,
            uri: None,
            client_ip: None,
        };
        sink.submit(&record, RotationPolicy::from(&config)).unwrap();
        let start = Instant::now();
        owner.shutdown();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_millis(6500),
            "{elapsed:?}"
        );
        std::process::exit(0);
    }

    #[test]
    fn blocked_stderr_does_not_extend_shutdown_budget() {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use nix::unistd::{pipe2, write};
        use std::process::{Command, Stdio};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rill-log-blocked-stderr-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (read, write_end) = pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).unwrap();
        while write(&write_end, &[b'x'; 4096]).is_ok() {}
        fcntl(&write_end, FcntlArg::F_SETFL(OFlag::empty())).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "observability::log_registry::tests::blocked_file_worker_shutdown_emits_timeout_without_extending_budget", "--test-threads=1"])
            .env("RILL_LOG_SHUTDOWN_TEST_CHILD", "1")
            .env("RILL_LOG_SHUTDOWN_TEST_DIR", &root)
            .stdout(Stdio::null()).stderr(Stdio::from(write_end)).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(7);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("shutdown exceeded deadline with blocked stderr");
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(read);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saturated_queue_drops_without_waiting_for_real_file_worker() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rill-log-queue-full-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("access.log");
        let config = config(&path);
        let registry = LogRegistry::new();
        let active = registry
            .prepare(std::slice::from_ref(&config))
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&active.sinks()[0]);
        active.commit();
        let stalled = sink.file.lock().unwrap();
        let record = FileLogRecord {
            fields: LogFields::default(),
            timestamp: 1,
            site: Some("site"),
            route: None,
            method: "GET",
            status: Some(200),
            duration_ms: 0,
            bytes: None,
            uri: None,
            client_ip: None,
        };
        let policy = RotationPolicy::from(&config);
        let start = Instant::now();
        for _ in 0..1024 {
            assert_eq!(sink.submit(&record, policy), Ok(()));
        }
        assert_eq!(sink.submit(&record, policy), Err(LogDropped));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(sink.dropped(), 1);
        drop(stalled);
        assert!(sink.shutdown(Duration::from_secs(5)));
        drop(sink);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_write_quarantine_survives_retirement_and_same_target_reopen() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rill-log-quarantine-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let first = root.join("first.log");
        let second = root.join("second.log");
        let registry = LogRegistry::new();
        let active = registry
            .prepare(&[config(&first)])
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&active.sinks()[0]);
        active.commit();
        fs::write(&first, b"{\"ok\":1}\n{\"partial\"").unwrap();
        let key = Key::from_file(&PreparedLogFile::prepare(&config(&first)).unwrap()).unwrap();
        let dirty = Arc::clone(
            &registry
                .inner
                .entries
                .lock()
                .unwrap()
                .get(&key)
                .unwrap()
                .partial_dirty,
        );
        dirty.store(true, Ordering::Release);
        assert!(sink.shutdown(Duration::from_secs(2)));
        drop(sink);
        let other = registry
            .prepare(&[config(&second)])
            .unwrap()
            .activate()
            .unwrap();
        let other_sink = Arc::clone(&other.sinks()[0]);
        other.commit();
        assert!(other_sink.shutdown(Duration::from_secs(2)));
        drop(other_sink);
        let candidate = registry.prepare(&[config(&first)]).unwrap();
        assert!(candidate.activate().is_err());
        assert_eq!(fs::read(&first).unwrap(), b"{\"ok\":1}\n{\"partial\"");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_candidate_cleanup_retains_disk_credit_when_unlink_denied() {
        if nix::unistd::geteuid().as_raw() == 0 {
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rill-log-rollback-denied-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.join("access.log");
        let registry = LogRegistry::new();
        let activated = registry
            .prepare(&[config(&target)])
            .unwrap()
            .activate()
            .unwrap();
        assert!(target.exists());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        drop(activated);
        assert!(target.exists());
        assert_eq!(registry.inner.entries.lock().unwrap().len(), 1);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let reopened = registry
            .prepare(&[config(&target)])
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&reopened.sinks()[0]);
        reopened.commit();
        assert!(sink.shutdown(Duration::from_secs(2)));
        drop(sink);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retired_disk_targets_remain_bounded_across_generations() {
        const CHILD: &str = "RILL_LOG_RETIRED_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "observability::log_registry::tests::retired_disk_targets_remain_bounded_across_generations", "--test-threads=1"])
                .env(CHILD, "1").status().unwrap();
            assert!(status.success());
            return;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rill-log-retired-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = LogRegistry::new();
        for index in 0..64 {
            let path = root.join(format!("log-{index}"));
            let activated = registry
                .prepare(&[config(&path)])
                .unwrap()
                .activate()
                .unwrap();
            let sink = Arc::clone(&activated.sinks()[0]);
            activated.commit();
            assert!(sink.shutdown(Duration::from_secs(2)));
            drop(sink);
            assert!(path.exists());
        }
        let extra = root.join("overflow");
        assert!(registry.prepare(&[config(&extra)]).is_err());
        assert!(!extra.exists());
        let existing = registry
            .prepare(&[config(&root.join("log-0"))])
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&existing.sinks()[0]);
        existing.commit();
        assert!(sink.shutdown(Duration::from_secs(2)));
        drop(sink);
        let retired = root.join("log-0");
        fs::remove_file(&retired).unwrap();
        symlink(root.join("log-1"), &retired).unwrap();
        assert!(registry.prepare(&[config(&retired)]).is_err());
        fs::remove_file(&retired).unwrap();
        fs::write(&retired, b"replacement\n").unwrap();
        let reopened = registry
            .prepare(&[config(&retired)])
            .unwrap()
            .activate()
            .unwrap();
        let sink = Arc::clone(&reopened.sinks()[0]);
        reopened.commit();
        assert!(sink.shutdown(Duration::from_secs(2)));
        drop(sink);
        assert_eq!(fs::read(&retired).unwrap(), b"replacement\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_rolls_back_failed_candidates_and_reuses_committed_sink() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rill-log-registry-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let a = root.join("a");
        let b = root.join("b");
        let registry = LogRegistry::new();
        let candidate = registry.prepare(&[config(&a), config(&b)]).unwrap();
        assert!(!a.exists() && !b.exists());
        symlink(&a, &b).unwrap();
        assert!(candidate.activate().is_err());
        assert!(registry.inner.entries.lock().unwrap().is_empty());
        assert!(!a.exists());
        fs::remove_file(&b).unwrap();
        assert!(
            registry
                .prepare(&[config(&a), config(&root.join("a.1"))])
                .is_err()
        );
        let activated = registry.prepare(&[config(&a)]).unwrap().activate().unwrap();
        let sink = Arc::clone(&activated.sinks()[0]);
        activated.commit();
        let next = registry.prepare(&[config(&a)]).unwrap().activate().unwrap();
        assert!(Arc::ptr_eq(&sink, &next.sinks()[0]));
        next.commit();
        let failing_reload = registry.prepare(&[config(&a), config(&b)]).unwrap();
        symlink(&a, &b).unwrap();
        assert!(failing_reload.activate().is_err());
        fs::remove_file(&b).unwrap();
        let record = FileLogRecord {
            fields: LogFields::default(),
            timestamp: 1,
            site: Some("example"),
            route: None,
            method: "GET",
            status: Some(200),
            duration_ms: 0,
            bytes: None,
            uri: None,
            client_ip: None,
        };
        sink.submit(&record, RotationPolicy::from(&config(&a)))
            .unwrap();
        for _ in 0..100 {
            if fs::metadata(&a).unwrap().len() > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(fs::read_to_string(&a).unwrap().contains("example"));
        assert!(sink.shutdown(Duration::from_secs(2)));
        drop(sink);
        fs::remove_dir_all(root).unwrap();
    }
}
