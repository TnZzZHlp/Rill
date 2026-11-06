use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

pub const CHUNK: usize = 16 * 1024;
pub const TOTAL: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Capacity,
    Budget,
    Output,
    Cpu,
    Codec,
    Cancelled,
    Finished,
}
pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone)]
pub struct Clock(Rc<ClockState>);
struct ClockState {
    elapsed: Cell<Duration>,
    active: Cell<Option<Instant>>,
}
impl Clock {
    pub fn new() -> Self {
        Self(Rc::new(ClockState {
            elapsed: Cell::new(Duration::ZERO),
            active: Cell::new(Some(Instant::now())),
        }))
    }
    pub fn check(&self) -> Result<()> {
        if self
            .0
            .elapsed
            .get()
            .saturating_add(self.0.active.get().map_or(Duration::ZERO, |t| t.elapsed()))
            > Duration::from_secs(2)
        {
            Err(Failure::Cpu)
        } else {
            Ok(())
        }
    }
    pub fn pause<T>(&self, f: impl FnOnce() -> T) -> T {
        if let Some(active) = self.0.active.take() {
            self.0
                .elapsed
                .set(self.0.elapsed.get().saturating_add(active.elapsed()));
        }
        let result = f();
        self.0.active.set(Some(Instant::now()));
        result
    }
}

pub struct OutputLimit(Cell<usize>);
impl OutputLimit {
    pub fn new() -> Self {
        Self(Cell::new(0))
    }
    pub fn add(&self, n: usize) -> Result<()> {
        self.0.set(
            self.0
                .get()
                .checked_add(n)
                .filter(|&n| n <= TOTAL)
                .ok_or(Failure::Output)?,
        );
        Ok(())
    }
}
