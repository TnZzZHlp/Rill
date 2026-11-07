use super::support::*;
use std::{fs, os::unix::fs::PermissionsExt, process::Command, thread, time::Duration};

#[test]
fn site_file_log_check_is_read_only_and_logs_static_rule_identity() -> TestResult {
    let directory = Directory::new()?;
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))?;
    let log = directory.0.join("access.jsonl");
    let control = directory.0.join("control.sock");
    let config_path = directory.0.join("config.kdl");
    let address = vacant()?;
    let text = format!(
        "server {{ http \"{address}\"; control-socket \"{}\"; }}\nsite \"API.example.test\" {{ site-label \"public\"; access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1; chain {{ rule \"health\" {{ respond 200 text=\"ok\"; }} }} }}",
        control.display(),
        log.display()
    );
    fs::write(&config_path, &text)?;
    let check = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&config_path)
        .output()?;
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(!log.exists());
    let invalid = text.replace(
        "retain-days=1;",
        "retain-days=1 { field \"authorization\"; };",
    );
    fs::write(&config_path, invalid)?;
    let rejected = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&config_path)
        .output()?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("LOG_FIELD_FORBIDDEN"));
    assert!(!log.exists());
    fs::write(&config_path, &text)?;
    let mut proxy = Proxy::start(address, &text)?;
    assert_eq!(
        send(address, "api.example.test", "/secret?token=value")?.0,
        200
    );
    let mut content = String::new();
    for _ in 0..100 {
        content = fs::read_to_string(&log).unwrap_or_default();
        if !content.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let record: serde_json::Value = serde_json::from_str(content.trim())?;
    assert_eq!(record["site"], "public");
    assert_eq!(record["route"], "health");
    assert_eq!(record["status"], 200);
    assert!(record.get("path").is_none());
    assert!(record.get("client-ip").is_none());
    assert!(!content.contains("token"));
    proxy.signal()?;
    assert_eq!(proxy.finish()?.0, 0);
    Ok(())
}

#[test]
fn explicit_fields_use_path_without_query_and_validated_client_ip() -> TestResult {
    use std::io::Write;
    let directory = Directory::new()?;
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))?;
    let log = directory.0.join("access.jsonl");
    let address = vacant()?;
    let config = format!(
        "server {{ http \"{address}\"; observability {{ access-log #true; }} }}\nsite \"example.test\" {{ access-log file=\"{}\" rotate-size=16384 retain-files=1 retain-days=1 {{ field \"route\"; field \"path\"; field \"client-ip\"; }}; chain {{ rule {{ respond 200 text=\"ok\"; }} }} }}",
        log.display()
    );
    let mut proxy = Proxy::start(address, &config)?;
    let mut stream = connect(address)?;
    stream.write_all(b"GET /safe?token=secret HTTP/1.1\r\nHost: example.test\r\nX-Forwarded-For: 192.0.2.9\r\nConnection: close\r\n\r\n")?;
    assert_eq!(response(&mut stream)?.0, 200);
    let mut content = String::new();
    for _ in 0..100 {
        content = fs::read_to_string(&log).unwrap_or_default();
        if !content.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let record: serde_json::Value = serde_json::from_str(content.trim())?;
    assert_eq!(record["route"], "rule-1");
    assert_eq!(record["path"], "/safe");
    assert_eq!(record["client-ip"], "127.0.0.1");
    assert!(record.get("site").is_none());
    assert!(!content.contains("secret"));
    proxy.signal()?;
    let (code, stderr) = proxy.finish()?;
    assert_eq!(code, 0);
    assert!(stderr.contains("\"event\":\"access\""));
    Ok(())
}
