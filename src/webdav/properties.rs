use super::{
    model::{DavError, Result, StorageIdentity},
    namespace,
    path::DavPath,
    visibility,
    xml::{self, Name, Node},
};
use crate::static_files::root::Root;
use nix::{
    errno::Errno,
    fcntl::{OFlag, OpenHow, ResolveFlag, openat2, renameat},
    sys::stat::Mode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
};

pub const PROPERTY_LIMIT: usize = 64 * 1024;
const RECORD_LIMIT: usize = 512 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    #[serde(deserialize_with = "path_key")]
    path: String,
    #[serde(deserialize_with = "property_values")]
    properties: Vec<String>,
}
struct LimitedString(usize);
impl<'de> serde::de::DeserializeSeed<'de> for LimitedString {
    type Value = String;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<String, D::Error> {
        struct Text(usize);
        impl serde::de::Visitor<'_> for Text {
            type Value = String;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bounded property text")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<String, E> {
                if value.len() > self.0 {
                    return Err(E::custom("property limit"));
                }
                Ok(value.into())
            }
        }
        deserializer.deserialize_str(Text(self.0))
    }
}
fn path_key<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    serde::de::DeserializeSeed::deserialize(LimitedString(4096), deserializer)
}
fn property_values<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    struct Values;
    impl<'de> serde::de::Visitor<'de> for Values {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded property list")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut properties = Vec::new();
            let mut remaining = PROPERTY_LIMIT;
            while let Some(value) = sequence.next_element_seed(LimitedString(remaining))? {
                if properties.len() == 64 {
                    return Err(serde::de::Error::custom("property limit"));
                }
                remaining -= value.len();
                properties.push(value);
            }
            Ok(properties)
        }
    }
    deserializer.deserialize_seq(Values)
}
#[derive(Debug)]
pub struct Properties {
    root: std::sync::Arc<Root>,
    directory: File,
    _private: visibility::PrivateEntry,
    files: std::sync::Mutex<BTreeMap<String, PrivateProperty>>,
}
#[derive(Debug)]
struct PrivateProperty {
    _private: visibility::PrivateEntry,
    _memory: super::budget::Memory,
}
fn register_property(
    file: &File,
    name: &str,
    entries: &mut visibility::Entries,
    memory: super::budget::Memory,
) -> Result<(String, PrivateProperty)> {
    let key = name.to_owned();
    let private = visibility::PrivateEntry::register(file, entries)?;
    Ok((
        key,
        PrivateProperty {
            _private: private,
            _memory: memory,
        },
    ))
}
fn open_file(root: &Root, directory: &File, name: &str) -> Result<File> {
    let file = File::from(openat2(
        directory,
        name,
        OpenHow::new()
            .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS
                    | ResolveFlag::RESOLVE_NO_SYMLINKS,
            ),
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() == 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > RECORD_LIMIT as u64
    {
        return Err(DavError::new(403, "DAV_PROPERTY_STORE"));
    }
    let file = root.reopen_checked(file, false)?;
    super::private_file::require(&file)?;
    Ok(file)
}
fn read(file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((RECORD_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > RECORD_LIMIT {
        return Err(DavError::limit());
    }
    Ok(bytes)
}
fn key(path: &DavPath) -> String {
    openssl::sha::sha256(path.key().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        + ".json"
}
fn marker(identity: &StorageIdentity, root: &Root) -> Result<Vec<u8>> {
    let (dev, ino) = namespace::identity(root.capability())?;
    Ok(format!(
        "rill-dav-v2 {dev:x} {ino:x} {}\n",
        openssl::sha::sha256(identity.storage_id.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
    .into_bytes())
}
fn validate_marker(identity: &StorageIdentity, root: &Root, directory: &File) -> Result<()> {
    let expected = marker(identity, root)?;
    let _memory = super::budget::Memory::acquire(2 * expected.len() + 128)?;
    let file = open_file(root, directory, ".owner")?;
    if file.metadata()?.len() != expected.len() as u64 || read(file)? != expected {
        return Err(DavError::new(403, "DAV_PROPERTY_STORE"));
    }
    Ok(())
}
fn directory(root: &Root) -> Result<Option<File>> {
    let result = openat2(
        root.capability(),
        ".rill-dav",
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS
                    | ResolveFlag::RESOLVE_NO_SYMLINKS,
            ),
    );
    match result {
        Ok(fd) => {
            let file = File::from(fd);
            let metadata = file.metadata()?;
            if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700
            {
                return Err(DavError::new(403, "DAV_PROPERTY_STORE"));
            }
            Ok(Some(file))
        }
        Err(Errno::ENOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn decode(bytes: &[u8], expected: Option<&DavPath>) -> Result<(DavPath, BTreeMap<Name, Node>)> {
    let record: Record =
        serde_json::from_slice(bytes).map_err(|_| DavError::new(500, "DAV_PROPERTY_STORE"))?;
    let path = DavPath::from_key(&record.path)?;
    if record.version != 1
        || path.key() != record.path
        || record.properties.len() > 64
        || record.properties.iter().map(String::len).sum::<usize>() > PROPERTY_LIMIT
        || expected.is_some_and(|expected| *expected != path)
    {
        return Err(DavError::new(500, "DAV_PROPERTY_STORE"));
    }
    let mut properties = BTreeMap::new();
    for property in record.properties {
        let node = xml::parse(property.as_bytes())?;
        if live(&node.name) || properties.insert(node.name.clone(), node).is_some() {
            return Err(DavError::new(500, "DAV_PROPERTY_STORE"));
        }
    }
    Ok((path, properties))
}
pub fn validate(
    identity: &StorageIdentity,
    root: &Root,
    mut charge: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    let Some(directory) = directory(root)? else {
        return Ok(());
    };
    validate_marker(identity, root, &directory)?;
    let clone = directory.try_clone()?;
    let mut entries = nix::dir::Dir::from_fd(clone.into())?;
    for entry in entries.iter() {
        let entry = entry?;
        let raw = entry.file_name().to_bytes();
        if matches!(raw, b"." | b".." | b".owner") {
            continue;
        }
        charge(512 + raw.len() + visibility::entry_bytes())?;
        let name =
            std::str::from_utf8(raw).map_err(|_| DavError::new(403, "DAV_PROPERTY_STORE"))?;
        let _work = super::budget::Memory::acquire(4 * RECORD_LIMIT)?;
        let bytes = read(open_file(root, &directory, name)?)?;
        let (path, _) = decode(&bytes, None)?;
        if key(&path) != name {
            return Err(DavError::new(403, "DAV_PROPERTY_STORE"));
        }
    }
    Ok(())
}
struct CreatedDirectory<'a> {
    parent: &'a File,
    identity: (u64, u64),
    cleanup: bool,
}
impl Drop for CreatedDirectory<'_> {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = namespace::remove_owned(self.parent, ".rill-dav", self.identity, true);
        }
    }
}
#[derive(Debug)]
pub struct ActivatedProperties {
    pub properties: std::sync::Arc<Properties>,
    created: bool,
    root: std::sync::Arc<Root>,
    committed: bool,
    owner_identity: Option<(u64, u64)>,
}
impl ActivatedProperties {
    pub fn activate(identity: &StorageIdentity, root: std::sync::Arc<Root>) -> Result<Self> {
        let mut files = BTreeMap::new();
        let mut visibility = visibility::write();
        let mut created_directory = None;
        let (directory, created) = match directory(&root)? {
            Some(directory) => {
                validate_marker(identity, &root, &directory)?;
                (directory, false)
            }
            None => {
                nix::sys::stat::mkdirat(
                    root.capability(),
                    ".rill-dav",
                    Mode::from_bits_truncate(0o700),
                )?;
                let metadata = nix::sys::stat::fstatat(
                    root.capability(),
                    ".rill-dav",
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                )?;
                created_directory = Some(CreatedDirectory {
                    parent: root.capability(),
                    identity: (metadata.st_dev, metadata.st_ino),
                    cleanup: true,
                });
                (directory(&root)?.ok_or_else(DavError::invalid)?, true)
            }
        };
        if !created {
            let mut entries = nix::dir::Dir::from_fd(directory.try_clone()?.into())?;
            for entry in entries.iter() {
                let entry = entry?;
                let raw = entry.file_name().to_bytes();
                if matches!(raw, b"." | b"..") {
                    continue;
                }
                let name = std::str::from_utf8(raw)
                    .map_err(|_| DavError::new(403, "DAV_PROPERTY_STORE"))?;
                let memory = super::budget::Memory::acquire(512 + name.len())?;
                let file = open_file(&root, &directory, name)?;
                let (key, private) = register_property(&file, name, &mut visibility, memory)?;
                files.insert(key, private);
            }
        }
        let private = visibility::PrivateEntry::register(&directory, &mut visibility)?;
        drop(visibility);
        let mut activated = Self {
            properties: std::sync::Arc::new(Properties {
                root: root.clone(),
                directory,
                _private: private,
                files: std::sync::Mutex::new(files),
            }),
            created,
            root: root.clone(),
            committed: false,
            owner_identity: None,
        };
        if let Some(directory) = &mut created_directory {
            directory.cleanup = false;
        }
        if created {
            let directory = &activated.properties.directory;
            let memory = super::budget::Memory::acquire(518)?;
            let mut entries = visibility::write();
            let mut owner = super::private_file::create(&activated.root, directory, ".owner")?;
            activated.owner_identity = Some(namespace::identity(&owner)?);
            let (key, private) = register_property(&owner, ".owner", &mut entries, memory)?;
            drop(entries);
            activated
                .properties
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, private);
            owner.write_all(&marker(identity, &activated.root)?)?;
            owner.sync_all()?;
            directory.sync_all()?;
            owner.keep();
        }
        Ok(activated)
    }
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for ActivatedProperties {
    fn drop(&mut self) {
        if self.created && !self.committed {
            if let Some(identity) = self.owner_identity {
                let _ =
                    namespace::remove_owned(&self.properties.directory, ".owner", identity, false);
            }
            if let Ok(identity) = namespace::identity(&self.properties.directory) {
                let _ =
                    namespace::remove_owned(self.root.capability(), ".rill-dav", identity, true);
            }
        }
    }
}
impl Properties {
    pub fn load(&self, path: &DavPath) -> Result<BTreeMap<Name, Node>> {
        match open_file(&self.root, &self.directory, &key(path)) {
            Ok(file) => decode(&read(file)?, Some(path)).map(|(_, p)| p),
            Err(error) if error.status == 404 => Ok(BTreeMap::new()),
            Err(error) => Err(error),
        }
    }
    pub fn store(
        &self,
        path: &DavPath,
        properties: &BTreeMap<Name, Node>,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if properties.len() > 64 {
            return Err(DavError::new(413, "DAV_LIMIT"));
        }
        let mut values = Vec::new();
        let mut total = 0_usize;
        for node in properties.values() {
            let value = node.serialize()?;
            total = total
                .checked_add(value.len())
                .filter(|n| *n <= PROPERTY_LIMIT)
                .ok_or_else(|| DavError::new(413, "DAV_LIMIT"))?;
            values.push(value);
        }
        let record = Record {
            version: 1,
            path: path.key().to_owned(),
            properties: values,
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|_| DavError::new(500, "DAV_PROPERTY_STORE"))?;
        if bytes.len() > RECORD_LIMIT {
            return Err(DavError::new(413, "DAV_LIMIT"));
        }
        let destination = key(path);
        let existing = match open_file(&self.root, &self.directory, &destination) {
            Ok(file) => Some(file),
            Err(error) if error.status == 404 => None,
            Err(error) => return Err(error),
        };
        let memory = super::budget::Memory::acquire(512 + destination.len())?;
        let name = format!(".rill-dav-put-{}", random_name()?);
        let mut visibility = visibility::write();
        let mut temporary = super::private_file::create(&self.root, &self.directory, &name)?;
        let (property_key, private) =
            register_property(&temporary, &destination, &mut visibility, memory)?;
        drop(visibility);
        (|| {
            temporary.write_all(&bytes)?;
            temporary.sync_all()?;
            before_commit()?;
            renameat(
                &self.directory,
                name.as_str(),
                &self.directory,
                destination.as_str(),
            )?;
            temporary.keep();
            self.files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(property_key, private);
            drop(existing);
            self.directory
                .sync_all()
                .map_err(|_| DavError::new(500, "DAV_PROPERTY_COMMIT_DURABILITY_UNKNOWN"))?;
            Ok(())
        })()
    }
    pub fn delete(&self, path: &DavPath, before_commit: impl FnOnce() -> Result<()>) -> Result<()> {
        let name = key(path);
        let file = match open_file(&self.root, &self.directory, &name) {
            Ok(file) => file,
            Err(error) if error.status == 404 => return Ok(()),
            Err(error) => return Err(error),
        };
        before_commit()?;
        match namespace::remove_owned(&self.directory, &name, namespace::identity(&file)?, false) {
            Ok(()) => {
                self.files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&name);
                self.directory.sync_all()?;
                Ok(())
            }
            Err(error) if error.status == 404 => Ok(()),
            Err(error) => Err(error),
        }
    }
}
pub fn random_name() -> Result<String> {
    let mut bytes = [0_u8; 16];
    openssl::rand::rand_bytes(&mut bytes).map_err(|_| DavError::new(500, "DAV_RANDOM"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn live(name: &Name) -> bool {
    name.namespace == xml::DAV
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_creation_owns_cleanup_before_registration_quota_failure()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        const CHILD: &str = "RILL_DAV_PRIVATE_QUOTA_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe()?).args(["webdav::properties::tests::private_creation_owns_cleanup_before_registration_quota_failure", "--exact", "--test-threads=1"]).env(CHILD,"1").status()?;
            assert!(status.success());
            return Ok(());
        }
        let path =
            std::env::temp_dir().join(format!("rill-dav-private-quota-{}", std::process::id()));
        std::fs::create_dir(&path)?;
        std::fs::write(path.join("file"), b"original")?;
        let root = std::sync::Arc::new(Root::from_prepared_fd(File::open(&path)?)?);
        let (device, inode) = namespace::identity(root.capability())?;
        let identity = StorageIdentity {
            storage_id: "quota-test".into(),
            device,
            inode,
        };
        let pressure = super::super::budget::Memory::acquire(64 * 1024 * 1024 - 128)?;
        assert_eq!(
            ActivatedProperties::activate(&identity, root.clone())
                .unwrap_err()
                .status,
            503
        );
        assert!(!path.join(".rill-dav").exists());
        drop(pressure);
        let name = ".rill-dav-put-quota";
        let named = super::super::private_file::create(&root, root.capability(), name)?;
        let pressure =
            super::super::budget::Memory::acquire(64 * 1024 * 1024 - name.len() - 256 - 128)?;
        assert!(visibility::PrivateEntry::register(&named, &mut visibility::write()).is_err());
        drop(named);
        assert!(!path.join(name).exists());
        drop(pressure);
        let activated = ActivatedProperties::activate(&identity, root.clone())?;
        let properties = activated.properties.clone();
        activated.commit();
        let resource = DavPath::parse("/file")?;
        properties.store(&resource, &BTreeMap::new(), || Ok(()))?;
        let record = path.join(".rill-dav").join(key(&resource));
        let previous = std::fs::read(&record)?;
        let mut pressure = super::super::budget::Memory::default();
        while pressure.reserve(4096).is_ok() {}
        while pressure.reserve(1).is_ok() {}
        pressure.release(1000);
        assert_eq!(
            properties
                .store(&resource, &BTreeMap::new(), || Ok(()))
                .unwrap_err()
                .status,
            503
        );
        assert_eq!(std::fs::read(&record)?, previous);
        assert!(std::fs::read_dir(path.join(".rill-dav"))?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".rill-dav-put-")
        }));
        drop(pressure);
        properties.store(&resource, &BTreeMap::new(), || Ok(()))?;
        assert_eq!(std::fs::read(path.join("file"))?, b"original");
        std::fs::remove_dir_all(&path)?;
        drop(properties);
        drop(root);
        drop(visibility::write());
        drop(super::super::budget::Memory::acquire(64 * 1024 * 1024)?);
        Ok(())
    }
    #[test]
    fn property_record_limits_count_text_and_paths_before_collecting() -> Result<()> {
        let too_many = Record {
            version: 1,
            path: "a".into(),
            properties: vec![String::new(); 65],
        };
        assert!(
            serde_json::from_slice::<Record>(
                &serde_json::to_vec(&too_many).map_err(|_| DavError::invalid())?
            )
            .is_err()
        );
        let too_large = Record {
            version: 1,
            path: "a".into(),
            properties: vec!["x".repeat(PROPERTY_LIMIT + 1)],
        };
        assert!(
            serde_json::from_slice::<Record>(
                &serde_json::to_vec(&too_large).map_err(|_| DavError::invalid())?
            )
            .is_err()
        );
        let too_long = Record {
            version: 1,
            path: "a".repeat(4097),
            properties: Vec::new(),
        };
        assert!(
            serde_json::from_slice::<Record>(
                &serde_json::to_vec(&too_long).map_err(|_| DavError::invalid())?
            )
            .is_err()
        );
        let (_,properties)=decode(br#"{"version":1,"path":"%2e","properties":["<p:note xmlns:p=\"urn:test\">value</p:note>"]}"#,Some(&DavPath::parse("/%252e")?))?;
        assert_eq!(properties.len(), 1);
        Ok(())
    }
}
