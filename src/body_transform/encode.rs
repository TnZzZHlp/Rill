use super::{
    budget::{Buffer, Credit, ResponseBudget},
    decode::io_error,
    headers::Coding,
    limits::{CHUNK, Clock, Failure, OutputLimit},
    pipeline::Event,
};
use std::{
    io::{self, Write},
    sync::Arc,
};
use tokio::sync::mpsc;

pub(crate) struct Destination {
    budget: Arc<ResponseBudget>,
    events: mpsc::Sender<super::limits::Result<Event>>,
    clock: Clock,
    output: OutputLimit,
    credit: Option<Arc<Credit>>,
}
impl Destination {
    pub fn new(
        budget: Arc<ResponseBudget>,
        events: mpsc::Sender<super::limits::Result<Event>>,
        clock: Clock,
        credit: Option<Arc<Credit>>,
    ) -> Self {
        Self {
            budget,
            events,
            clock,
            output: OutputLimit::new(),
            credit,
        }
    }
}
impl Write for Destination {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.clock.check().map_err(io_error)?;
        for chunk in input.chunks(CHUNK) {
            self.output.add(chunk.len()).map_err(io_error)?;
            let mut data = self
                .credit
                .as_ref()
                .and_then(|credit| Buffer::from_credit(credit, chunk.len()))
                .or_else(|| Buffer::new(&self.budget, chunk.len()))
                .ok_or_else(|| io_error(Failure::Budget))?;
            data.data_mut().extend_from_slice(chunk);
            self.clock
                .pause(|| {
                    self.events
                        .blocking_send(Ok(Event::Chunk(data.into_bytes())))
                })
                .map_err(|_| io_error(Failure::Cancelled))?;
        }
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.clock.check().map_err(io_error)
    }
}

pub(crate) enum Encoder {
    Identity(Destination),
    Gzip(flate2::write::GzEncoder<Destination>),
    Zstd(zstd::stream::write::Encoder<'static, Destination>),
}
impl Encoder {
    pub fn new(destination: Destination, coding: Coding) -> io::Result<Self> {
        Ok(match coding {
            Coding::Identity => Self::Identity(destination),
            Coding::Gzip => Self::Gzip(flate2::write::GzEncoder::new(
                destination,
                flate2::Compression::new(3),
            )),
            Coding::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(destination, 3)?;
                encoder.window_log(20)?;
                encoder.include_checksum(true)?;
                Self::Zstd(encoder)
            }
        })
    }
    pub fn finish(self) -> io::Result<()> {
        match self {
            Self::Identity(_) => Ok(()),
            Self::Gzip(encoder) => encoder.finish().map(|_| ()),
            Self::Zstd(encoder) => encoder.finish().map(|_| ()),
        }
    }
}
impl Write for Encoder {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        match self {
            Self::Identity(w) => w.write(input),
            Self::Gzip(w) => w.write(input),
            Self::Zstd(w) => w.write(input),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Identity(w) => w.flush(),
            Self::Gzip(w) => w.flush(),
            Self::Zstd(w) => w.flush(),
        }
    }
}
