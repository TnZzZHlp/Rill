use super::{
    limits::{Cancellation, DiskBudget},
    model::{DavError, Result},
    namespace,
    properties::random_name,
    registry::StorageHandle,
    request_model::DavRequest,
    visibility,
};
use std::{fs::File, io::Write, sync::Arc, time::Instant};

pub struct Temporary {
    parent: File,
    file: super::private_file::NamedFile,
    name: String,
    _private: visibility::PrivateEntry,
    budget: DiskBudget,
    committed: bool,
}
impl Temporary {
    pub fn create(storage: &StorageHandle, request: &DavRequest) -> Result<Self> {
        let submitted = request.check(storage)?;
        let existing = namespace::metadata(&request.namespace, &request.path)?;
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(&request.path, false, existing.is_none(), &submitted)?;
        if existing.is_some() {
            namespace::writable(&request.namespace, &request.path)?;
        }
        let parent = namespace::parent(&request.namespace, &request.path)?;
        let name = format!(".rill-dav-put-{}", random_name()?);
        let mut visibility = visibility::write();
        let file = super::private_file::create(&storage.root, &parent, &name)?;
        let private = visibility::PrivateEntry::register(&file, &mut visibility)?;
        drop(visibility);
        Ok(Self {
            parent,
            file,
            name,
            _private: private,
            budget: DiskBudget::default(),
            committed: false,
        })
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > super::limits::BLOCK {
            return Err(DavError::limit());
        }
        self.budget.add(bytes.len())?;
        self.file.write_all(bytes)?;
        Ok(())
    }
    pub fn finish(
        mut self,
        storage: &StorageHandle,
        request: &DavRequest,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<u16> {
        if let Some(length) = super::request_model::single(&request.headers, "content-length")?
            && length.parse::<u64>().map_err(|_| DavError::invalid())? != self.budget.bytes()
        {
            return Err(DavError::invalid());
        }
        cancellation.check(deadline)?;
        self.file.sync_all()?;
        namespace::revalidate(&request.namespace, &request.path, &self.parent)?;
        let submitted = request.check(storage)?;
        let exists = namespace::metadata(&request.namespace, &request.path)?.is_some();
        storage
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(&request.path, false, !exists, &submitted)?;
        if exists {
            namespace::writable(&request.namespace, &request.path)?;
        }
        if std::os::unix::fs::MetadataExt::nlink(&self.file.metadata()?) != 1 {
            return Err(DavError::new(403, "DAV_ALIAS"));
        }
        let rename = namespace::prepare_rename(
            &request.namespace,
            &self.parent,
            &self.name,
            &self.parent,
            request.path.name(),
            &self.file,
        )?;
        cancellation.admit_commit(deadline)?;
        rename.commit()?;
        self.file.keep();
        self.committed = true;
        self.parent
            .sync_all()
            .map_err(|_| DavError::new(500, "DAV_COMMIT_DURABILITY_UNKNOWN"))?;
        super::private_file::publish(&self.file)?;
        self._private.publish();
        Ok(if exists { 204 } else { 201 })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.file.remove();
        }
        if !self._private.published() {
            self._private.retain_disk(std::mem::take(&mut self.budget));
        }
    }
}
struct UploadState {
    temporary: Temporary,
    storage: Arc<StorageHandle>,
    request: DavRequest,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
pub struct Upload(Option<UploadState>);
impl Upload {
    pub fn create(
        storage: Arc<StorageHandle>,
        request: DavRequest,
        permit: tokio::sync::OwnedSemaphorePermit,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<Self> {
        cancellation.check(deadline)?;
        let temporary = Temporary::create(&storage, &request)?;
        Ok(Self(Some(UploadState {
            temporary,
            storage,
            request,
            _permit: permit,
        })))
    }
    pub fn write(
        mut self,
        bytes: super::budget::Buffer,
        cancellation: &Cancellation,
        deadline: Instant,
    ) -> Result<Self> {
        cancellation.check(deadline)?;
        self.0
            .as_mut()
            .ok_or_else(|| DavError::new(500, "DAV_UPLOAD_STATE"))?
            .temporary
            .write(bytes.as_ref())?;
        Ok(self)
    }
    pub fn finish(mut self, cancellation: &Cancellation, deadline: Instant) -> Result<u16> {
        let state = self
            .0
            .take()
            .ok_or_else(|| DavError::new(500, "DAV_UPLOAD_STATE"))?;
        let UploadState {
            temporary,
            storage,
            request,
            _permit,
        } = state;
        temporary.finish(&storage, &request, cancellation, deadline)
    }
}
impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            tokio::task::spawn_blocking(move || drop(state));
        }
    }
}
