use super::model::{DavError, Result};

#[derive(Default)]
pub struct Buffer {
    bytes: Vec<u8>,
    memory: Memory,
}
impl Buffer {
    pub fn with_capacity(capacity: usize) -> Result<Self> {
        let memory = Memory::acquire(capacity)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| DavError::limit())?;
        Ok(Self { bytes, memory })
    }
    pub fn copy_from(bytes: &[u8]) -> Result<Self> {
        let mut buffer = Self::default();
        buffer.extend(bytes, bytes.len())?;
        Ok(buffer)
    }
    pub fn extend(&mut self, bytes: &[u8], maximum: usize) -> Result<()> {
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= maximum)
            .ok_or_else(|| DavError::new(413, "DAV_LIMIT"))?;
        if self.bytes.capacity() < length {
            let old = self.bytes.capacity();
            self.memory.reserve(length)?;
            if self
                .bytes
                .try_reserve_exact(length - self.bytes.len())
                .is_err()
            {
                self.memory.release(length);
                return Err(DavError::limit());
            }
            self.memory.release(old);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
#[derive(Debug, Default)]
pub struct Memory(crate::http_policy::compile::ExternalBudget);
impl Memory {
    pub fn reserve(&mut self, bytes: usize) -> Result<()> {
        if !self.0.reserve_process(bytes) {
            return Err(DavError::limit());
        }
        Ok(())
    }
    pub fn release(&mut self, bytes: usize) {
        self.0.release(bytes);
    }
    pub fn acquire(bytes: usize) -> Result<Self> {
        let mut memory = Self::default();
        memory.reserve(bytes)?;
        Ok(memory)
    }
}
