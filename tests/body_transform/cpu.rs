use crate::support::*;
use bytes::Bytes;
use rill::{
    body_transform::{
        headers::{Coding, Selection},
        pipeline::Pipeline,
    },
    config::prepare::build,
    http_policy::model::Action,
    runtime::state::{HttpPreparer, RuntimeState},
};
use std::{fs, sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_wait_timeout_cancels_results_without_detaching_the_running_regex_owner() -> TestResult
{
    const CHILD: &str = "RILL_BODY_CPU_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["body_transform::cpu::cpu_wait_timeout_cancels_results_without_detaching_the_running_regex_owner", "--exact", "--test-threads=1"])
            .env(CHILD, "1").status()?;
        assert!(status.success());
        return Ok(());
    }
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    fs::write(
        &entry,
        r#"server { http "127.0.0.1:8080"; }; site "app.test" { chain { rule { body-replace mime="text/html" { regex "(?:a?){1000}a{1000}b" "x"; }; respond 200; }; }; }"#,
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
    let started = tokio::time::Instant::now();
    let result = pipeline.next().await;
    assert!(
        result.is_err(),
        "expensive regex escaped the execution budget"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waiting did not time out"
    );
    assert!(pipeline.next().await.is_err(), "late output was published");
    drop(pipeline);
    let snapshot = weak
        .upgrade()
        .ok_or("timeout released a still-running computation owner")?;
    let mut idle = Vec::new();
    for _ in 0..63 {
        idle.push(
            Pipeline::start(
                snapshot.clone(),
                Selection {
                    replacements: Vec::new(),
                    decode: Coding::Identity,
                    encode: Coding::Identity,
                    vary: false,
                },
            )
            .await?,
        );
    }
    assert!(
        Pipeline::start(
            snapshot.clone(),
            Selection {
                replacements: Vec::new(),
                decode: Coding::Identity,
                encode: Coding::Identity,
                vary: false
            }
        )
        .await
        .is_err(),
        "timed-out worker released its response permit early"
    );
    drop(idle);
    drop(snapshot);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while weak.upgrade().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        weak.upgrade().is_none(),
        "cancelled computation never released its owner"
    );
    Ok(())
}
