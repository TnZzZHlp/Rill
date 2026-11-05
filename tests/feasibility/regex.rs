use regex::bytes::{Captures, Regex, RegexBuilder};

use super::snapshot::TestResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Limit {
    Pending,
    Output,
}

struct PendingRegex {
    regex: Regex,
    replacement: Vec<u8>,
    pending: Box<[u8]>,
    used: usize,
    output_limit: usize,
    failed: Option<Limit>,
}

impl PendingRegex {
    fn prepare(
        pattern: &str,
        replacement: &[u8],
        pending: usize,
        output: usize,
    ) -> TestResult<Self> {
        let expression = regex_syntax::ParserBuilder::new()
            .utf8(false)
            .build()
            .parse(pattern)?;
        if expression.properties().maximum_len().is_some() {
            return Err("this probe only handles unbounded regex EOF fallback".into());
        }
        let regex = RegexBuilder::new(pattern)
            .size_limit(2 * 1024 * 1024)
            .dfa_size_limit(2 * 1024 * 1024)
            .build()?;
        Ok(Self {
            regex,
            replacement: replacement.to_vec(),
            pending: vec![0; pending].into_boxed_slice(),
            used: 0,
            output_limit: output,
            failed: None,
        })
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), Limit> {
        if let Some(limit) = self.failed {
            return Err(limit);
        }
        if bytes.len() > self.pending.len().saturating_sub(self.used) {
            self.failed = Some(Limit::Pending);
            return Err(Limit::Pending);
        }
        let end = self.used + bytes.len();
        self.pending[self.used..end].copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    fn finish(self, sink: &mut dyn FnMut(&[u8])) -> Result<(), Limit> {
        if let Some(limit) = self.failed {
            return Err(limit);
        }
        let mut output = Output {
            sink,
            written: 0,
            limit: self.output_limit,
        };
        let input = &self.pending[..self.used];
        let mut previous = 0;
        for capture in self.regex.captures_iter(input) {
            let matched = capture.get(0).expect("whole match always exists");
            output.append(&input[previous..matched.start()])?;
            expand(&mut output, &capture, &self.replacement)?;
            previous = matched.end();
        }
        output.append(&input[previous..])
    }
}

struct Output<'a> {
    sink: &'a mut dyn FnMut(&[u8]),
    written: usize,
    limit: usize,
}

impl Output<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<(), Limit> {
        if bytes.len() > self.limit.saturating_sub(self.written) {
            return Err(Limit::Output);
        }
        for chunk in bytes.chunks(64 * 1024) {
            (self.sink)(chunk);
        }
        self.written += bytes.len();
        Ok(())
    }
}

fn collect(transform: PendingRegex) -> Result<Vec<u8>, Limit> {
    let mut output = Vec::new();
    transform.finish(&mut |chunk| output.extend_from_slice(chunk))?;
    Ok(output)
}

fn expand(
    output: &mut Output<'_>,
    captures: &Captures<'_>,
    replacement: &[u8],
) -> Result<(), Limit> {
    let mut position = 0;
    while position < replacement.len() {
        match replacement.get(position..position + 2) {
            Some([b'$', b'$']) => {
                output.append(b"$")?;
                position += 2;
            }
            Some([b'$', digit @ b'1'..=b'9']) => {
                if let Some(capture) = captures.get(usize::from(*digit - b'0')) {
                    output.append(capture.as_bytes())?;
                }
                position += 2;
            }
            _ => {
                output.append(&replacement[position..position + 1])?;
                position += 1;
            }
        }
    }
    Ok(())
}

#[test]
fn unbounded_capture_replacement_is_identical_for_every_chunk_split() -> TestResult {
    let cases: &[(&str, &[u8], &[u8])] = &[
        ("(a+)(b*)", b"xxaaaabbbbaaaz", b"$2/$1"),
        ("(?s)(a.*)(z)", b"start a\nlong z tail z end", b"$2:$1"),
        ("(a+?)", b"baaaac", b"<$1>"),
        ("(a*|b+)", b"abba", b"[$1]"),
        ("(a+)|(b+)", b"aa___bbb__aa", b"$1:$2"),
        (
            "(\\w+)\\s+(\\w+)",
            "前缀 café 世界 后缀".as_bytes(),
            b"$2/$1/$$",
        ),
        ("(?m)^(.+)$", b"first\nsecond\n", b"<$1>"),
        ("(?-u:([\\x00-\\xff]+))", b"\xffa\x00b", b"$1$1"),
    ];
    let mut partitions = 0;
    for &(pattern, input, replacement) in cases {
        let expected = Regex::new(pattern)?
            .replace_all(input, replacement)
            .into_owned();
        for split in 0..=input.len() {
            let mut transform = PendingRegex::prepare(pattern, replacement, 1024, 4096)?;
            transform.push(&input[..split]).expect("input under budget");
            transform.push(&input[split..]).expect("input under budget");
            assert_eq!(
                collect(transform),
                Ok(expected.clone()),
                "{pattern}, split {split}"
            );
            partitions += 1;
        }
        for width in 1..=input.len().max(1) {
            let mut transform = PendingRegex::prepare(pattern, replacement, 1024, 4096)?;
            for chunk in input.chunks(width) {
                transform.push(chunk).expect("input under budget");
            }
            assert_eq!(
                collect(transform),
                Ok(expected.clone()),
                "{pattern}, width {width}"
            );
            partitions += 1;
        }
    }
    println!(
        "unbounded regex: {} patterns, {partitions} partitions match batch output",
        cases.len()
    );
    Ok(())
}

#[test]
fn pending_and_expansion_limits_fail_without_returning_truncated_success() -> TestResult {
    let mut exact = PendingRegex::prepare("(a+)", b"$1", 1024 * 1024, 1024 * 1024)?;
    for size in [600 * 1024, 400 * 1024, 24 * 1024] {
        exact
            .push(&vec![b'a'; size])
            .expect("exact pending budget allowed for uneven chunks");
    }
    assert_eq!(
        collect(exact).expect("exact output budget allowed").len(),
        1024 * 1024
    );

    let mut overflow = PendingRegex::prepare("(a+)", b"$1", 4, 16)?;
    overflow.push(b"aaaa").expect("exact budget allowed");
    assert_eq!(overflow.push(b"a"), Err(Limit::Pending));
    assert_eq!(overflow.push(b""), Err(Limit::Pending));
    assert_eq!(collect(overflow), Err(Limit::Pending));

    let mut expansion = PendingRegex::prepare("(a+)", b"$1$1", 4, 7)?;
    expansion.push(b"aaaa").expect("input under budget");
    assert_eq!(collect(expansion), Err(Limit::Output));
    println!(
        "unbounded regex: exact 1MiB pending accepted; pending/expanded output overflow rejected"
    );
    Ok(())
}
