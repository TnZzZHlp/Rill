use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::error::{ConfigError, Result};
use super::source::Origin;

const MAX_ITEMS: usize = 8192;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}

impl Fingerprint {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GlobDirectory {
    pub directory: PathBuf,
    pub pattern: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathFingerprint {
    Present(Fingerprint),
    Missing,
    NotDirectory,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureBaseline {
    Stable,
    Changed,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumedFingerprint {
    pub fingerprint: Fingerprint,
    pub directory: bool,
}

#[derive(Clone, Debug)]
pub struct DependencyPlan {
    pub paths: BTreeMap<PathBuf, PathFingerprint>,
    pub consumed: BTreeMap<PathBuf, ConsumedFingerprint>,
    pub glob_directories: BTreeSet<GlobDirectory>,
    pub glob_matches: BTreeMap<GlobDirectory, BTreeSet<std::ffi::OsString>>,
    bytes: usize,
    enabled: bool,
}

impl Default for DependencyPlan {
    fn default() -> Self {
        Self {
            paths: BTreeMap::new(),
            consumed: BTreeMap::new(),
            glob_directories: BTreeSet::new(),
            glob_matches: BTreeMap::new(),
            bytes: 0,
            enabled: true,
        }
    }
}

impl DependencyPlan {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    fn limit(origin: &Origin) -> ConfigError {
        ConfigError::new(
            "CONFIG_LIMIT",
            "watch",
            "dependency tracking limit exceeded",
            origin,
        )
    }

    fn reserve(&mut self, bytes: usize, origin: &Origin) -> Result<()> {
        if bytes > MAX_PATH_BYTES
            || self.paths.len()
                + self.consumed.len()
                + self.glob_directories.len()
                + self.glob_matches.values().map(BTreeSet::len).sum::<usize>()
                >= MAX_ITEMS
            || self
                .bytes
                .checked_add(bytes)
                .is_none_or(|n| n > MAX_TOTAL_BYTES)
        {
            return Err(Self::limit(origin));
        }
        self.bytes += bytes;
        Ok(())
    }

    fn record(&mut self, path: &Path, origin: &Origin) -> Result<Option<Metadata>> {
        if !self.paths.contains_key(path) {
            self.reserve(path.as_os_str().as_bytes().len(), origin)?;
            self.paths
                .insert(path.to_path_buf(), PathFingerprint::Unavailable);
            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    self.paths.insert(
                        path.to_path_buf(),
                        PathFingerprint::Present(Fingerprint::from_metadata(&metadata)),
                    );
                    Ok(Some(metadata))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.paths
                        .insert(path.to_path_buf(), PathFingerprint::Missing);
                    Ok(None)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                    self.paths
                        .insert(path.to_path_buf(), PathFingerprint::NotDirectory);
                    Ok(None)
                }
                Err(_) => Err(ConfigError::new(
                    "RESOURCE_IO",
                    "watch",
                    "cannot inspect dependency path",
                    origin,
                )),
            }
        } else {
            fs::symlink_metadata(path).map(Some).or_else(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) {
                    Ok(None)
                } else {
                    Err(ConfigError::new(
                        "RESOURCE_IO",
                        "watch",
                        "cannot inspect dependency path",
                        origin,
                    ))
                }
            })
        }
    }

    pub fn track_path(&mut self, path: &Path, origin: &Origin) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !path.is_absolute() || path.components().count() > 64 {
            return Err(Self::limit(origin));
        }
        let mut pending: VecDeque<_> = path
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        let mut current = PathBuf::new();
        let mut links = 0;
        let mut steps = 0;
        while let Some(component) = pending.pop_front() {
            steps += 1;
            if steps > 4096 {
                return Err(Self::limit(origin));
            }
            match Path::new(&component).components().next() {
                Some(Component::RootDir) => {
                    current = PathBuf::from("/");
                }
                Some(Component::CurDir) => continue,
                Some(Component::ParentDir) => {
                    current.pop();
                }
                _ => current.push(component),
            }
            if let Some(metadata) = self.record(&current, origin)?
                && metadata.file_type().is_symlink()
            {
                links += 1;
                if links > 40 {
                    return Err(Self::limit(origin));
                }
                let target = fs::read_link(&current).map_err(|_| {
                    ConfigError::new(
                        "RESOURCE_IO",
                        "watch",
                        "cannot read dependency symlink",
                        origin,
                    )
                })?;
                current.pop();
                let mut components: VecDeque<_> = target
                    .components()
                    .map(|c| c.as_os_str().to_os_string())
                    .collect();
                components.append(&mut pending);
                pending = components;
            }
        }
        self.record(path, origin)?;
        Ok(())
    }

    pub fn track_glob(&mut self, directory: &Path, pattern: &str, origin: &Origin) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.track_path(directory, origin)?;
        let item = GlobDirectory {
            directory: directory.to_path_buf(),
            pattern: pattern.to_owned(),
        };
        if !self.glob_directories.contains(&item) {
            self.reserve(
                directory
                    .as_os_str()
                    .as_bytes()
                    .len()
                    .saturating_add(pattern.len()),
                origin,
            )?;
            self.glob_directories.insert(item.clone());
            self.glob_matches.insert(item, BTreeSet::new());
        }
        Ok(())
    }

    pub(crate) fn track_glob_match(
        &mut self,
        directory: &Path,
        pattern: &str,
        name: &std::ffi::OsStr,
        origin: &Origin,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let item = GlobDirectory {
            directory: directory.to_path_buf(),
            pattern: pattern.to_owned(),
        };
        if !self
            .glob_matches
            .get(&item)
            .is_some_and(|items| items.contains(name))
        {
            self.reserve(name.as_bytes().len(), origin)?;
            self.glob_matches
                .get_mut(&item)
                .expect("glob registered")
                .insert(name.to_os_string());
        }
        Ok(())
    }

    pub(crate) fn track_tls_node(&mut self, node: &kdl::KdlNode, origin: &Origin) -> Result<()> {
        if node.name().value() != "tls" {
            return Ok(());
        }
        for child in node.children().into_iter().flat_map(|doc| doc.nodes()) {
            if !matches!(child.name().value(), "cert" | "key") {
                continue;
            }
            if let Some(value) = child
                .entries()
                .first()
                .and_then(|entry| entry.value().as_string())
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            {
                self.track_path(&origin.relative_path(value), origin)?;
            }
        }
        Ok(())
    }

    pub fn record_consumed(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        origin: &Origin,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let item = ConsumedFingerprint {
            fingerprint: Fingerprint::from_metadata(metadata),
            directory: metadata.is_dir(),
        };
        let actual = fs::metadata(path).map_err(|_| {
            ConfigError::new(
                "RESOURCE_CHANGED",
                "watch",
                "consumed dependency changed",
                origin,
            )
        })?;
        if !Self::same_consumed(&item, &actual) {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                "watch",
                "consumed dependency changed",
                origin,
            ));
        }
        if let Some(previous) = self.consumed.get(path) {
            if !Self::same_consumed(previous, metadata) {
                return Err(ConfigError::new(
                    "RESOURCE_CHANGED",
                    "watch",
                    "consumed dependency changed",
                    origin,
                ));
            }
        } else {
            self.reserve(path.as_os_str().as_bytes().len(), origin)?;
            self.consumed.insert(path.to_path_buf(), item);
        }
        Ok(())
    }

    fn same_consumed(expected: &ConsumedFingerprint, actual: &Metadata) -> bool {
        let fingerprint = Fingerprint::from_metadata(actual);
        if expected.directory {
            actual.is_dir()
                && expected.fingerprint.device == fingerprint.device
                && expected.fingerprint.inode == fingerprint.inode
                && expected.fingerprint.mode == fingerprint.mode
        } else {
            expected.fingerprint == fingerprint
        }
    }

    pub fn validate_bounds(&self) -> Result<()> {
        let origin = Origin::file(Path::new("/"));
        let mut count = self
            .paths
            .len()
            .saturating_add(self.consumed.len())
            .saturating_add(self.glob_directories.len());
        let mut bytes = 0usize;
        for path in self.paths.keys() {
            let len = path.as_os_str().as_bytes().len();
            if len > MAX_PATH_BYTES {
                return Err(Self::limit(&origin));
            }
            bytes = bytes.saturating_add(len);
        }
        for path in self.consumed.keys() {
            let len = path.as_os_str().as_bytes().len();
            if len > MAX_PATH_BYTES {
                return Err(Self::limit(&origin));
            }
            bytes = bytes.saturating_add(len);
        }
        for glob in &self.glob_directories {
            let len = glob
                .directory
                .as_os_str()
                .as_bytes()
                .len()
                .saturating_add(glob.pattern.len());
            if len > MAX_PATH_BYTES {
                return Err(Self::limit(&origin));
            }
            bytes = bytes.saturating_add(len);
        }
        for (glob, names) in &self.glob_matches {
            if !self.glob_directories.contains(glob) {
                return Err(Self::limit(&origin));
            }
            count = count.saturating_add(names.len());
            for name in names {
                let len = name.as_bytes().len();
                if len > MAX_PATH_BYTES {
                    return Err(Self::limit(&origin));
                }
                bytes = bytes.saturating_add(len);
            }
        }
        if count > MAX_ITEMS || bytes > MAX_TOTAL_BYTES {
            return Err(Self::limit(&origin));
        }
        Ok(())
    }

    pub fn merge_recent_failure(&self, failure: &Self) -> Result<Self> {
        self.validate_bounds()?;
        failure.validate_bounds()?;
        let mut merged = self.clone();
        let origin = Origin::file(Path::new("/"));
        for (path, fingerprint) in &failure.paths {
            if !merged.paths.contains_key(path) {
                merged.reserve(path.as_os_str().as_bytes().len(), &origin)?;
                merged.paths.insert(path.clone(), fingerprint.clone());
            }
        }
        for (path, consumed) in &failure.consumed {
            if !merged.consumed.contains_key(path) {
                merged.reserve(path.as_os_str().as_bytes().len(), &origin)?;
                merged.consumed.insert(path.clone(), consumed.clone());
            }
        }
        for glob in &failure.glob_directories {
            if !merged.glob_directories.contains(glob) {
                merged.reserve(
                    glob.directory
                        .as_os_str()
                        .as_bytes()
                        .len()
                        .saturating_add(glob.pattern.len()),
                    &origin,
                )?;
                merged.glob_directories.insert(glob.clone());
                merged.glob_matches.insert(glob.clone(), BTreeSet::new());
            }
            if let Some(names) = failure.glob_matches.get(glob) {
                for name in names {
                    merged.track_glob_match(&glob.directory, &glob.pattern, name, &origin)?;
                }
            }
        }
        merged.validate_bounds()?;
        Ok(merged)
    }

    pub fn failure_baseline(&self) -> FailureBaseline {
        use FailureBaseline::{Changed, Stable, Unavailable};
        let mut status = Stable;
        for (path, expected) in &self.paths {
            let current = fs::symlink_metadata(path);
            let result = match (expected, current) {
                (PathFingerprint::Unavailable, _) => Unavailable,
                (PathFingerprint::Present(expected), Ok(actual)) if actual.is_dir() => {
                    if expected.device == actual.dev()
                        && expected.inode == actual.ino()
                        && expected.mode == actual.mode()
                    {
                        Stable
                    } else {
                        Changed
                    }
                }
                (PathFingerprint::Present(expected), Ok(actual)) => {
                    if *expected == Fingerprint::from_metadata(&actual) {
                        Stable
                    } else {
                        Changed
                    }
                }
                (PathFingerprint::Missing, Err(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    Stable
                }
                (PathFingerprint::NotDirectory, Err(error))
                    if error.kind() == std::io::ErrorKind::NotADirectory =>
                {
                    Stable
                }
                (_, Err(error))
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Unavailable
                }
                _ => Changed,
            };
            if result == Unavailable {
                return Unavailable;
            }
            if result == Changed {
                status = Changed;
            }
        }
        for (path, expected) in &self.consumed {
            let result = match fs::metadata(path) {
                Ok(actual)
                    if expected.directory
                        && self
                            .glob_directories
                            .iter()
                            .any(|glob| glob.directory == *path) =>
                {
                    if expected.fingerprint == Fingerprint::from_metadata(&actual) {
                        Stable
                    } else {
                        Changed
                    }
                }
                Ok(actual) => {
                    if Self::same_consumed(expected, &actual) {
                        Stable
                    } else {
                        Changed
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Changed
                }
                Err(_) => Unavailable,
            };
            if result == Unavailable {
                return Unavailable;
            }
            if result == Changed {
                status = Changed;
            }
        }
        for glob in &self.glob_directories {
            if !self.consumed.contains_key(&glob.directory) {
                match fs::metadata(&glob.directory) {
                    Ok(metadata) if metadata.is_dir() => return Unavailable,
                    Err(error)
                        if !matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Unavailable;
                    }
                    _ => {}
                }
            }
        }
        status
    }

    pub fn unchanged(&self) -> bool {
        let paths_match =
            self.paths
                .iter()
                .all(|(path, expected)| match fs::symlink_metadata(path) {
                    Ok(metadata) => match expected {
                        PathFingerprint::Present(expected) if metadata.is_dir() => {
                            expected.device == metadata.dev()
                                && expected.inode == metadata.ino()
                                && expected.mode == metadata.mode()
                        }
                        PathFingerprint::Present(expected) => {
                            *expected == Fingerprint::from_metadata(&metadata)
                        }
                        _ => false,
                    },
                    Err(error) => {
                        *expected == PathFingerprint::Missing
                            && error.kind() == std::io::ErrorKind::NotFound
                    }
                });
        paths_match
            && self.consumed.iter().all(|(path, expected)| {
                fs::metadata(path).is_ok_and(|actual| Self::same_consumed(expected, &actual))
            })
            && self.glob_matches.iter().all(|(glob, expected)| {
                let Ok(entries) = fs::read_dir(&glob.directory) else {
                    return false;
                };
                let origin = Origin::file(&glob.directory);
                let mut actual = BTreeSet::new();
                for entry in entries {
                    let Ok(entry) = entry else {
                        return false;
                    };
                    let name = entry.file_name();
                    let Some(text) = name.to_str() else {
                        return false;
                    };
                    if super::glob::matches_component(&glob.pattern, text, &origin).unwrap_or(false)
                    {
                        actual.insert(name);
                    }
                    if actual.len() > 512 {
                        return false;
                    }
                }
                &actual == expected
            })
    }
}
