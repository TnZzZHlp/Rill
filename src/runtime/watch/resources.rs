use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};

use crate::config::dependencies::{DependencyPlan, FailureBaseline};
use crate::config::source::Origin;
use crate::config::{ConfigError, Result};

const MAX_DIRECTORIES: usize = 2048;
const MAX_READ_BATCHES: usize = 32;

#[derive(Debug, Default)]
struct Filter {
    names: BTreeSet<OsString>,
    patterns: BTreeSet<String>,
}

impl Filter {
    fn matches(&self, name: &OsStr) -> bool {
        if self.names.contains(name) {
            return true;
        }
        let Some(name) = name.to_str() else {
            return !self.patterns.is_empty();
        };
        let origin = Origin::file(Path::new("/"));
        self.patterns.iter().any(|pattern| {
            crate::config::glob::matches_component(pattern, name, &origin).unwrap_or(false)
        })
    }

    fn extend(&mut self, other: Self) {
        self.names.extend(other.names);
        self.patterns.extend(other.patterns);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DrainOutcome {
    pub dirty: bool,
    pub needs_rebuild: bool,
    pub code: Option<&'static str>,
}

#[derive(Debug)]
pub struct WatchResources {
    inotify: Inotify,
    filters: BTreeMap<WatchDescriptor, Filter>,
    needs_rebuild: bool,
    pending_dirty: bool,
    pending_code: Option<&'static str>,
}

fn error(code: &'static str, reason: &'static str) -> ConfigError {
    ConfigError::new(code, "watch", reason, &Origin::file(Path::new("/")))
}

fn directory_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Ok(false),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(_) => Err(error("RESOURCE_IO", "cannot inspect watch directory")),
    }
}

fn directory_filter<'a>(
    filters: &'a mut BTreeMap<PathBuf, Filter>,
    directory: &Path,
) -> Result<&'a mut Filter> {
    if !filters.contains_key(directory) && filters.len() >= MAX_DIRECTORIES {
        return Err(error("CONFIG_LIMIT", "watch directory limit exceeded"));
    }
    Ok(filters.entry(directory.to_path_buf()).or_default())
}

fn add_exact(filters: &mut BTreeMap<PathBuf, Filter>, path: &Path) -> Result<()> {
    let mut child = path;
    while let Some(parent) = child.parent() {
        let Some(name) = child.file_name() else {
            break;
        };
        if directory_exists(parent)? {
            directory_filter(filters, parent)?
                .names
                .insert(name.to_os_string());
            return Ok(());
        }
        child = parent;
    }
    Ok(())
}

impl WatchResources {
    pub fn prepare(plan: &DependencyPlan) -> Result<Self> {
        Self::prepare_filtered(Some(plan), plan)
    }

    pub fn prepare_with_failure(active: &DependencyPlan, failure: &DependencyPlan) -> Result<Self> {
        let merged = active.merge_recent_failure(failure)?;
        let mut resources = Self::prepare_filtered(None, &merged)?;
        match failure.failure_baseline() {
            FailureBaseline::Stable => {}
            FailureBaseline::Changed => resources.pending_dirty = true,
            FailureBaseline::Unavailable => {
                resources.pending_dirty = true;
                resources.needs_rebuild = true;
                resources.pending_code = Some("RESOURCE_IO");
            }
        }
        Ok(resources)
    }

    fn prepare_filtered(active: Option<&DependencyPlan>, plan: &DependencyPlan) -> Result<Self> {
        plan.validate_bounds()?;
        let mut directories = BTreeMap::<PathBuf, Filter>::new();
        for path in plan.paths.keys() {
            add_exact(&mut directories, path)?;
        }
        for glob in &plan.glob_directories {
            add_exact(&mut directories, &glob.directory)?;
            if directory_exists(&glob.directory)? {
                directory_filter(&mut directories, &glob.directory)?
                    .patterns
                    .insert(glob.pattern.clone());
            }
        }
        if directories.len() > MAX_DIRECTORIES {
            return Err(error("CONFIG_LIMIT", "watch directory limit exceeded"));
        }
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
            .map_err(|_| error("RESOURCE_IO", "cannot create watch resources"))?;
        let mask = AddWatchFlags::IN_MODIFY
            | AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_MOVED_FROM
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_ATTRIB
            | AddWatchFlags::IN_DELETE_SELF
            | AddWatchFlags::IN_MOVE_SELF
            | AddWatchFlags::IN_ONLYDIR;
        let mut filters = BTreeMap::<WatchDescriptor, Filter>::new();
        let mut identities = Vec::new();
        for (directory, filter) in directories {
            let before = fs::metadata(&directory)
                .map_err(|_| error("RESOURCE_IO", "cannot inspect watch directory"))?;
            let descriptor = inotify
                .add_watch(directory.as_path(), mask)
                .map_err(|_| error("RESOURCE_IO", "cannot register watch directory"))?;
            filters.entry(descriptor).or_default().extend(filter);
            identities.push((directory, before.dev(), before.ino(), before.mode()));
        }
        for (directory, dev, ino, mode) in identities {
            let after = fs::metadata(&directory).map_err(|_| {
                error(
                    "RESOURCE_CHANGED",
                    "watch directory changed during registration",
                )
            })?;
            if after.dev() != dev || after.ino() != ino || after.mode() != mode {
                return Err(error(
                    "RESOURCE_CHANGED",
                    "watch directory changed during registration",
                ));
            }
        }
        if active.is_some_and(|plan| !plan.unchanged()) {
            return Err(error(
                "RESOURCE_CHANGED",
                "dependencies changed during watch registration",
            ));
        }
        Ok(Self {
            inotify,
            filters,
            needs_rebuild: false,
            pending_dirty: false,
            pending_code: None,
        })
    }

    pub fn drain(&mut self) -> DrainOutcome {
        let mut dirty = std::mem::take(&mut self.pending_dirty);
        let mut code = self.pending_code.take();
        for _ in 0..MAX_READ_BATCHES {
            let events = match self.inotify.read_events() {
                Ok(events) => events,
                Err(Errno::EAGAIN) => {
                    return DrainOutcome {
                        dirty,
                        needs_rebuild: self.needs_rebuild,
                        code,
                    };
                }
                Err(_) => {
                    self.needs_rebuild = true;
                    return DrainOutcome {
                        dirty: true,
                        needs_rebuild: true,
                        code: Some("RESOURCE_IO"),
                    };
                }
            };
            for event in events {
                if event.mask.intersects(
                    AddWatchFlags::IN_Q_OVERFLOW
                        | AddWatchFlags::IN_IGNORED
                        | AddWatchFlags::IN_UNMOUNT
                        | AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF,
                ) {
                    self.needs_rebuild = true;
                    dirty = true;
                    code = Some(if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                        "WATCH_OVERFLOW"
                    } else {
                        "WATCH_INVALIDATED"
                    });
                } else if let Some(filter) = self.filters.get(&event.wd) {
                    dirty |= event
                        .name
                        .as_deref()
                        .is_some_and(|name| filter.matches(name));
                } else {
                    self.needs_rebuild = true;
                    dirty = true;
                    code = Some("WATCH_INVALIDATED");
                }
            }
        }
        DrainOutcome {
            dirty: true,
            needs_rebuild: self.needs_rebuild,
            code,
        }
    }

    pub fn needs_rebuild(&self) -> bool {
        self.needs_rebuild
    }
}
