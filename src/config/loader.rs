use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kdl::{KdlDocument, KdlNode};

use super::dependencies::{DependencyPlan, Fingerprint, PathFingerprint};
use super::error::{ConfigError, Result};
use super::glob;
use super::source::{Origin, Source};

pub struct LoadedNode {
    pub node: KdlNode,
    pub origin: Origin,
}

pub struct LoadedConfig {
    pub entry: PathBuf,
    pub nodes: Vec<LoadedNode>,
}

struct FileState {
    path: PathBuf,
    metadata: Metadata,
    origin: Origin,
}

struct Loader<'a> {
    dependencies: &'a mut DependencyPlan,
    seen: HashMap<(u64, u64), Origin>,
    stack: Vec<(u64, u64)>,
    files: Vec<FileState>,
    nodes: Vec<LoadedNode>,
    total_bytes: usize,
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

impl Loader<'_> {
    fn visit(&mut self, path: &Path, reference: &Origin, references: &[Origin]) -> Result<()> {
        if self.stack.len() >= 64 || self.files.len() >= 512 {
            return Err(ConfigError::new(
                "CONFIG_LIMIT",
                "import",
                "configuration import limit exceeded",
                reference,
            ));
        }
        self.dependencies.track_path(path, reference)?;
        let consumed_path = path.to_path_buf();
        let path = fs::canonicalize(path).map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "import",
                "cannot resolve configuration file",
                reference,
            )
        })?;
        self.dependencies.track_path(&path, reference)?;
        let before_open = fs::metadata(&path).map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "import",
                "cannot inspect configuration path",
                reference,
            )
        })?;
        if !before_open.is_file() {
            return Err(ConfigError::new(
                "RESOURCE_IO",
                "import",
                "configuration input is not a regular file",
                reference,
            ));
        }
        let mut file = File::open(&path).map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "import",
                "cannot open configuration file",
                reference,
            )
        })?;
        let metadata = file.metadata().map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "import",
                "cannot inspect configuration file",
                reference,
            )
        })?;
        if !same_file(&before_open, &metadata)
            || (self.dependencies.enabled()
                && self.dependencies.paths.get(&path)
                    != Some(&PathFingerprint::Present(Fingerprint::from_metadata(
                        &metadata,
                    ))))
        {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                "import",
                "configuration changed while opening",
                reference,
            ));
        }
        if !metadata.is_file() {
            return Err(ConfigError::new(
                "RESOURCE_IO",
                "import",
                "configuration input is not a regular file",
                reference,
            ));
        }
        self.dependencies
            .record_consumed(&consumed_path, &metadata, reference)?;
        let identity = (metadata.dev(), metadata.ino());
        if let Some(first) = self.seen.get(&identity) {
            let code = if self.stack.contains(&identity) {
                "IMPORT_CYCLE"
            } else {
                "DUPLICATE"
            };
            return Err(ConfigError::new(
                code,
                "import",
                "configuration file already imported",
                reference,
            )
            .related(first));
        }
        let mut text = String::new();
        file.by_ref()
            .take(1024 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|_| {
                ConfigError::new(
                    "RESOURCE_IO",
                    "import",
                    "cannot read UTF-8 configuration",
                    reference,
                )
            })?;
        self.total_bytes += text.len();
        if text.len() > 1024 * 1024 || self.total_bytes > 8 * 1024 * 1024 {
            return Err(ConfigError::new(
                "CONFIG_LIMIT",
                "import",
                "configuration size limit exceeded",
                reference,
            ));
        }
        let after = file.metadata().map_err(|_| {
            ConfigError::new(
                "RESOURCE_IO",
                "import",
                "cannot inspect configuration after reading",
                reference,
            )
        })?;
        if !same_file(&metadata, &after) {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                "import",
                "configuration changed while reading",
                reference,
            ));
        }
        let source = Arc::new(Source {
            path: path.clone(),
            text,
        });
        let file_origin = Origin {
            source: source.clone(),
            offset: 0,
            length: 0,
            references: references.to_vec(),
        };
        super::preflight::check(&file_origin)?;
        let document = KdlDocument::parse_v2(&source.text).map_err(|error| {
            let offset = error
                .diagnostics
                .first()
                .map_or(0, |diagnostic| diagnostic.span.offset());
            let origin = Origin {
                offset,
                ..file_origin.clone()
            };
            let reason = error
                .diagnostics
                .first()
                .and_then(|diagnostic| diagnostic.message.as_deref())
                .unwrap_or("invalid KDL v2 syntax");
            ConfigError::new("CONFIG_SYNTAX", "document", reason, &origin)
        })?;
        self.seen.insert(identity, reference.clone());
        self.stack.push(identity);
        self.files.push(FileState {
            path,
            metadata,
            origin: file_origin.clone(),
        });
        for node in document.nodes() {
            let origin = Origin {
                source: source.clone(),
                offset: node.span().offset(),
                length: node.span().len(),
                references: references.to_vec(),
            };
            if node.name().value() == "import" {
                if node.ty().is_some()
                    || node.children().is_some()
                    || node.entries().len() != 1
                    || node.entries()[0].name().is_some()
                    || node.entries()[0].ty().is_some()
                {
                    return Err(ConfigError::new(
                        "CONFIG_TYPE",
                        "import",
                        "expected one path argument without children or annotations",
                        &origin,
                    ));
                }
                let value = node.entries()[0]
                    .value()
                    .as_string()
                    .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                    .ok_or_else(|| {
                        ConfigError::new(
                            "CONFIG_TYPE",
                            "import",
                            "expected a nonempty import path without control characters",
                            &origin,
                        )
                    })?;
                let imported = origin.relative_path(value);
                let mut chain = references.to_vec();
                let mut site = origin.clone();
                site.references.clear();
                chain.push(site);
                for path in glob::expand_tracked(&imported, &origin, self.dependencies)? {
                    self.visit(&path, &origin, &chain)?;
                }
            } else {
                self.dependencies.track_tls_node(node, &origin)?;
                self.nodes.push(LoadedNode {
                    node: node.clone(),
                    origin,
                });
            }
        }
        self.stack.pop();
        Ok(())
    }
}

pub fn load(entry: &Path) -> Result<LoadedConfig> {
    load_tracked(entry, &mut DependencyPlan::disabled())
}

pub fn load_tracked(entry: &Path, dependencies: &mut DependencyPlan) -> Result<LoadedConfig> {
    let absolute = std::path::absolute(entry).map_err(|_| {
        ConfigError::new(
            "RESOURCE_IO",
            "config",
            "cannot resolve entry path",
            &Origin::file(entry),
        )
    })?;
    let mut loader = Loader {
        dependencies,
        seen: HashMap::new(),
        stack: Vec::new(),
        files: Vec::new(),
        nodes: Vec::new(),
        total_bytes: 0,
    };
    loader.visit(&absolute, &Origin::file(&absolute), &[])?;
    for file in &loader.files {
        let metadata = fs::metadata(&file.path).map_err(|_| {
            ConfigError::new(
                "RESOURCE_CHANGED",
                "import",
                "configuration disappeared during load",
                &file.origin,
            )
        })?;
        if !same_file(&file.metadata, &metadata) {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                "import",
                "configuration changed during load",
                &file.origin,
            ));
        }
    }
    Ok(LoadedConfig {
        entry: absolute,
        nodes: loader.nodes,
    })
}
