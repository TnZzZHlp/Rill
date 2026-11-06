use super::budget::{BufferPermit, ResponseBudget};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const CHUNK_LIMIT: usize = 64 * 1024;
const CPU_LIMIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Budget,
    Output,
    Cpu,
    Finished,
    Sink,
}

pub struct Literal<'a> {
    pattern: &'a [u8],
    replacement: &'a [u8],
    prefix: Box<[usize]>,
    matched: usize,
    _permit: BufferPermit,
    output: usize,
    cpu: Duration,
    finished: bool,
}
impl<'a> Literal<'a> {
    pub fn new(
        pattern: &'a [u8],
        replacement: &'a [u8],
        budget: &Arc<ResponseBudget>,
    ) -> Result<Self, Error> {
        let bytes = pattern
            .len()
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or(Error::Budget)?;
        let permit = budget.reserve(bytes).ok_or(Error::Budget)?;
        let start = Instant::now();
        let mut prefix = vec![0; pattern.len()].into_boxed_slice();
        let mut matched = 0;
        for index in 1..pattern.len() {
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix[index] = matched;
            if index % 4096 == 0 && start.elapsed() > CPU_LIMIT {
                return Err(Error::Cpu);
            }
        }
        Ok(Self {
            pattern,
            replacement,
            prefix,
            matched: 0,
            _permit: permit,
            output: 0,
            cpu: start.elapsed(),
            finished: false,
        })
    }
    pub fn push<F>(&mut self, input: &[u8], eof: bool, sink: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        if self.finished {
            return Err(Error::Finished);
        }
        let result = self.process(input, eof, sink);
        if result.is_err() || eof {
            self.finished = true;
        }
        result
    }
    fn process<F>(&mut self, mut input: &[u8], eof: bool, mut sink: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        let mut active = Instant::now();
        if self.pattern.is_empty() {
            if self.replacement.is_empty() {
                self.emit(input, &mut sink, &mut active)?;
            } else {
                for byte in input {
                    self.emit(self.replacement, &mut sink, &mut active)?;
                    self.emit(std::slice::from_ref(byte), &mut sink, &mut active)?;
                }
                if eof {
                    self.emit(self.replacement, &mut sink, &mut active)?;
                }
            }
        } else {
            while !input.is_empty() {
                self.check(active)?;
                if self.matched == 0 {
                    let run = input
                        .iter()
                        .position(|&byte| byte == self.pattern[0])
                        .unwrap_or(input.len());
                    self.emit(&input[..run], &mut sink, &mut active)?;
                    input = &input[run..];
                    if input.is_empty() {
                        break;
                    }
                }
                let byte = input[0];
                while self.matched > 0 && byte != self.pattern[self.matched] {
                    let next = self.prefix[self.matched - 1];
                    self.emit(&self.pattern[..self.matched - next], &mut sink, &mut active)?;
                    self.matched = next;
                    self.check(active)?;
                }
                if byte == self.pattern[self.matched] {
                    self.matched += 1;
                    if self.matched == self.pattern.len() {
                        self.emit(self.replacement, &mut sink, &mut active)?;
                        self.matched = 0;
                    }
                } else {
                    self.emit(&input[..1], &mut sink, &mut active)?;
                }
                input = &input[1..];
            }
            if eof {
                self.emit(&self.pattern[..self.matched], &mut sink, &mut active)?;
                self.matched = 0;
            }
        }
        self.cpu = self.cpu.saturating_add(active.elapsed());
        self.check(Instant::now())
    }
    fn check(&self, active: Instant) -> Result<(), Error> {
        if self.cpu.saturating_add(active.elapsed()) > CPU_LIMIT {
            Err(Error::Cpu)
        } else {
            Ok(())
        }
    }
    fn emit<F>(&mut self, bytes: &[u8], sink: &mut F, active: &mut Instant) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        self.output = self
            .output
            .checked_add(bytes.len())
            .filter(|&n| n <= OUTPUT_LIMIT)
            .ok_or(Error::Output)?;
        for chunk in bytes.chunks(CHUNK_LIMIT) {
            self.cpu = self.cpu.saturating_add(active.elapsed());
            if self.cpu > CPU_LIMIT {
                return Err(Error::Cpu);
            }
            sink(chunk)?;
            *active = Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_prefix_and_empty_byte_boundaries_are_partition_independent() {
        for (input, pattern, replacement, expected) in [
            (&b""[..], &b""[..], &b"X"[..], &b"X"[..]),
            ("é".as_bytes(), b"", b"X", b"X\xc3X\xa9X"),
            (
                "a猫猫b".as_bytes(),
                "猫".as_bytes(),
                "猫!".as_bytes(),
                "a猫!猫!b".as_bytes(),
            ),
            (b"aaabaaaab", b"aaab", b"X", b"XaX"),
            (b"abcabcabd", b"abcabd", b"X", b"abcX"),
        ] {
            for split in 0..=input.len() {
                let budget = ResponseBudget::acquire().unwrap();
                let mut literal = Literal::new(pattern, replacement, &budget).unwrap();
                let mut output = Vec::new();
                for (chunk, eof) in [
                    (&input[..split], false),
                    (&[][..], false),
                    (&input[split..], true),
                ] {
                    literal
                        .push(chunk, eof, |b| {
                            output.extend_from_slice(b);
                            Ok(())
                        })
                        .unwrap();
                }
                assert_eq!(output, expected, "split={split}");
            }
        }
    }
    #[test]
    fn literal_error_poisons_the_instance_without_replaying_output() {
        let budget = ResponseBudget::acquire().unwrap();
        let mut literal = Literal::new(b"a", b"b", &budget).unwrap();
        assert_eq!(
            literal.push(b"a", false, |_| Err(Error::Sink)),
            Err(Error::Sink)
        );
        assert_eq!(
            literal.push(b"a", true, |_| panic!("replayed")),
            Err(Error::Finished)
        );
    }
}
