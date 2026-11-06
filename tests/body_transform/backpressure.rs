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
use std::{fs, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_output_channel_applies_backpressure_without_consuming_cpu_budget() -> TestResult {
    let directory = Directory::new()?;
    let entry = directory.0.join("config.kdl");
    fs::write(
        &entry,
        format!(
            r#"server {{ http "127.0.0.1:8080"; }}; site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "a" "{}"; }}; respond 200; }}; }}; }}"#,
            "x".repeat(32)
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
    pipeline.push(Some(Bytes::from(vec![b'a'; 65536])), true)?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut total = 0;
    loop {
        match pipeline.next().await? {
            Event::Chunk(bytes) => {
                assert!(bytes.len() <= 65536);
                assert!(bytes.iter().all(|&b| b == b'x'));
                total += bytes.len();
            }
            Event::Finished => break,
            _ => return Err("unexpected input request while draining EOF expansion".into()),
        }
    }
    assert_eq!(total, 65536 * 32);
    Ok(())
}

#[test]
fn slow_http_reader_backpressures_the_transform_and_then_finishes_successfully() -> TestResult {
    use std::io::Write;
    let directory = Directory::new()?;
    fs::write(directory.0.join("large.html"), vec![b'a'; 65536])?;
    let address = vacant()?;
    let configuration = format!(
        r#"server {{ http "{address}"; }}
site "app.test" {{ chain {{ rule {{ body-replace mime="text/html" {{ literal "a" "{}"; }}; serve-files root="{}"; }}; }}; }}"#,
        "x".repeat(128),
        directory.0.display()
    );
    let _proxy = Proxy::start(address, &configuration)?;
    let mut stream = connect(address)?;
    socket2::SockRef::from(&stream).set_recv_buffer_size(65536)?;
    stream.write_all(b"GET /large.html HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")?;
    std::thread::sleep(Duration::from_secs(3));
    let (headers, body) = super::http::read_body(&mut stream)?;
    assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
    assert_eq!(body.len(), 65536 * 128);
    assert!(body.iter().all(|&b| b == b'x'));
    Ok(())
}
