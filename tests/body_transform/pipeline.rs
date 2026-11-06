use crate::support::*;
use bytes::Bytes;
use rill::{
    body_transform::{
        headers::{Coding, Selection},
        pipeline::{Event, Pipeline},
    },
    config::prepare::build,
    http_policy::model::Action,
    runtime::state::{HttpPreparer, RuntimeSnapshot, RuntimeState},
};
use std::{fs, io::Write, sync::Arc};

async fn run(
    snapshot: Arc<RuntimeSnapshot>,
    decode: Coding,
    encode: Coding,
    input: &[u8],
    partition: usize,
) -> TestResult<Vec<u8>> {
    let rules = snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("chain")?
        .rules
        .iter()
        .flat_map(|r| &r.actions)
        .filter_map(|a| {
            if let Action::BodyReplace(r) = a {
                Some(r.clone())
            } else {
                None
            }
        })
        .collect();
    let mut pipeline = Pipeline::start(
        snapshot,
        Selection {
            replacements: rules,
            decode,
            encode,
            vary: true,
        },
    )
    .await?;
    let mut output = Vec::new();
    for (index, chunk) in input.chunks(partition).enumerate() {
        pipeline.push(
            Some(Bytes::copy_from_slice(chunk)),
            (index + 1) * partition >= input.len(),
        )?;
        loop {
            match pipeline.next().await? {
                Event::Chunk(b) => {
                    assert!(b.len() <= 64 * 1024);
                    output.extend_from_slice(&b);
                }
                Event::InputDone | Event::Finished => break,
                Event::Ready => return Err("unexpected ready".into()),
            }
        }
    }
    if input.is_empty() {
        pipeline.push(None, true)?;
        loop {
            match pipeline.next().await? {
                Event::Chunk(b) => output.extend_from_slice(&b),
                Event::Finished => break,
                _ => return Err("unexpected event".into()),
            }
        }
    }
    Ok(output)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipeline_decodes_validates_and_encodes_with_shared_rules_and_chunk_independence()
-> TestResult {
    let dir = Directory::new()?;
    let entry = dir.0.join("config.kdl");
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { literal "old" "cat"; regex "(cat)" "$1!"; }; respond 200; }; }; }"#,
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    let input = b"old cat old";
    for decode in [Coding::Identity, Coding::Gzip, Coding::Zstd] {
        let encoded = match decode {
            Coding::Identity => input.to_vec(),
            Coding::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
                encoder.write_all(input)?;
                encoder.finish()?
            }
            Coding::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
                encoder.window_log(20)?;
                encoder.write_all(input)?;
                encoder.finish()?
            }
        };
        for encode in [Coding::Identity, Coding::Gzip, Coding::Zstd] {
            for partition in [1, 7, 65536] {
                let output = run(snapshot.clone(), decode, encode, &encoded, partition)
                    .await
                    .map_err(|error| {
                        format!(
                            "decode={decode:?} encode={encode:?} partition={partition}: {error}"
                        )
                    })?;
                let decoded = match encode {
                    Coding::Identity => output,
                    Coding::Gzip => {
                        use std::io::Read;
                        let mut bytes = Vec::new();
                        flate2::read::GzDecoder::new(output.as_slice()).read_to_end(&mut bytes)?;
                        bytes
                    }
                    Coding::Zstd => zstd::stream::decode_all(output.as_slice())?,
                };
                assert_eq!(decoded, b"cat! cat! cat!");
            }
        }
        if decode != Coding::Identity {
            assert!(
                run(
                    snapshot.clone(),
                    decode,
                    Coding::Identity,
                    &encoded[..encoded.len() - 1],
                    1
                )
                .await
                .is_err()
            );
            let mut trailing = encoded.clone();
            trailing.extend_from_slice(b"garbage");
            assert!(
                run(snapshot.clone(), decode, Coding::Identity, &trailing, 7)
                    .await
                    .is_err()
            );
        }
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
    encoder.write_all(input)?;
    let mut corrupt = encoder.finish()?;
    let end = corrupt.len();
    corrupt[end - 8] ^= 1;
    assert!(
        run(
            snapshot.clone(),
            Coding::Gzip,
            Coding::Identity,
            &corrupt,
            7
        )
        .await
        .is_err()
    );
    let mut first = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
    first.write_all(b"ol")?;
    let mut members = first.finish()?;
    let mut second = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
    second.write_all(b"d")?;
    members.extend_from_slice(&second.finish()?);
    assert_eq!(
        run(
            snapshot.clone(),
            Coding::Gzip,
            Coding::Identity,
            &members,
            1
        )
        .await?,
        b"cat!"
    );
    let mut window = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    window.window_log(21)?;
    window.write_all(input)?;
    let oversized_window = window.finish()?;
    assert!(
        run(
            snapshot.clone(),
            Coding::Zstd,
            Coding::Identity,
            &oversized_window,
            65536
        )
        .await
        .is_err()
    );
    for coding in [Coding::Gzip, Coding::Zstd] {
        let data = vec![b'a'; 128 * 1024];
        let bomb = if coding == Coding::Gzip {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
            encoder.write_all(&data)?;
            encoder.finish()?
        } else {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
            encoder.window_log(20)?;
            encoder.write_all(&data)?;
            encoder.finish()?
        };
        assert!(
            run(snapshot.clone(), coding, Coding::Identity, &bomb, 65536)
                .await
                .is_err(),
            "{coding:?} expansion limit"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_with_blocked_expansion_keeps_snapshot_until_last_output_clone() -> TestResult
{
    let dir = Directory::new()?;
    let entry = dir.0.join("config.kdl");
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { regex "(a+)" "$1!"; }; respond 200; }; }; }"#,
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    let Action::BodyReplace(rule) = &snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("chain")?
        .rules[0]
        .actions[0]
    else {
        return Err("replace action".into());
    };
    let selection = Selection {
        replacements: vec![rule.clone()],
        decode: Coding::Identity,
        encode: Coding::Identity,
        vary: true,
    };
    let weak = Arc::downgrade(&snapshot);
    let mut pipeline = Pipeline::start(snapshot, selection).await?;
    pipeline.push(Some(Bytes::from(vec![b'a'; 65536])), true)?;
    let first = loop {
        if let Event::Chunk(bytes) = pipeline.next().await? {
            break bytes;
        }
    };
    let retained = first.clone();
    drop(first);
    drop(pipeline);
    assert!(weak.upgrade().is_some());
    drop(retained);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while weak.upgrade().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        weak.upgrade().is_none(),
        "cancelled codec task retained its snapshot"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbounded_arena_removes_double_allocation_and_has_one_shared_working_boundary()
-> TestResult {
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { regex "a*" "x"; }; respond 200; }; }; }"#,
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    let capacity = 1024 * 1024 - 64 * 1024 - 16 * 1024 - 16 * 1024 - 48 * 1024;
    let batch = regex::bytes::Regex::new("a*")?;
    for length in [496 * 1024, capacity - 1, capacity, capacity + 1] {
        let input = vec![b'a'; length];
        let expected = batch.replace_all(&input, &b"x"[..]).into_owned();
        for partition in [16 * 1024, 64 * 1024] {
            let result = run(
                snapshot.clone(),
                Coding::Identity,
                Coding::Identity,
                &input,
                partition,
            )
            .await;
            if length <= capacity {
                assert_eq!(result?, expected, "length={length} partition={partition}");
            } else {
                assert!(
                    result.is_err(),
                    "one-byte arena overflow length={length} partition={partition}"
                );
            }
        }
    }
    let input = vec![b'a'; 4096];
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(3));
    gzip.write_all(&input)?;
    let gzip = gzip.finish()?;
    let zstd = run(snapshot.clone(), Coding::Gzip, Coding::Zstd, &gzip, 1).await?;
    assert_eq!(zstd::stream::decode_all(zstd.as_slice())?, b"x");
    let mut encoded = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    encoded.window_log(20)?;
    encoded.write_all(&input)?;
    let encoded = encoded.finish()?;
    let output = run(snapshot, Coding::Zstd, Coding::Gzip, &encoded, 1).await?;
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(output.as_slice()),
        &mut decoded,
    )?;
    assert_eq!(decoded, b"x");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbounded_stages_share_regions_and_preserve_capture_offsets_across_compaction()
-> TestResult {
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    let patterns = [("(a+)", "$1!"), ("(?s:(.*))", "$1?"), ("(a+)", "$1.")];
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { regex "(a+)" "$1!"; regex "(?s:(.*))" "$1?"; regex "(a+)" "$1."; }; respond 200; }; }; }"#,
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    for input in [Vec::new(), b"a ba".to_vec(), vec![b'a'; 400 * 1024]] {
        let mut expected = input.clone();
        for (pattern, replacement) in patterns {
            expected = regex::bytes::Regex::new(pattern)?
                .replace_all(&expected, replacement.as_bytes())
                .into_owned();
        }
        for partition in [16 * 1024, 64 * 1024] {
            assert_eq!(
                run(
                    snapshot.clone(),
                    Coding::Identity,
                    Coding::Identity,
                    &input,
                    partition
                )
                .await?,
                expected
            );
        }
    }
    let input = vec![b'a'; 450 * 1024];
    for partition in [16 * 1024, 64 * 1024] {
        assert!(
            run(
                snapshot.clone(),
                Coding::Identity,
                Coding::Identity,
                &input,
                partition
            )
            .await
            .is_err(),
            "simultaneously live upstream and downstream regions exceeded the shared arena"
        );
    }
    Ok(())
}
