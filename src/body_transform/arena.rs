use super::{
    budget::{Buffer, ResponseBudget},
    limits::{Clock, Failure, Result},
};
use std::sync::Arc;

#[derive(Clone, Copy, Default)]
struct Region {
    start: usize,
    len: usize,
    live: bool,
}

pub(crate) struct PendingArena {
    buffer: Buffer,
    regions: [Region; 16],
}
impl PendingArena {
    pub fn new(budget: &Arc<ResponseBudget>, enabled: bool) -> Result<Self> {
        let capacity = if enabled { budget.remaining() } else { 0 };
        let mut buffer = Buffer::new(budget, capacity).ok_or(Failure::Budget)?;
        buffer.data_mut().resize(capacity, 0);
        Ok(Self {
            buffer,
            regions: [Region::default(); 16],
        })
    }
    pub fn data(&self, id: usize) -> &[u8] {
        let region = self.regions[id];
        &self.buffer.data()[region.start..region.start + region.len]
    }
    pub fn append(&mut self, id: usize, bytes: &[u8], clock: &Clock) -> Result<()> {
        clock.check()?;
        if bytes.is_empty() {
            return Ok(());
        }
        let end = self
            .regions
            .iter()
            .filter(|r| r.live)
            .map(|r| r.start + r.len)
            .max()
            .unwrap_or(0);
        if !self.regions[id].live {
            self.regions[id] = Region {
                start: end,
                len: 0,
                live: true,
            };
        }
        if self.regions[id].start + self.regions[id].len != end {
            return Err(Failure::Finished);
        }
        if end
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.buffer.data().len())
        {
            self.compact(clock)?;
        }
        let region = &mut self.regions[id];
        let end = region.start + region.len;
        let next = end
            .checked_add(bytes.len())
            .filter(|&n| n <= self.buffer.data().len())
            .ok_or(Failure::Budget)?;
        self.buffer.data_mut()[end..next].copy_from_slice(bytes);
        region.len += bytes.len();
        clock.check()
    }
    fn compact(&mut self, clock: &Clock) -> Result<()> {
        let mut order: [usize; 16] = std::array::from_fn(|id| id);
        order.sort_unstable_by_key(|&id| self.regions[id].start);
        let mut end = 0;
        for id in order {
            let region = &mut self.regions[id];
            if !region.live {
                continue;
            }
            clock.check()?;
            if region.start != end {
                self.buffer
                    .data_mut()
                    .copy_within(region.start..region.start + region.len, end);
                region.start = end;
            }
            end += region.len;
        }
        clock.check()
    }
    pub fn release(&mut self, id: usize) {
        self.regions[id] = Region::default();
    }
}
