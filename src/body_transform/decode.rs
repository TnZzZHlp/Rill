use super::{
    budget::Buffer,
    headers::Coding,
    limits::{Clock, Failure},
    pipeline::{Event, Input},
};
use std::{
    cell::Cell,
    io::{self, BufRead, Read},
    rc::Rc,
};
use tokio::sync::mpsc;

pub(crate) fn io_error(failure: Failure) -> io::Error {
    io::Error::other(format!("body transform {failure:?}"))
}

pub(crate) struct Source {
    rx: mpsc::Receiver<Input>,
    events: mpsc::Sender<super::limits::Result<Event>>,
    current: Option<Buffer>,
    position: usize,
    eof: bool,
    had_input: bool,
    clock: Clock,
    pub consumed: Rc<Cell<usize>>,
}
impl Source {
    pub fn new(
        rx: mpsc::Receiver<Input>,
        events: mpsc::Sender<super::limits::Result<Event>>,
        clock: Clock,
        consumed: Rc<Cell<usize>>,
    ) -> Self {
        Self {
            rx,
            events,
            current: None,
            position: 0,
            eof: false,
            had_input: false,
            clock,
            consumed,
        }
    }
}
impl BufRead for Source {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.clock.check().map_err(io_error)?;
        if self
            .current
            .as_ref()
            .is_some_and(|b| self.position == b.data().len())
        {
            self.current = None;
            self.position = 0;
        }
        if self.current.is_none() && !self.eof {
            if self.had_input {
                self.clock
                    .pause(|| self.events.blocking_send(Ok(Event::InputDone)))
                    .map_err(|_| io_error(Failure::Cancelled))?;
            }
            match self.clock.pause(|| self.rx.blocking_recv()) {
                Some(Input::Data(data, eof)) => {
                    self.current = Some(data);
                    self.eof = eof;
                    self.had_input = true;
                }
                Some(Input::End) => self.eof = true,
                None => return Err(io_error(Failure::Cancelled)),
            }
        }
        Ok(self
            .current
            .as_ref()
            .map_or(&[], |b| &b.data()[self.position..]))
    }
    fn consume(&mut self, amount: usize) {
        self.position += amount;
        self.consumed
            .set(self.consumed.get().saturating_add(amount));
    }
}
impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let input = self.fill_buf()?;
        let n = input.len().min(bytes.len());
        bytes[..n].copy_from_slice(&input[..n]);
        self.consume(n);
        Ok(n)
    }
}

pub(crate) fn decoder(source: Source, coding: Coding) -> io::Result<Box<dyn Read>> {
    match coding {
        Coding::Identity => Ok(Box::new(source)),
        Coding::Gzip => Ok(Box::new(flate2::bufread::MultiGzDecoder::new(source))),
        Coding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(source)?;
            decoder.window_log_max(20)?;
            Ok(Box::new(decoder))
        }
    }
}
