use crate::support::*;
use bytes::Bytes;
use rill::{
    body_transform::{
        headers::{Coding, Selection},
        pipeline::{Event, Pipeline},
    },
    config::prepare::build,
    http_policy::model::Action,
    runtime::state::{HttpPreparer, RuntimeState},
};
use std::fs;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_rules_share_output_and_pending_capacity_instead_of_each_getting_a_mebibyte()
-> TestResult {
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    fs::write(
        &entry,
        format!(
            r#"server {{ http "127.0.0.1:8080"; }}; site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ {} }}; respond 200; }}; }}; }}"#,
            "literal \"z\" \"z\";".repeat(16)
        ),
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    let Action::BodyReplace(rule) = &snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("chain")?
        .rules[0]
        .actions[0]
    else {
        return Err("replace".into());
    };
    let mut pipeline = Pipeline::start(
        snapshot.clone(),
        Selection {
            replacements: vec![rule.clone()],
            decode: Coding::Identity,
            encode: Coding::Identity,
            vary: false,
        },
    )
    .await?;
    let mut total = 0;
    let mut failed = false;
    for index in 0..65 {
        pipeline.push(Some(Bytes::from(vec![b'a'; 65536])), index == 64)?;
        loop {
            match pipeline.next().await {
                Ok(Event::Chunk(bytes)) => total += bytes.len(),
                Ok(Event::InputDone) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
                _ => return Err("shared output limit was not enforced".into()),
            }
        }
        if failed {
            break;
        }
    }
    assert!(
        failed && total <= 4 * 1024 * 1024,
        "16 rule outputs did not share 64 MiB: {total}"
    );
    drop(pipeline);
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { regex "a*" "x"; regex "b*" "x"; }; respond 200; }; }; }"#,
    )?;
    let snapshot = RuntimeState::new(build(&entry, &HttpPreparer)?)?.snapshot();
    let Action::BodyReplace(rule) = &snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("chain")?
        .rules[0]
        .actions[0]
    else {
        return Err("replace".into());
    };
    let mut pipeline = Pipeline::start(
        snapshot.clone(),
        Selection {
            replacements: vec![rule.clone()],
            decode: Coding::Identity,
            encode: Coding::Identity,
            vary: false,
        },
    )
    .await?;
    let mut failed = false;
    for _ in 0..16 {
        if pipeline
            .push(Some(Bytes::from(vec![b'a'; 65536])), false)
            .is_err()
        {
            failed = true;
            break;
        }
        match pipeline.next().await {
            Ok(Event::InputDone) => (),
            Err(_) => {
                failed = true;
                break;
            }
            _ => return Err("unbounded regex output before EOF".into()),
        }
    }
    assert!(
        failed,
        "pending buffers exceeded their shared request limit"
    );
    Ok(())
}
