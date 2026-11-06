use super::{
    arena::PendingArena,
    budget::{Buffer, Credit, ResponseBudget},
    decode::{Source, decoder},
    encode::{Destination, Encoder},
    headers::Selection,
    limits::{CHUNK, Clock, Failure, OutputLimit, Result, TOTAL},
    replace,
};
use crate::runtime::state::RuntimeSnapshot;
use bytes::Bytes;
use std::{
    cell::Cell,
    io::{Read, Write},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub(crate) enum Input {
    Data(Buffer, bool),
    End,
}
pub enum Event {
    Ready,
    Chunk(Bytes),
    InputDone,
    Finished,
}

pub struct Pipeline {
    input: Option<mpsc::Sender<Input>>,
    output: mpsc::Receiver<Result<Event>>,
    budget: Arc<ResponseBudget>,
    input_credit: Option<Arc<Credit>>,
    empty: bool,
    finished: bool,
    pending: bool,
}

pub(crate) fn proxy_error(failure: Failure) -> Box<pingora::Error> {
    pingora::Error::explain(
        pingora::ErrorType::HTTPStatus(503),
        format!(
            "{}: {failure:?}",
            if failure == Failure::Capacity {
                "BODY_CAPACITY"
            } else {
                "BODY_BUDGET_EXCEEDED"
            }
        ),
    )
}

impl Pipeline {
    pub async fn start(
        snapshot: Arc<RuntimeSnapshot>,
        selection: Selection,
    ) -> pingora::Result<Self> {
        let budget = Self::acquire(snapshot.clone())?;
        Self::start_budgeted(snapshot, selection, budget).await
    }

    pub(crate) fn acquire(snapshot: Arc<RuntimeSnapshot>) -> pingora::Result<Arc<ResponseBudget>> {
        ResponseBudget::acquire_owned(Some(snapshot)).ok_or_else(|| proxy_error(Failure::Capacity))
    }

    pub(crate) async fn start_budgeted(
        snapshot: Arc<RuntimeSnapshot>,
        selection: Selection,
        budget: Arc<ResponseBudget>,
    ) -> pingora::Result<Self> {
        let unbounded = replace::has_unbounded(&selection.replacements, &snapshot.policy);
        let input_credit = if unbounded {
            Some(Credit::new(&budget, 64 * 1024).ok_or_else(|| proxy_error(Failure::Budget))?)
        } else {
            None
        };
        let output_credit = if unbounded {
            Some(Credit::new(&budget, 3 * CHUNK).ok_or_else(|| proxy_error(Failure::Budget))?)
        } else {
            None
        };
        let (input, rx) = mpsc::channel(1);
        let (tx, output) = mpsc::channel(1);
        let worker_budget = budget.clone();
        tokio::task::spawn_blocking(move || {
            let result = run(
                &snapshot,
                &selection,
                rx,
                &tx,
                &worker_budget,
                output_credit,
            );
            if let Err(failure) = result {
                let _ = tx.blocking_send(Err(failure));
            }
        });
        let mut pipeline = Self {
            input: Some(input),
            output,
            budget,
            input_credit,
            empty: false,
            finished: false,
            pending: false,
        };
        match pipeline.next().await? {
            Event::Ready => Ok(pipeline),
            _ => Err(proxy_error(Failure::Finished)),
        }
    }

    pub fn push(&mut self, data: Option<Bytes>, eof: bool) -> pingora::Result<()> {
        let input = self.copy_input(data.as_deref().unwrap_or_default(), eof);
        drop(data);
        self.finish_submit(input, eof)
    }

    pub(crate) fn push_slice(&mut self, data: &[u8], eof: bool) -> pingora::Result<()> {
        let input = self.copy_input(data, eof);
        self.finish_submit(input, eof)
    }

    fn copy_input(&mut self, data: &[u8], eof: bool) -> pingora::Result<Option<Input>> {
        self.check_input()?;
        if data.is_empty() {
            return Ok(eof.then_some(Input::End));
        }
        let mut buffer = self.input_buffer(data.len())?;
        buffer.data_mut().copy_from_slice(data);
        Ok(Some(Input::Data(buffer, eof)))
    }

    fn check_input(&self) -> pingora::Result<()> {
        if self.finished || self.pending {
            Err(proxy_error(Failure::Finished))
        } else {
            Ok(())
        }
    }

    pub(crate) fn input_buffer(&self, capacity: usize) -> pingora::Result<Buffer> {
        self.check_input()?;
        let mut buffer = if let Some(credit) = &self.input_credit {
            Buffer::from_credit(credit, capacity)
        } else {
            Buffer::new(&self.budget, capacity)
        }
        .ok_or_else(|| proxy_error(Failure::Budget))?;
        buffer.data_mut().resize(capacity, 0);
        Ok(buffer)
    }

    pub(crate) fn push_buffer(&mut self, buffer: Buffer, eof: bool) -> pingora::Result<()> {
        self.check_input()?;
        if !buffer.belongs_to(&self.budget) {
            return Err(proxy_error(Failure::Budget));
        }
        self.finish_submit(Ok(Some(Input::Data(buffer, eof))), eof)
    }

    fn finish_submit(
        &mut self,
        input: pingora::Result<Option<Input>>,
        eof: bool,
    ) -> pingora::Result<()> {
        let result = input.and_then(|input| self.submit(input, eof));
        if result.is_err() {
            self.cancel();
        }
        result
    }

    fn cancel(&mut self) {
        self.input = None;
        self.input_credit = None;
        self.output.close();
        while self.output.try_recv().is_ok() {}
        self.finished = true;
    }

    fn submit(&mut self, input: Option<Input>, eof: bool) -> pingora::Result<()> {
        self.pending = true;
        let Some(input) = input else {
            self.empty = true;
            return Ok(());
        };
        self.input
            .as_ref()
            .ok_or_else(|| proxy_error(Failure::Finished))?
            .try_send(input)
            .map_err(|_| proxy_error(Failure::Cancelled))?;
        if eof {
            self.input = None;
            self.input_credit = None;
        }
        Ok(())
    }

    pub async fn next(&mut self) -> pingora::Result<Event> {
        if self.finished {
            return Err(proxy_error(Failure::Finished));
        }
        if self.empty {
            self.empty = false;
            self.pending = false;
            return Ok(Event::InputDone);
        }
        let result = tokio::time::timeout(Duration::from_secs(2), self.output.recv()).await;
        match result {
            Ok(Some(Ok(event))) => {
                match event {
                    Event::Finished => {
                        self.finished = true;
                        self.pending = false;
                    }
                    Event::InputDone => self.pending = false,
                    _ => (),
                }
                Ok(event)
            }
            _ => {
                self.cancel();
                Err(proxy_error(match result {
                    Ok(Some(Err(failure))) => failure,
                    Ok(None) => Failure::Cancelled,
                    _ => Failure::Cpu,
                }))
            }
        }
    }
}

fn run(
    snapshot: &RuntimeSnapshot,
    selection: &Selection,
    input: mpsc::Receiver<Input>,
    events: &mpsc::Sender<Result<Event>>,
    budget: &Arc<ResponseBudget>,
    output_credit: Option<Arc<Credit>>,
) -> Result<()> {
    let clock = Clock::new();
    let consumed = Rc::new(Cell::new(0usize));
    let source = Source::new(input, events.clone(), clock.clone(), consumed.clone());
    let destination =
        Destination::new(budget.clone(), events.clone(), clock.clone(), output_credit);
    let mut destination =
        Encoder::new(destination, selection.encode).map_err(|_| Failure::Codec)?;
    let mut stages = replace::stages(&selection.replacements, &snapshot.policy, budget)?;
    let output_limit = OutputLimit::new();
    let mut decoded = Buffer::new(budget, CHUNK).ok_or(Failure::Budget)?;
    decoded.data_mut().resize(CHUNK, 0);
    let mut arena = PendingArena::new(
        budget,
        replace::has_unbounded(&selection.replacements, &snapshot.policy),
    )?;
    clock
        .pause(|| events.blocking_send(Ok(Event::Ready)))
        .map_err(|_| Failure::Cancelled)?;
    let mut source = decoder(source, selection.decode).map_err(|_| Failure::Codec)?;
    let mut total = 0usize;
    loop {
        clock.check()?;
        let n = source
            .read(decoded.data_mut())
            .map_err(|_| Failure::Codec)?;
        clock.check()?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n)
            .filter(|&n| {
                n <= TOTAL && n <= consumed.get().saturating_mul(100).saturating_add(64 * 1024)
            })
            .ok_or(Failure::Output)?;
        replace::push(
            &mut stages,
            &decoded.data()[..n],
            false,
            &mut arena,
            &clock,
            &output_limit,
            &mut destination,
        )?;
        destination.flush().map_err(|_| Failure::Codec)?;
    }
    drop(source);
    drop(decoded);
    replace::push(
        &mut stages,
        &[],
        true,
        &mut arena,
        &clock,
        &output_limit,
        &mut destination,
    )?;
    destination.finish().map_err(|_| Failure::Codec)?;
    clock.check()?;
    clock
        .pause(|| events.blocking_send(Ok(Event::Finished)))
        .map_err(|_| Failure::Cancelled)?;
    Ok(())
}
