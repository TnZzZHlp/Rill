use super::{
    arena::PendingArena,
    budget::{Buffer, ResponseBudget},
    limits::{CHUNK, Clock, Failure, OutputLimit, Result},
    literal::{self, Literal},
    model::{Replace, RuleKind},
    regex_stream::RegexStream,
};
use crate::http_policy::compile::PreparedPolicy;
use std::{io::Write, sync::Arc};

enum Engine<'a> {
    Literal(Literal<'a>),
    Regex(Box<RegexStream<'a>>),
}
pub(crate) struct Stage<'a> {
    engine: Option<Engine<'a>>,
    output: Buffer,
}

pub(crate) fn has_unbounded(policies: &[Arc<Replace>], compiled: &PreparedPolicy) -> bool {
    policies
        .iter()
        .flat_map(|policy| &policy.rules)
        .any(|rule| {
            matches!(&rule.kind, RuleKind::Regex(_, _))
                && compiled
                    .indices
                    .get(&(rule.origin.source.path.clone(), rule.origin.offset))
                    .is_some_and(|&index| compiled.regexes[index].maximum_len.is_none())
        })
}

pub(crate) fn stages<'a>(
    policies: &'a [Arc<Replace>],
    compiled: &'a PreparedPolicy,
    budget: &Arc<ResponseBudget>,
) -> Result<Vec<Stage<'a>>> {
    let mut stages = Vec::new();
    for policy in policies {
        for rule in &policy.rules {
            if stages.len() >= 16 {
                return Err(Failure::Budget);
            }
            let engine = match &rule.kind {
                RuleKind::Literal(pattern, replacement) => Engine::Literal(
                    Literal::new(pattern.as_bytes(), replacement.as_bytes(), budget)
                        .map_err(|_| Failure::Budget)?,
                ),
                RuleKind::Regex(_, replacement) => {
                    let index = compiled
                        .indices
                        .get(&(rule.origin.source.path.clone(), rule.origin.offset))
                        .ok_or(Failure::Finished)?;
                    Engine::Regex(Box::new(RegexStream::new(
                        &compiled.regexes[*index],
                        replacement,
                        budget,
                        stages.len(),
                    )?))
                }
            };
            stages.push(Stage {
                engine: Some(engine),
                output: Buffer::new(budget, CHUNK).ok_or(Failure::Budget)?,
            });
        }
    }
    Ok(stages)
}

pub(crate) fn push(
    stages: &mut [Stage<'_>],
    input: &[u8],
    eof: bool,
    arena: &mut PendingArena,
    clock: &Clock,
    output_limit: &OutputLimit,
    destination: &mut dyn Write,
) -> Result<()> {
    clock.check()?;
    let Some((stage, tail)) = stages.split_first_mut() else {
        destination.write_all(input).map_err(|_| Failure::Codec)?;
        return Ok(());
    };
    match stage.engine.as_mut().ok_or(Failure::Finished)? {
        Engine::Literal(engine) => {
            let output = &mut stage.output;
            engine
                .push(input, eof, |bytes| {
                    output_limit
                        .add(bytes.len())
                        .map_err(|_| literal::Error::Output)?;
                    for mut chunk in bytes.chunks(CHUNK) {
                        while !chunk.is_empty() {
                            clock.check().map_err(|_| literal::Error::Cpu)?;
                            let n = chunk.len().min(CHUNK - output.data().len());
                            output.data_mut().extend_from_slice(&chunk[..n]);
                            chunk = &chunk[n..];
                            if output.data().len() == CHUNK {
                                push(
                                    tail,
                                    output.data(),
                                    false,
                                    arena,
                                    clock,
                                    output_limit,
                                    destination,
                                )
                                .map_err(|_| literal::Error::Sink)?;
                                output.data_mut().clear();
                            }
                        }
                    }
                    Ok(())
                })
                .map_err(|error| match error {
                    literal::Error::Cpu => Failure::Cpu,
                    literal::Error::Output => Failure::Output,
                    _ => Failure::Budget,
                })?;
        }
        Engine::Regex(engine) => {
            engine.begin(input, eof, arena, clock)?;
            while let Some(fragment) = engine.next(arena, clock)? {
                let length = engine.fragment(&fragment, arena).len();
                output_limit.add(length)?;
                let mut position = 0;
                while position < length {
                    clock.check()?;
                    let n = (length - position).min(CHUNK - stage.output.data().len());
                    stage.output.data_mut().extend_from_slice(
                        &engine.fragment(&fragment, arena)[position..position + n],
                    );
                    position += n;
                    if stage.output.data().len() == CHUNK {
                        push(
                            tail,
                            stage.output.data(),
                            false,
                            arena,
                            clock,
                            output_limit,
                            destination,
                        )?;
                        stage.output.data_mut().clear();
                    }
                }
            }
            engine.end(arena)?;
        }
    }
    if eof {
        stage.engine = None;
    }
    if eof || !stage.output.data().is_empty() {
        push(
            tail,
            stage.output.data(),
            eof,
            arena,
            clock,
            output_limit,
            destination,
        )?;
    }
    stage.output.data_mut().clear();
    clock.check()
}
