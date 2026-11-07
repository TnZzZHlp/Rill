use crate::{dav_support::response, reload_support::Fixture, support::TestResult};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, thread, time::Duration};

#[test]
fn authenticated_dav_transform_and_log_survive_failed_candidate_and_reload() -> TestResult {
    let mut f = Fixture::new()?;
    let root = f.dir.join("dav");
    let logs = f.dir.join("logs");
    fs::create_dir(&root)?;
    fs::create_dir(&logs)?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o700))?;
    let token = f.dir.join("token");
    fs::write(&token, "integration-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let config = format!(
        r#"server {{ http "{}"; control-socket "{}"; }}
site "example.test" {{ site-label "integration"; access-log file="{}" rotate-size=16384 retain-files=1 retain-days=1;
 chain {{ rule "dav" {{ static-token header="X-Token" file="{}"; header "response" {{ set "X-Generation" "old"; }}; body-replace mime="application/octet-stream" {{ literal "old" "new"; }}; webdav root="{}" storage-id="documents"; }}; }}; }}"#,
        f.address,
        f.socket.display(),
        logs.join("access.jsonl").display(),
        token.display(),
        root.display()
    );
    fs::write(&f.entry, &config)?;
    f.start()?;
    let mut unauth = f.connect()?;
    unauth.write_all(b"GET /file HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut unauth, "GET")?.0, 401);
    let mut put = f.connect()?;
    put.write_all(b"PUT /file HTTP/1.1\r\nHost: example.test\r\nX-Token: integration-fixture\r\nContent-Length: 3\r\nConnection: close\r\n\r\nold")?;
    assert!(matches!(response(&mut put, "PUT")?.0, 200 | 201 | 204));
    fs::write(
        &f.entry,
        config.replace(
            &token.display().to_string(),
            &f.dir.join("missing").display().to_string(),
        ),
    )?;
    assert_eq!(f.cli(&[])?.status.code(), Some(1));
    let check = |expected: &str| -> TestResult {
        let mut get = f.connect()?;
        get.write_all(b"GET /file HTTP/1.1\r\nHost: example.test\r\nX-Token: integration-fixture\r\nConnection: close\r\n\r\n")?;
        let (status, headers, body) = response(&mut get, "GET")?;
        assert_eq!(status, 200);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains(&format!("x-generation: {expected}")),
            "{headers}"
        );
        assert_eq!(body, b"new");
        Ok(())
    };
    check("old")?;
    fs::write(
        &f.entry,
        config.replace(
            "set \"X-Generation\" \"old\"",
            "set \"X-Generation\" \"next\"",
        ),
    )?;
    assert!(f.cli(&[])?.status.success());
    check("next")?;
    assert_eq!(fs::read(root.join("file"))?, b"old");
    let path = logs.join("access.jsonl");
    let mut contents = String::new();
    for _ in 0..100 {
        contents = fs::read_to_string(&path)?;
        if contents.lines().count() >= 4 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(contents.contains("\"site\":\"integration\""));
    assert!(contents.contains("\"route\":\"dav\""));
    assert!(!contents.contains("integration-fixture"));
    Ok(())
}
