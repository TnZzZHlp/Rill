use std::fs;
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rill::config::model::Action;
use rill::config::prepare::{ConfigurationOnly, build};
use rill::config::{load, normalize, parse, validate};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn check_accepts_single_and_multi_file_examples() -> TestResult {
    for file in ["examples/config.kdl", "examples/multi/config.kdl"] {
        let result = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config", file])
            .output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("scope: configuration"));
    }
    Ok(())
}

#[test]
fn profile_changes_rebuild_effective_route_parameters() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-002-profile-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let entry = directory.join("config.kdl");
    let profile = directory.join("profiles.kdl");
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nimport \"profiles.kdl\"\nsite \"API.example.test.\" {\nread-timeout \"3s\"\nuse \"base\"\nroute \"/*\" {\nuse \"slow\"\nproxy \"http://127.0.0.1:3000\"\n}\n}\n",
    )?;
    for (input, expected) in [
        (
            "profile \"slow\" { use \"base\"; read-timeout \"4s\"; }\nprofile \"base\" { read-timeout \"1s\"; }\n",
            4,
        ),
        (
            "profile \"slow\" { use \"base\"; read-timeout \"8s\"; }\nprofile \"base\" { read-timeout \"1s\"; }\n",
            8,
        ),
    ] {
        fs::write(&profile, input)?;
        let snapshot = build(&entry, &ConfigurationOnly)?;
        let site = &snapshot.configuration.sites[0];
        assert_eq!(site.domains[0].value, "api.example.test");
        let timeout = site.routes[0]
            .parameters
            .read_timeout
            .as_ref()
            .ok_or("missing timeout")?;
        assert_eq!(timeout.value, Duration::from_secs(expected));
        assert_eq!(timeout.origin.source.path, fs::canonicalize(&profile)?);
        let canonical_entry = fs::canonicalize(&entry)?;
        assert!(
            timeout
                .origin
                .references
                .iter()
                .any(|reference| reference.source.path == canonical_entry)
        );
    }
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn local_values_override_ordered_profiles_even_when_written_before_use() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-order-{}-{unique}.kdl",
        std::process::id()
    ));
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nprofile \"a\" { read-timeout \"1s\"; connect-timeout \"1s\"; }\nprofile \"b\" { read-timeout \"2s\"; connect-timeout \"2s\"; }\nsite \"example.test\" {\nread-timeout \"3s\"\nuse \"a\" \"b\"\nproxy \"http://127.0.0.1:3000\"\n}\n",
    )?;
    let snapshot = build(&entry, &ConfigurationOnly)?;
    let parameters = &snapshot.configuration.sites[0].routes[0].parameters;
    assert_eq!(
        parameters
            .read_timeout
            .as_ref()
            .ok_or("missing timeout")?
            .value,
        Duration::from_secs(3)
    );
    assert_eq!(
        parameters
            .connect_timeout
            .as_ref()
            .ok_or("missing timeout")?
            .value,
        Duration::from_secs(2)
    );
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn nested_globs_load_in_sorted_order_and_keep_defining_file_paths() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-002-glob-{}-{unique}", std::process::id()));
    fs::create_dir_all(directory.join("nested/sites"))?;
    let entry = directory.join("config.kdl");
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nimport \"nested/include.kdl\"\n",
    )?;
    fs::write(
        directory.join("nested/include.kdl"),
        "import \"sites/[ab]?.kdl\"\n",
    )?;
    for name in ["b2", "a1"] {
        fs::write(
            directory.join(format!("nested/sites/{name}.kdl")),
            format!("site \"{name}.example.test\" {{ root \"assets\"; }}\n"),
        )?;
    }
    let configuration = validate(normalize(parse(load(&entry)?)?)?)?;
    assert_eq!(configuration.sites[0].domains[0].value, "a1.example.test");
    assert_eq!(configuration.sites[1].domains[0].value, "b2.example.test");
    let Action::Static { root, .. } = &configuration.sites[0].routes[0].action else {
        return Err("expected root".into());
    };
    assert_eq!(
        &root.value,
        &fs::canonicalize(directory.join("nested/sites"))?.join("assets")
    );
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn import_cycles_and_file_aliases_are_rejected_with_both_locations() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-002-cycle-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let entry = directory.join("config.kdl");
    fs::write(&entry, "import \"child.kdl\"\n")?;
    fs::write(directory.join("child.kdl"), "import \"config.kdl\"\n")?;
    let error = match load(&entry) {
        Ok(_) => return Err("cycle accepted".into()),
        Err(error) => error,
    };
    assert_eq!(error.code, "IMPORT_CYCLE");
    assert!(error.to_string().contains("child.kdl"));
    assert!(!error.related.is_empty());
    fs::write(directory.join("child.kdl"), "")?;
    fs::hard_link(directory.join("child.kdl"), directory.join("alias.kdl"))?;
    fs::write(&entry, "import \"child.kdl\"\nimport \"alias.kdl\"\n")?;
    let error = match load(&entry) {
        Ok(_) => return Err("duplicate accepted".into()),
        Err(error) => error,
    };
    assert_eq!(error.code, "DUPLICATE");
    assert_eq!(error.origin.position(), (2, 1));
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn entry_symlink_remains_the_reload_entry_after_resource_preparation() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-002-entry-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let entry = directory.join("active.kdl");
    for name in ["old", "new"] {
        fs::write(
            directory.join(format!("{name}.kdl")),
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\nsite \"{name}.example.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"
            ),
        )?;
    }
    std::os::unix::fs::symlink("old.kdl", &entry)?;
    let initial = build(&entry, &ConfigurationOnly)?;
    assert_eq!(initial.configuration.entry, entry);
    assert_eq!(
        initial.configuration.sites[0].domains[0].value,
        "old.example.test"
    );
    fs::remove_file(&entry)?;
    std::os::unix::fs::symlink("new.kdl", &entry)?;
    let rebuilt = build(&initial.configuration.entry, &ConfigurationOnly)?;
    assert_eq!(
        rebuilt.configuration.sites[0].domains[0].value,
        "new.example.test"
    );
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn strict_schema_rejects_unknown_nodes_types_properties_and_duplicates() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-schema-{}-{unique}.kdl",
        std::process::id()
    ));
    let cases = [
        ("typo \"secret-sentinel\"\n", "CONFIG_UNKNOWN"),
        (
            "proxy \"http://127.0.0.1:3000\" unexpected=1\n",
            "CONFIG_TYPE",
        ),
        ("proxy (text)\"http://127.0.0.1:3000\"\n", "CONFIG_TYPE"),
        ("proxy \"http://127.0.0.1:3000\" { }\n", "CONFIG_TYPE"),
        (
            "proxy \"http://127.0.0.1:3000\"\nwebsocket \"true\"\n",
            "CONFIG_TYPE",
        ),
        (
            "proxy \"http://127.0.0.1:3000\"\nread-timeout \"1s\"\nread-timeout \"2s\"\n",
            "DUPLICATE",
        ),
        (
            "proxy \"http://127.0.0.1:3000\"\nread-timeout \"18446744073709551615h\"\n",
            "CONFIG_TYPE",
        ),
        (
            "proxy \"http://127.0.0.1:3000\"\nspa #false\n",
            "CONFIG_TYPE",
        ),
    ];
    for (body, code) in cases {
        fs::write(
            &entry,
            format!("server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{\n{body}}}\n"),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("invalid schema accepted");
        assert_eq!(error.code, code, "{error}");
        assert!(error.origin.position().0 >= 3, "{error}");
        assert!(!error.to_string().contains("secret-sentinel"));
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn all_profile_definitions_are_checked_for_cycles_and_unknown_references() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-002-refs-{}-{unique}.kdl", std::process::id()));
    for profiles in [
        "profile \"a\" { use \"b\"; }\nprofile \"b\" { use \"a\"; }\n",
        "profile \"unused\" { use \"missing\"; }\n",
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\n{profiles}site \"example.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"
            ),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("bad profile accepted");
        assert_eq!(error.code, "PROFILE_REF");
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn conflicting_domains_routes_and_missing_tls_references_are_rejected() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-conflicts-{}-{unique}.kdl",
        std::process::id()
    ));
    let cases = [
        (
            "site \"Example.test\" \"example.test.\" { proxy \"http://127.0.0.1:3000\"; }",
            "DUPLICATE",
        ),
        (
            "site \"example.test\" {\nproxy \"http://127.0.0.1:3000\"\nroute \"/*\" { root \"assets\"; }\n}",
            "ROUTE_CONFLICT",
        ),
        (
            "site \"example.test\" {\nroute \"/%aa\" { proxy \"http://127.0.0.1:3000\"; }\nroute \"/%AA\" { proxy \"http://127.0.0.1:3000\"; }\n}",
            "ROUTE_CONFLICT",
        ),
        (
            "site \"example.test\" { tls \"missing\"; proxy \"http://127.0.0.1:3000\"; }",
            "TLS_INVALID",
        ),
    ];
    for (body, code) in cases {
        fs::write(
            &entry,
            format!("server {{ http \"127.0.0.1:8080\"; }}\n{body}\n"),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("conflict accepted");
        assert_eq!(error.code, code, "{error}");
        if code != "TLS_INVALID" {
            assert!(!error.related.is_empty());
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn upstream_origins_cannot_bypass_literal_target_policy() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-upstream-{}-{unique}.kdl",
        std::process::id()
    ));
    for upstream in [
        "file:///etc/passwd",
        "http://user:password@localhost",
        "http://localhost/path",
        "http://localhost?query",
        "http://localhost#fragment",
        "http://localhost:0",
        "http://169.254.169.254",
        "http://[::ffff:169.254.169.254]",
        "http://2130706433",
        "http://0177.0.0.1",
        "http://0x7f000001",
        "http://0x7f000001.",
        "http://2130706433.",
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ proxy \"{upstream}\"; }}\n"
            ),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("unsafe upstream accepted");
        assert!(
            matches!(error.code, "CONFIG_TYPE" | "UPSTREAM_DENIED"),
            "{error}"
        );
        assert!(!error.to_string().contains("password"));
    }
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; upstream-allow \"10.0.0.0/8\"; }\nsite \"example.test\" { proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly)
            .expect_err("allowlist bypass")
            .code,
        "UPSTREAM_DENIED"
    );
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn http_only_sites_can_share_a_server_with_tls_sites() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-mixed-{}-{unique}.kdl",
        std::process::id()
    ));
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; https \"127.0.0.1:8443\"; }\ntls \"default\" { cert \"cert.pem\"; key \"key.pem\"; }\nsite \"plain.example.test\" { proxy \"http://127.0.0.1:3000\"; }\nsite \"secure.example.test\" { tls \"default\"; proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    let configuration = validate(normalize(parse(load(&entry)?)?)?)?;
    assert!(configuration.sites[0].tls.is_none());
    assert!(configuration.sites[1].tls.is_some());
    assert_eq!(
        build(&entry, &ConfigurationOnly)
            .expect_err("TLS preparation unavailable")
            .code,
        "CONFIG_UNSUPPORTED"
    );
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn missing_resource_preparers_cannot_report_success() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-resources-{}-{unique}.kdl",
        std::process::id()
    ));
    for body in [
        "site \"example.test\" { root \"missing-directory\"; }",
        "tls \"cert\" { cert \"missing.pem\"; key \"missing.key\"; }\nsite \"example.test\" { tls \"cert\"; proxy \"http://127.0.0.1:3000\"; }",
    ] {
        fs::write(
            &entry,
            format!("server {{ http \"127.0.0.1:8080\"; }}\n{body}\n"),
        )?;
        let result = Command::new(env!("CARGO_BIN_EXE_rill"))
            .arg("check")
            .arg("--config")
            .arg(&entry)
            .output()?;
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("CONFIG_UNSUPPORTED"));
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn check_does_not_bind_listeners_or_contact_the_control_socket() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "rill-002-side-effects-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&directory)?;
    let entry = directory.join("config.kdl");
    let socket = directory.join("control.sock");
    let control = UnixListener::bind(&socket)?;
    control.set_nonblocking(true)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    fs::write(
        &entry,
        format!(
            "server {{ http \"{}\"; control-socket \"{}\"; }}\nsite \"example.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n",
            listener.local_addr()?,
            socket.display()
        ),
    )?;
    let before = fs::read(&entry)?;
    let result = Command::new(env!("CARGO_BIN_EXE_rill"))
        .arg("check")
        .arg("--config")
        .arg(&entry)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        control
            .accept()
            .expect_err("check contacted control socket")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read(&entry)?, before);
    drop(control);
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn excessive_kdl_nesting_is_rejected_before_the_recursive_parser() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-002-depth-{}-{unique}.kdl",
        std::process::id()
    ));
    let nested = "n {\n".repeat(1000) + &"}\n".repeat(1000);
    for source in [
        nested.clone(),
        "/*".repeat(1000) + &"*/".repeat(1000),
        format!("// comment\u{2028}{nested}"),
        "/- ".repeat(1000) + "n\n",
        "n ".to_string() + &"/- ".repeat(1000) + "1\n",
        "/- \u{2028}".repeat(1000) + "n\n",
    ] {
        fs::write(&entry, source)?;
        let error = build(&entry, &ConfigurationOnly).expect_err("nesting limit bypass");
        assert_eq!(error.code, "CONFIG_LIMIT");
    }
    fs::write(&entry, "n {\n".repeat(32) + &"}\n".repeat(32))?;
    assert_eq!(
        build(&entry, &ConfigurationOnly)
            .expect_err("unknown node accepted")
            .code,
        "CONFIG_UNKNOWN"
    );
    let quoted = "{".repeat(1000);
    fs::write(
        &entry,
        format!(
            "// {{ {{ {{\u{2028}server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ proxy \"{quoted}\"; }}\n"
        ),
    )?;
    let error = build(&entry, &ConfigurationOnly).expect_err("invalid upstream accepted");
    assert_eq!(error.code, "CONFIG_TYPE");
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn check_reports_syntax_locations_and_cli_argument_exit_codes() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-002-cli-{}-{unique}.kdl", std::process::id()));
    fs::write(
        &entry,
        "server {\nhttp \"127.0.0.1:8080\"\nredirect-https true\n}\n",
    )?;
    let result = Command::new(env!("CARGO_BIN_EXE_rill"))
        .arg("check")
        .arg("--config")
        .arg(&entry)
        .output()?;
    assert_eq!(result.status.code(), Some(1));
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("CONFIG_SYNTAX") && error.contains(":3:"),
        "{error}"
    );
    for args in [
        vec![],
        vec!["check", "--config"],
        vec!["check", "--unknown"],
        vec!["run", "--unknown"],
        vec!["reload"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(args)
            .output()?;
        assert_eq!(result.status.code(), Some(2));
    }
    assert!(build(Path::new("examples/config.kdl"), &ConfigurationOnly).is_ok());
    fs::remove_file(entry)?;
    Ok(())
}
