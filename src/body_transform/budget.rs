use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

const REQUEST_LIMIT: usize = 1024 * 1024;
const PROCESS_LIMIT: usize = 64 * REQUEST_LIMIT;
const RESPONSE_LIMIT: usize = 64;

static PROCESS_BYTES: AtomicUsize = AtomicUsize::new(0);
static RESPONSES: AtomicUsize = AtomicUsize::new(0);

pub struct ResponseBudget {
    used: AtomicUsize,
    _owner: Option<Arc<dyn Send + Sync>>,
}

impl ResponseBudget {
    pub fn acquire() -> Option<Arc<Self>> {
        Self::acquire_owned(None)
    }

    pub fn acquire_owned(owner: Option<Arc<dyn Send + Sync>>) -> Option<Arc<Self>> {
        RESPONSES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(1).filter(|next| *next <= RESPONSE_LIMIT)
            })
            .ok()?;
        Some(Arc::new(Self {
            used: AtomicUsize::new(0),
            _owner: owner,
        }))
    }

    pub(crate) fn remaining(&self) -> usize {
        REQUEST_LIMIT.saturating_sub(self.used.load(Ordering::Acquire))
    }

    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<BufferPermit> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= REQUEST_LIMIT)
            })
            .ok()?;
        if PROCESS_BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= PROCESS_LIMIT)
            })
            .is_err()
        {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
            return None;
        }
        Some(BufferPermit {
            response: Arc::clone(self),
            bytes,
            credit: Weak::new(),
        })
    }
}

impl Drop for ResponseBudget {
    fn drop(&mut self) {
        RESPONSES.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct BufferPermit {
    response: Arc<ResponseBudget>,
    bytes: usize,
    credit: Weak<Credit>,
}

pub(crate) struct Credit {
    response: Arc<ResponseBudget>,
    free: Mutex<usize>,
}
impl Credit {
    pub fn new(response: &Arc<ResponseBudget>, bytes: usize) -> Option<Arc<Self>> {
        let mut permit = response.reserve(bytes)?;
        permit.bytes = 0;
        Some(Arc::new(Self {
            response: response.clone(),
            free: Mutex::new(bytes),
        }))
    }
    fn take(self: &Arc<Self>, bytes: usize) -> Option<BufferPermit> {
        let mut free = self.free.lock().ok()?;
        *free = free.checked_sub(bytes)?;
        Some(BufferPermit {
            response: self.response.clone(),
            bytes,
            credit: Arc::downgrade(self),
        })
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        let bytes = *self
            .free
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PROCESS_BYTES.fetch_sub(bytes, Ordering::AcqRel);
        self.response.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

impl BufferPermit {
    pub(crate) fn retain_source<T: AsRef<[u8]> + Send + 'static>(self, data: T) -> bytes::Bytes {
        bytes::Bytes::from_owner(SourceOwner {
            data,
            _permit: self,
        })
    }

    pub fn into_bytes(self, data: Box<[u8]>) -> Result<bytes::Bytes, (Self, Box<[u8]>)> {
        if self.bytes != data.len() {
            return Err((self, data));
        }
        Ok(bytes::Bytes::from_owner(BudgetedBytes {
            data,
            _permit: self,
        }))
    }
}

struct SourceOwner<T> {
    data: T,
    _permit: BufferPermit,
}
impl<T: AsRef<[u8]>> AsRef<[u8]> for SourceOwner<T> {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref()
    }
}

struct BudgetedBytes {
    data: Box<[u8]>,
    _permit: BufferPermit,
}

impl AsRef<[u8]> for BudgetedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for BufferPermit {
    fn drop(&mut self) {
        if let Some(credit) = self.credit.upgrade() {
            *credit
                .free
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += self.bytes;
        } else {
            PROCESS_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
            self.response.used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

impl std::fmt::Debug for BufferPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPermit")
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub struct Buffer {
    data: Vec<u8>,
    permit: BufferPermit,
}
impl Buffer {
    pub fn new(budget: &Arc<ResponseBudget>, capacity: usize) -> Option<Self> {
        Self::allocate(budget.reserve(capacity)?, capacity)
    }
    pub(crate) fn from_credit(credit: &Arc<Credit>, capacity: usize) -> Option<Self> {
        Self::allocate(credit.take(capacity)?, capacity)
    }
    fn allocate(permit: BufferPermit, capacity: usize) -> Option<Self> {
        let mut data = Vec::new();
        data.try_reserve_exact(capacity).ok()?;
        if data.capacity() != capacity {
            return None;
        }
        Some(Self { data, permit })
    }
    pub(crate) fn belongs_to(&self, budget: &Arc<ResponseBudget>) -> bool {
        Arc::ptr_eq(&self.permit.response, budget)
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn data_mut(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }
    pub fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(OwnedBuffer {
            data: self.data,
            _permit: self.permit,
        })
    }
}
struct OwnedBuffer {
    data: Vec<u8>,
    _permit: BufferPermit,
}
impl AsRef<[u8]> for OwnedBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_permits_share_response_limit_and_keep_response_alive() {
        let response = ResponseBudget::acquire().expect("response capacity");
        let permit = response.reserve(REQUEST_LIMIT).expect("exact limit");
        assert!(response.reserve(1).is_none());
        let weak = Arc::downgrade(&response);
        drop(response);
        assert!(weak.upgrade().is_some());
        drop(permit);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn output_clones_hold_buffer_and_response_permits() {
        let response = ResponseBudget::acquire().expect("response capacity");
        let permit = response.reserve(4).expect("buffer capacity");
        let output = permit
            .into_bytes(Box::new(*b"test"))
            .expect("matching size");
        let clone = output.clone();
        let weak = Arc::downgrade(&response);
        drop(response);
        drop(output);
        assert!(weak.upgrade().is_some());
        assert_eq!(&clone[..], b"test");
        drop(clone);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn simultaneous_allocations_and_output_clones_share_actual_capacity() {
        let response = ResponseBudget::acquire().expect("capacity");
        let mut buffer = Buffer::new(&response, REQUEST_LIMIT / 2).expect("initial allocation");
        buffer.data_mut().push(b'a');
        assert!(Buffer::new(&response, REQUEST_LIMIT / 2 + 1).is_none());
        assert_eq!(buffer.data(), b"a");
        let clone = buffer.into_bytes();
        let retained = clone.clone();
        assert!(response.reserve(REQUEST_LIMIT / 2).is_some());
        assert!(response.reserve(REQUEST_LIMIT / 2 + 1).is_none());
        drop(clone);
        assert_eq!(response.used.load(Ordering::Acquire), REQUEST_LIMIT / 2);
        drop(retained);
        assert_eq!(response.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn working_credit_transfers_without_double_charging_and_outlives_its_pool() {
        let response = ResponseBudget::acquire().expect("capacity");
        let credit = Credit::new(&response, 64 * 1024).expect("working reservation");
        assert_eq!(response.used.load(Ordering::Acquire), 64 * 1024);
        let mut buffer = Buffer::from_credit(&credit, 16 * 1024).expect("credit transfer");
        buffer.data_mut().push(b'a');
        assert_eq!(response.used.load(Ordering::Acquire), 64 * 1024);
        let bytes = buffer.into_bytes();
        let clone = bytes.clone();
        drop(credit);
        assert_eq!(response.used.load(Ordering::Acquire), 16 * 1024);
        drop(bytes);
        assert_eq!(response.used.load(Ordering::Acquire), 16 * 1024);
        drop(clone);
        assert_eq!(response.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn returned_working_credit_is_reusable_only_after_the_last_owner_drops() {
        let response = ResponseBudget::acquire().expect("capacity");
        let credit = Credit::new(&response, 16).expect("credit");
        let mut buffer = Buffer::from_credit(&credit, 16).expect("buffer");
        buffer.data_mut().push(1);
        let bytes = buffer.into_bytes();
        let retained = bytes.clone();
        drop(bytes);
        assert!(Buffer::from_credit(&credit, 1).is_none());
        drop(retained);
        assert!(Buffer::from_credit(&credit, 16).is_some());
        assert_eq!(response.used.load(Ordering::Acquire), 16);
        drop(credit);
        assert_eq!(response.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn isolated_process_limit_is_64_actual_mebibytes_and_64_live_responses() {
        const CHILD: &str = "RILL_BODY_CAPACITY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["body_transform::budget::tests::isolated_process_limit_is_64_actual_mebibytes_and_64_live_responses", "--exact", "--test-threads=1"])
                .env(CHILD, "1").status().expect("isolated budget test");
            assert!(status.success());
            return;
        }
        let mut responses = Vec::new();
        let mut buffers = Vec::new();
        for _ in 0..RESPONSE_LIMIT {
            let response = ResponseBudget::acquire().expect("64 response permits");
            let mut buffer = Buffer::new(&response, REQUEST_LIMIT).expect("one MiB per response");
            buffer.data_mut().push(1);
            responses.push(response);
            buffers.push(buffer);
        }
        assert!(ResponseBudget::acquire().is_none());
        assert_eq!(PROCESS_BYTES.load(Ordering::Acquire), PROCESS_LIMIT);
        let retained = buffers.pop().expect("buffer").into_bytes();
        drop(responses.pop());
        assert!(ResponseBudget::acquire().is_none());
        drop(retained);
        assert!(ResponseBudget::acquire().is_some());
        drop(buffers);
        drop(responses);
        assert_eq!(PROCESS_BYTES.load(Ordering::Acquire), 0);
        assert_eq!(RESPONSES.load(Ordering::Acquire), 0);
    }
}
