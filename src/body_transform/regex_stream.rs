use super::{
    arena::PendingArena,
    budget::{Buffer, ResponseBudget},
    limits::{Clock, Failure, Result},
    replacement::Part,
};
use crate::http_policy::{compile::CompiledRegex, workspace};
use regex_automata::{Input, nfa::thompson::pikevm::Cache, util::captures::Captures};
use std::{ops::Range, sync::Arc};

enum Storage {
    Bounded(Buffer),
    Arena(usize),
}
#[derive(Clone)]
pub(crate) enum Fragment {
    Source(Range<usize>),
    Literal(usize),
}
struct MatchOutput {
    end: usize,
    part: usize,
    prefix: bool,
    start: usize,
}

pub(crate) struct RegexStream<'a> {
    regex: &'a CompiledRegex,
    parts: &'a [Part],
    cache: Cache,
    captures: Captures,
    _workspace: workspace::Permit,
    storage: Option<Storage>,
    cursor: usize,
    last_end: Option<usize>,
    matched: Option<MatchOutput>,
    safe: usize,
    eof: bool,
    drained: bool,
}
impl<'a> RegexStream<'a> {
    pub fn new(
        regex: &'a CompiledRegex,
        parts: &'a [Part],
        budget: &Arc<ResponseBudget>,
        region: usize,
    ) -> Result<Self> {
        let permit = workspace::reserve(&regex.vm).ok_or(Failure::Budget)?;
        let storage = if let Some(max) = regex.maximum_len {
            Storage::Bounded(
                Buffer::new(
                    budget,
                    max.saturating_add(super::limits::CHUNK).saturating_add(8),
                )
                .ok_or(Failure::Budget)?,
            )
        } else {
            Storage::Arena(region)
        };
        Ok(Self {
            regex,
            parts,
            cache: regex.vm.create_cache(),
            captures: regex.vm.create_captures(),
            _workspace: permit,
            storage: Some(storage),
            cursor: 0,
            last_end: None,
            matched: None,
            safe: 0,
            eof: false,
            drained: true,
        })
    }
    fn data<'b>(&'b self, arena: &'b PendingArena) -> &'b [u8] {
        match self.storage.as_ref() {
            Some(Storage::Bounded(b)) => b.data(),
            Some(Storage::Arena(id)) => arena.data(*id),
            None => &[],
        }
    }
    pub fn begin(
        &mut self,
        input: &[u8],
        eof: bool,
        arena: &mut PendingArena,
        clock: &Clock,
    ) -> Result<()> {
        if !self.drained || self.eof || self.storage.is_none() {
            return Err(Failure::Finished);
        }
        match self.storage.as_mut().ok_or(Failure::Finished)? {
            Storage::Bounded(buffer) => {
                if buffer
                    .data()
                    .len()
                    .checked_add(input.len())
                    .is_none_or(|n| n > buffer.data_mut().capacity())
                {
                    return Err(Failure::Budget);
                }
                buffer.data_mut().extend_from_slice(input);
            }
            Storage::Arena(id) => arena.append(*id, input, clock)?,
        }
        self.eof = eof;
        self.safe = if eof {
            self.data(arena).len()
        } else {
            self.regex.maximum_len.map_or(0, |max| {
                self.data(arena).len().saturating_sub(max.saturating_add(4))
            })
        };
        self.drained = !eof && self.regex.maximum_len.is_none();
        Ok(())
    }
    fn search(&mut self, start: usize, arena: &PendingArena) -> Result<()> {
        let data = match self.storage.as_ref().ok_or(Failure::Finished)? {
            Storage::Bounded(b) => b.data(),
            Storage::Arena(id) => arena.data(*id),
        };
        self.regex.vm.captures(
            &mut self.cache,
            Input::new(data).span(start..data.len()),
            &mut self.captures,
        );
        Ok(())
    }
    pub fn next(&mut self, arena: &PendingArena, clock: &Clock) -> Result<Option<Fragment>> {
        loop {
            clock.check()?;
            if self.drained {
                return Ok(None);
            }
            if let Some(output) = &mut self.matched {
                if output.prefix {
                    output.prefix = false;
                    if self.cursor < output.start {
                        return Ok(Some(Fragment::Source(self.cursor..output.start)));
                    }
                }
                while let Some(part) = self.parts.get(output.part) {
                    let index = output.part;
                    output.part += 1;
                    match part {
                        Part::Literal(bytes) if !bytes.is_empty() => {
                            return Ok(Some(Fragment::Literal(index)));
                        }
                        Part::Capture(group) => {
                            if let Some(span) = self.captures.get_group(usize::from(*group))
                                && !span.is_empty()
                            {
                                return Ok(Some(Fragment::Source(span.start..span.end)));
                            }
                        }
                        _ => (),
                    }
                }
                self.cursor = output.end;
                self.last_end = Some(output.end);
                self.matched = None;
            }
            let len = self.data(arena).len();
            if self.cursor > len || (!self.eof && self.cursor >= self.safe) {
                self.drained = true;
                return Ok(None);
            }
            self.search(self.cursor, arena)?;
            clock.check()?;
            if let Some(found) = self.captures.get_match()
                && found.is_empty()
                && self.last_end == Some(found.end())
            {
                let next = found.end().checked_add(1).ok_or(Failure::Budget)?;
                if next > len {
                    self.drained = true;
                    return Ok(None);
                }
                self.search(next, arena)?;
                clock.check()?;
            }
            if let Some(found) = self.captures.get_match()
                && (self.eof || found.start() < self.safe)
            {
                self.matched = Some(MatchOutput {
                    start: found.start(),
                    end: found.end(),
                    part: 0,
                    prefix: true,
                });
            } else {
                let end = if self.eof { len } else { self.safe };
                let start = self.cursor;
                self.cursor = end;
                self.drained = true;
                return Ok((end > start).then_some(Fragment::Source(start..end)));
            }
        }
    }
    pub fn fragment<'b>(&'b self, fragment: &Fragment, arena: &'b PendingArena) -> &'b [u8] {
        match fragment {
            Fragment::Source(span) => &self.data(arena)[span.clone()],
            Fragment::Literal(index) => match &self.parts[*index] {
                Part::Literal(bytes) => bytes,
                Part::Capture(_) => &[],
            },
        }
    }
    pub fn end(&mut self, arena: &mut PendingArena) -> Result<()> {
        if !self.drained {
            return Err(Failure::Finished);
        }
        if self.eof {
            if let Some(Storage::Arena(id)) = self.storage.take() {
                arena.release(id);
            }
            self.storage = None;
        } else if let Some(Storage::Bounded(buffer)) = &mut self.storage {
            let trim = self.cursor.saturating_sub(4);
            if trim > 0 {
                let len = buffer.data().len();
                buffer.data_mut().copy_within(trim..len, 0);
                buffer.data_mut().truncate(len - trim);
                self.cursor -= trim;
                self.last_end = self.last_end.and_then(|end| end.checked_sub(trim));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex_automata::{
        nfa::thompson::{self, pikevm::PikeVM},
        util::syntax,
    };

    #[test]
    fn regex_stream_matches_bytes_batch_for_partitions_assertions_captures_and_empty_matches() {
        for pattern in [
            "",
            "a{0,3}",
            "a{0,3}?",
            "aa|a",
            "a|aa",
            "(a{1,3})(b)?",
            r"\A(a{0,2})|b\z",
            r"(?m)^a{0,2}$",
            r"\b(\w{0,3})\b",
            r"(?-u:\b.\b)",
            r"(?-u:\xFF{0,2})",
            "a*",
            "(a|b)+",
            r"(?mR)^a{0,3}$",
            r"a{0,2}(?:\b|\B)",
            r"\A|\z",
            r"(?-u:)|a",
        ] {
            let vm = PikeVM::builder()
                .syntax(syntax::Config::new().utf8(false))
                .thompson(thompson::Config::new().utf8(false))
                .build(pattern)
                .unwrap();
            let maximum_len = regex_syntax::ParserBuilder::new()
                .utf8(false)
                .build()
                .parse(pattern)
                .unwrap()
                .properties()
                .maximum_len();
            let compiled = CompiledRegex { vm, maximum_len };
            let batch = regex::bytes::Regex::new(pattern).unwrap();
            let parts = crate::body_transform::replacement::parse("<$1!$9!$$>").unwrap();
            for input in [
                &b"aaaaab aa\nb\na\n\xff\xff!"[..],
                "éa 猫aa b\n".as_bytes(),
                &b""[..],
            ] {
                let expected = batch.replace_all(input, &b"<$1!$9!$$>"[..]);
                for partition in 0..=input.len() + 1 {
                    let budget = ResponseBudget::acquire().unwrap();
                    let mut stream = RegexStream::new(&compiled, &parts, &budget, 0).unwrap();
                    let mut arena = PendingArena::new(&budget, maximum_len.is_none()).unwrap();
                    let mut actual = Vec::new();
                    let clock = Clock::new();
                    let chunks = if partition <= input.len() {
                        vec![&input[..partition], &[][..], &input[partition..]]
                    } else {
                        input.chunks(1).collect()
                    };
                    for (chunk, eof) in chunks
                        .into_iter()
                        .map(|chunk| (chunk, false))
                        .chain(std::iter::once((&[][..], true)))
                    {
                        stream.begin(chunk, eof, &mut arena, &clock).unwrap();
                        while let Some(fragment) = stream.next(&arena, &clock).unwrap() {
                            actual.extend_from_slice(stream.fragment(&fragment, &arena));
                        }
                        stream.end(&mut arena).unwrap();
                    }
                    assert_eq!(
                        actual.as_slice(),
                        expected.as_ref(),
                        "pattern={pattern:?} partition={partition} input={input:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn bounded_regex_emits_before_eof_without_accumulating_the_complete_body() {
        let compiled = CompiledRegex {
            vm: PikeVM::new(r"\b(a{1,3})\b").unwrap(),
            maximum_len: Some(3),
        };
        let parts = crate::body_transform::replacement::parse("<$1!>").unwrap();
        let budget = ResponseBudget::acquire().unwrap();
        let mut stream = RegexStream::new(&compiled, &parts, &budget, 0).unwrap();
        let mut arena = PendingArena::new(&budget, false).unwrap();
        let clock = Clock::new();
        let mut bytes = 0;
        for (chunk, eof) in (0..1024)
            .map(|_| (b"aa ".repeat(64), false))
            .chain(std::iter::once((Vec::new(), true)))
        {
            stream.begin(&chunk, eof, &mut arena, &clock).unwrap();
            while let Some(fragment) = stream.next(&arena, &clock).unwrap() {
                bytes += stream.fragment(&fragment, &arena).len();
            }
            stream.end(&mut arena).unwrap();
            if !eof {
                assert!(bytes > 0);
            }
        }
        assert_eq!(bytes, 1024 * 64 * 6);
    }
}
