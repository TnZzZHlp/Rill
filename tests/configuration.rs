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
            .expect_err("TLS certificate missing")
            .code,
        "RESOURCE_IO"
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
    for (body, code) in [
        (
            "site \"example.test\" { root \"missing-directory\"; }",
            "RESOURCE_IO",
        ),
        (
            "tls \"cert\" { cert \"missing.pem\"; key \"missing.key\"; }\nsite \"example.test\" { tls \"cert\"; proxy \"http://127.0.0.1:3000\"; }",
            "RESOURCE_IO",
        ),
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
        assert!(String::from_utf8_lossy(&result.stderr).contains(code));
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
        vec!["reload", "--unknown"],
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

#[test]
fn snippet_nested_forward_reuse_keeps_paths_and_profile_precedence() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("rill-snippet-{}-{unique}", std::process::id()));
    fs::create_dir_all(dir.join("defs/assets"))?;
    let entry = dir.join("config.kdl");
    fs::write(
        dir.join("defs/snippets.kdl"),
        "snippet \"shared\" { use-snippet \"inner\"; }\nsnippet \"inner\" { route \"/assets/*\" { root \"assets\"; use \"shared\"; read-timeout \"7s\"; } }\n",
    )?;
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nimport \"defs/snippets.kdl\"\nprofile \"shared\" { read-timeout \"4s\"; connect-timeout \"2s\"; }\nsite \"a.test\" { read-timeout \"1s\"; use-snippet \"shared\"; }\nsite \"b.test\" { read-timeout \"2s\"; use-snippet \"shared\"; }\n",
    )?;
    let config = validate(normalize(parse(load(&entry)?)?)?)?;
    for site in &config.sites {
        let route = &site.routes[0];
        assert_eq!(
            route
                .parameters
                .read_timeout
                .as_ref()
                .ok_or("timeout")?
                .value,
            Duration::from_secs(7)
        );
        assert_eq!(
            route
                .parameters
                .connect_timeout
                .as_ref()
                .ok_or("timeout")?
                .value,
            Duration::from_secs(2)
        );
        let Action::Static { root, .. } = &route.action else {
            return Err("expected static".into());
        };
        assert_eq!(
            root.value,
            fs::canonicalize(dir.join("defs"))?.join("assets")
        );
        assert!(root.origin.references.len() >= 2);
    }
    let result = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&entry)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn snippet_invalid_definitions_and_conflicts_are_rejected() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-snippet-invalid-{}-{unique}.kdl",
        std::process::id()
    ));
    for (defs, site, code) in [
        (
            "snippet \"a\" { use-snippet \"missing\"; }",
            "proxy \"http://127.0.0.1:3000\";",
            "SNIPPET_REF",
        ),
        (
            "snippet \"a\" { use-snippet \"b\"; }\nsnippet \"b\" { use-snippet \"a\"; }",
            "proxy \"http://127.0.0.1:3000\";",
            "SNIPPET_REF",
        ),
        (
            "snippet \"a\" { route \"/*\" { proxy \"http://127.0.0.1:3000\"; } }\nsnippet \"a\" { route \"/*\" { proxy \"http://127.0.0.1:3000\"; } }",
            "proxy \"http://127.0.0.1:3000\";",
            "DUPLICATE",
        ),
        (
            "snippet \"a\" {}",
            "proxy \"http://127.0.0.1:3000\";",
            "CONFIG_TYPE",
        ),
        (
            "snippet \"a\" { use-snippet \"b\" \"c\"; }",
            "proxy \"http://127.0.0.1:3000\";",
            "CONFIG_TYPE",
        ),
        (
            "snippet \"a\" { root \"x\"; }",
            "proxy \"http://127.0.0.1:3000\";",
            "CONFIG_UNKNOWN",
        ),
        (
            "",
            "route \"/*\" { use-snippet \"x\"; proxy \"http://127.0.0.1:3000\"; }",
            "CONFIG_UNKNOWN",
        ),
        (
            "profile \"a\" { use-snippet \"x\"; }",
            "proxy \"http://127.0.0.1:3000\";",
            "CONFIG_UNKNOWN",
        ),
        (
            "snippet \"a\" { route \"/*\" { proxy \"http://127.0.0.1:3000\"; } }",
            "use-snippet \"a\"; proxy \"http://127.0.0.1:3000\";",
            "ROUTE_CONFLICT",
        ),
        (
            "snippet \"a\" { route \"/*\" { proxy \"http://127.0.0.1:3000\"; } }",
            "use-snippet \"a\"; use-snippet \"a\";",
            "ROUTE_CONFLICT",
        ),
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\n{defs}\nsite \"a.test\" {{ {site} }}\n"
            ),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("invalid snippet accepted");
        assert_eq!(error.code, code, "{error}");
        if matches!(code, "DUPLICATE" | "ROUTE_CONFLICT") {
            assert!(!error.related.is_empty());
        }
        if code == "ROUTE_CONFLICT" {
            assert!(error.to_string().matches("related:").count() >= 2);
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn snippet_expansion_limits_are_enforced() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-snippet-limit-{}-{unique}.kdl",
        std::process::id()
    ));
    let prefix = "server { http \"127.0.0.1:8080\"; }\n";
    let route = "route \"/*\" { proxy \"http://127.0.0.1:3000\"; }";
    let mut depth = String::new();
    for i in 0..65 {
        depth.push_str(&format!(
            "snippet \"s{i}\" {{ {} }}\n",
            if i == 64 {
                route.to_string()
            } else {
                format!("use-snippet \"s{}\";", i + 1)
            }
        ));
    }
    for defs in [
        depth,
        format!(
            "snippet \"s\" {{ {route} }}\n{}",
            (0..4097)
                .map(|i| format!("site \"s{i}.test\" {{ use-snippet \"s\"; }}\n"))
                .collect::<String>()
        ),
    ] {
        fs::write(
            &entry,
            format!("{prefix}{defs}\nsite \"end.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"),
        )?;
        assert_eq!(
            build(&entry, &ConfigurationOnly)
                .expect_err("limit bypass")
                .code,
            "CONFIG_LIMIT"
        );
    }
    for (count, succeeds) in [(8190, true), (8191, false)] {
        let defs = format!(
            "snippet \"a-wide\" {{ {} }}\nsnippet \"z-leaf\" {{ {route} }}\nsite \"a.test\" {{ proxy \"http://127.0.0.1:3000\"; }}",
            "use-snippet \"z-leaf\";\n".repeat(count)
        );
        fs::write(&entry, format!("{prefix}{defs}"))?;
        let result = build(&entry, &ConfigurationOnly);
        if succeeds {
            assert!(result.is_ok(), "{result:?}");
        } else {
            let error = result.expect_err("step limit bypass");
            assert_eq!(error.code, "CONFIG_LIMIT");
            assert_eq!(&*error.reason, "snippet step limit exceeded");
        }
    }
    let long_pattern = format!("/{}", "a".repeat(6000));
    let large = format!(
        "snippet \"s\" {{ route \"{long_pattern}\" {{ proxy \"http://127.0.0.1:3000\"; }} }}\nsite \"a.test\" {{ {} }}",
        "use-snippet \"s\"; ".repeat(2000)
    );
    fs::write(&entry, format!("{prefix}{large}"))?;
    assert_eq!(
        build(&entry, &ConfigurationOnly)
            .expect_err("byte limit bypass")
            .code,
        "CONFIG_LIMIT"
    );
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn snippet_reference_errors_keep_cross_file_call_chains() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rill-snippet-chain-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&dir)?;
    let entry = dir.join("entry.kdl");
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nimport \"a.kdl\"\nimport \"b.kdl\"\nsite \"x.test\" { proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    fs::write(dir.join("a.kdl"), "snippet \"a\" { use-snippet \"b\"; }\n")?;
    for (body, code) in [
        (
            "snippet \"b\" { use-snippet \"missing\"; }\n",
            "SNIPPET_REF",
        ),
        ("snippet \"b\" { use-snippet \"a\"; }\n", "SNIPPET_REF"),
    ] {
        fs::write(dir.join("b.kdl"), body)?;
        let error = build(&entry, &ConfigurationOnly).expect_err("reference accepted");
        assert_eq!(error.code, code);
        assert_eq!(
            error.origin.source.path,
            fs::canonicalize(dir.join("b.kdl"))?
        );
        assert_eq!(error.origin.position(), (1, 15));
        assert!(error.origin.references.iter().any(|r| r.source.path
            == fs::canonicalize(dir.join("a.kdl")).unwrap()
            && r.position() == (1, 15)));
    }
    let mut definitions = String::new();
    for i in 1..65 {
        definitions.push_str(&format!(
            "snippet \"b{i:02}\" {{ use-snippet \"{}\"; }}\n",
            if i == 64 {
                "last".to_string()
            } else {
                format!("b{:02}", i + 1)
            }
        ));
    }
    definitions
        .push_str("snippet \"last\" { route \"/*\" { proxy \"http://127.0.0.1:3000\"; } }\n");
    fs::write(
        dir.join("a.kdl"),
        "snippet \"a\" { use-snippet \"b01\"; }\n",
    )?;
    fs::write(dir.join("b.kdl"), definitions)?;
    let error = build(&entry, &ConfigurationOnly).expect_err("depth accepted");
    assert_eq!(error.code, "CONFIG_LIMIT");
    assert!(error.origin.references.iter().any(|r| r.source.path
        == fs::canonicalize(dir.join("a.kdl")).unwrap()
        && r.position() == (1, 15)));
    assert_eq!(
        error
            .origin
            .references
            .iter()
            .filter(|r| r.source.path == fs::canonicalize(dir.join("b.kdl")).unwrap())
            .count(),
        62
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn snippet_conflict_deduplicates_deep_import_and_reference_chains() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rill-snippet-dedup-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&dir)?;
    for i in 0..16 {
        fs::write(
            dir.join(format!("{i}.kdl")),
            format!("import \"{}.kdl\"\n", i + 1),
        )?;
    }
    let mut defs = String::new();
    for i in 0..16 {
        defs.push_str(&format!(
            "snippet \"s{i:02}\" {{ {} }}\n",
            if i == 15 {
                "route \"/*\" { proxy \"http://127.0.0.1:3000\"; }".to_string()
            } else {
                format!("use-snippet \"s{:02}\";", i + 1)
            }
        ));
    }
    defs.push_str("server { http \"127.0.0.1:8080\"; }\nsite \"a.test\" { use-snippet \"s00\"; use-snippet \"s00\"; }\n");
    fs::write(dir.join("16.kdl"), defs)?;
    let error = build(&dir.join("0.kdl"), &ConfigurationOnly).expect_err("conflict accepted");
    assert_eq!(error.code, "ROUTE_CONFLICT");
    let text = error.to_string();
    let lines: Vec<_> = text.lines().skip(1).collect();
    let unique_lines: std::collections::BTreeSet<_> = lines.iter().collect();
    assert_eq!(lines.len(), unique_lines.len());
    assert!(lines.len() >= 33 && lines.len() <= 36, "{text}");
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn snippet_payload_budget_deduplicates_imports_and_reports_nested_chain() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rill-snippet-payload-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&dir)?;
    for i in 0..20 {
        fs::write(
            dir.join(format!("{i}.kdl")),
            format!("import \"{}.kdl\"\n", i + 1),
        )?;
    }
    let definition = dir.join("20.kdl");
    for (pattern_len, count, succeeds) in [(1, 300, true), (16000, 500, false)] {
        let mut source = String::new();
        for i in 0..20 {
            let item = if i == 19 {
                format!(
                    "route \"/{}\" {{ proxy \"http://127.0.0.1:3000\"; }}",
                    "a".repeat(pattern_len)
                )
            } else {
                format!("use-snippet \"s{:02}\";", i + 1)
            };
            source.push_str(&format!("snippet \"s{i:02}\" {{ {item} }}\n"));
        }
        source.push_str("server { http \"127.0.0.1:8080\"; }\nsite \"a.test\" {\n");
        source.push_str(&"use-snippet \"s00\";\n".repeat(count));
        source.push_str("}\n");
        fs::write(&definition, source)?;
        let result = normalize(parse(load(&dir.join("0.kdl"))?)?);
        if succeeds {
            assert_eq!(result?.sites[0].routes.len(), count);
        } else {
            let error = result.expect_err("payload limit bypass");
            assert_eq!(error.code, "CONFIG_LIMIT");
            assert_eq!(error.origin.source.path, fs::canonicalize(&definition)?);
            assert_eq!(error.origin.position(), (20, 17));
            assert!(
                error
                    .origin
                    .references
                    .iter()
                    .any(|r| r.source.path == error.origin.source.path && r.position() == (1, 17))
            );
            assert!(
                error
                    .origin
                    .references
                    .iter()
                    .any(|r| r.source.path == error.origin.source.path
                        && r.position().0 >= 23
                        && r.position().1 == 1)
            );
        }
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn watch_configuration_is_boolean_and_singleton() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-watch-config-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let entry = directory.join("config.kdl");
    for (field, expected) in [
        ("", Some(false)),
        ("watch #true;", Some(true)),
        ("watch #false;", Some(false)),
        ("watch true;", None),
        ("watch #true; watch #false;", None),
        ("watch #true extra=1;", None),
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; {field} }}\nsite \"example.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"
            ),
        )?;
        let parsed = load(&entry).and_then(parse);
        match expected {
            Some(value) => assert_eq!(parsed?.server.ok_or("missing server")?.watch, value),
            None => assert!(parsed.is_err(), "accepted {field}"),
        }
    }
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn tracked_build_retains_glob_and_symlink_dependencies_on_failure() -> TestResult {
    use rill::config::prepare::build_tracked;
    use std::os::unix::fs::symlink;

    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rill-watch-graph-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let actual = directory.join("actual.kdl");
    let entry = directory.join("entry.kdl");
    let imports = directory.join("imports");
    fs::create_dir(&imports)?;
    fs::write(
        &actual,
        "server { http \"127.0.0.1:8080\"; }\nimport \"imports/*/*.kdl\"\nsite \"example.test\" { proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    symlink(&actual, &entry)?;
    let failure = build_tracked(&entry, &ConfigurationOnly).expect_err("empty glob must fail");
    assert!(failure.dependencies.paths.contains_key(&entry));
    assert!(failure.dependencies.paths.contains_key(&actual));
    assert!(
        failure
            .dependencies
            .glob_directories
            .iter()
            .any(|item| item.directory == imports && item.pattern == "*")
    );
    fs::create_dir(imports.join("new"))?;
    assert!(!failure.dependencies.unchanged());
    fs::write(
        imports.join("new/profile.kdl"),
        "profile \"base\" { read-timeout \"1s\"; }\n",
    )?;
    let tracked = build_tracked(&entry, &ConfigurationOnly).map_err(|failure| failure.error)?;
    assert!(
        tracked
            .dependencies
            .glob_directories
            .iter()
            .any(|item| item.directory == imports.join("new") && item.pattern == "*.kdl")
    );
    assert!(tracked.dependencies.unchanged());
    fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn static_enhancement_fields_and_snippet_origins() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("rill-010-config-{}-{unique}", std::process::id()));
    fs::create_dir_all(dir.join("defs"))?;
    let entry = dir.join("config.kdl");
    fs::write(
        dir.join("defs/shared.kdl"),
        "snippet \"outer\" { use-snippet \"inner\"; }\nsnippet \"inner\" { route \"/assets/*\" { root \"www\"; precompressed #true; browse #true; error-page 404 \"errors/404.html\"; error-page 503 \"errors/503.html\"; } }\n",
    )?;
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; static-cache-bytes 16777216; }\nimport \"defs/shared.kdl\"\nsite \"a.test\" { root \"www\"; precompressed #true; error-page 403 \"forbidden.html\"; route \"/other\" { root \"other\"; }; use-snippet \"outer\"; }\nsite \"b.test\" { use-snippet \"outer\"; }\n",
    )?;
    let parsed = parse(load(&entry)?)?;
    let normalized = normalize(parsed)?;
    for site in &normalized.sites {
        let route = site
            .routes
            .iter()
            .find(|route| route.pattern.value == "/assets/*")
            .ok_or("snippet route")?;
        assert!(
            route
                .precompressed
                .as_ref()
                .ok_or("precompressed")?
                .origin
                .references
                .len()
                >= 2
        );
        assert!(
            route
                .browse
                .as_ref()
                .ok_or("browse")?
                .origin
                .references
                .len()
                >= 2
        );
        assert!(route.error_pages[&404].origin.references.len() >= 2);
        assert_eq!(
            route.error_pages[&404].value,
            fs::canonicalize(dir.join("defs"))?.join("errors/404.html")
        );
    }
    let config = validate(normalized)?;
    assert_eq!(config.server.static_cache_bytes, 16_777_216);
    let Action::Static {
        precompressed,
        browse,
        error_pages,
        ..
    } = &config.sites[0]
        .routes
        .iter()
        .find(|r| r.pattern.value == "/other")
        .ok_or("route")?
        .action
    else {
        return Err("static".into());
    };
    assert!(!precompressed && !browse && error_pages.is_empty());
    let Action::Static {
        precompressed,
        error_pages,
        ..
    } = &config.sites[0]
        .routes
        .iter()
        .find(|r| r.pattern.value == "/*")
        .ok_or("shorthand")?
        .action
    else {
        return Err("static".into());
    };
    assert!(*precompressed && error_pages.contains_key(&403));
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn static_enhancement_strict_config_and_budget() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-010-invalid-{}-{unique}.kdl",
        std::process::id()
    ));
    for (server, route, expected) in [
        ("static-cache-bytes -1;", "root \"www\";", "CONFIG_TYPE"),
        (
            "static-cache-bytes 268435457;",
            "root \"www\";",
            "CONFIG_TYPE",
        ),
        ("static-cache-bytes \"1\";", "root \"www\";", "CONFIG_TYPE"),
        ("", "root \"www\"; precompressed 1;", "CONFIG_TYPE"),
        ("", "root \"www\"; browse #true { };", "CONFIG_TYPE"),
        ("", "root \"www\"; error-page 200 \"x\";", "CONFIG_TYPE"),
        ("", "root \"www\"; error-page 404 1;", "CONFIG_TYPE"),
        (
            "",
            "root \"www\"; error-page 404 \"x\" extra=1;",
            "CONFIG_TYPE",
        ),
        (
            "",
            "root \"www\"; error-page 404 \"x\"; error-page 404 \"y\";",
            "DUPLICATE",
        ),
        (
            "",
            "proxy \"http://127.0.0.1:3000\"; browse #true;",
            "CONFIG_TYPE",
        ),
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; {server} }}\nsite \"a.test\" {{ {route} }}\n"
            ),
        )?;
        let result = load(&entry)
            .and_then(parse)
            .and_then(normalize)
            .and_then(validate);
        let error = result.expect_err("invalid config accepted");
        assert_eq!(error.code, expected, "{error}");
        if expected == "DUPLICATE" {
            assert!(!error.related.is_empty());
        }
    }
    let path = "a".repeat(16000);
    fs::write(
        &entry,
        format!(
            "snippet \"s\" {{ route \"/*\" {{ root \"www\"; precompressed #true; browse #true; error-page 404 \"{path}\"; }} }}\nserver {{ http \"127.0.0.1:8080\"; }}\nsite \"a.test\" {{ {} }}",
            "use-snippet \"s\"; ".repeat(600)
        ),
    )?;
    let error =
        normalize(parse(load(&entry)?)?).expect_err("new fields bypass snippet payload budget");
    assert_eq!(error.code, "CONFIG_LIMIT");
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn observability_defaults_and_explicit_fields() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-observability-{}-{unique}.kdl",
        std::process::id()
    ));
    let site = "site \"example.test\" { proxy \"http://127.0.0.1:3000\"; }\n";
    fs::write(
        &entry,
        format!("server {{ http \"127.0.0.1:8080\"; }}\n{site}"),
    )?;
    let config = build(&entry, &ConfigurationOnly)?.configuration;
    let observability = &config.server.observability;
    assert!(!observability.policy.access_log);
    assert_eq!(observability.policy.access_sample_per_mille, 1000);

    fs::write(
        &entry,
        format!(
            "server {{ http \"127.0.0.1:8080\"; observability {{ access-log #true; access-sample-per-mille 0; }}; }}\n{site}"
        ),
    )?;
    let config = build(&entry, &ConfigurationOnly)?.configuration;
    let observability = &config.server.observability;
    assert!(observability.policy.access_log);
    assert_eq!(observability.policy.access_sample_per_mille, 0);
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn observability_rejects_invalid_and_removed_fields() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-observability-invalid-{}-{unique}.kdl",
        std::process::id()
    ));
    for fields in [
        "access-log \"true\";",
        "tracing #true;",
        "tracing #false;",
        "metrics \"127.0.0.1:19090\";",
        "otlp-endpoint \"http://127.0.0.1:4318/v1/traces\";",
        "access-sample-per-mille -1;",
        "access-sample-per-mille 1001;",
        "access-sample-per-mille 1.5;",
        "trace-sample-per-mille 1001;",
        "trace-sample-per-mille 1.5;",
        "metrics \"0.0.0.0:19090\";",
        "metrics \"localhost:19090\";",
        "metrics \"127.0.0.1:0\";",
        "otlp-endpoint \"http://192.0.2.1:4318/v1/traces\";",
        "otlp-endpoint \"http://user:secret@127.0.0.1:4318/v1/traces\";",
        "otlp-endpoint \"http://127.0.0.1:4318/v1/traces?secret\";",
        "otlp-endpoint \"http://127.0.0.1:4318/v1/traces#fragment\";",
        "otlp-endpoint \"http://localhost:4318/v1/traces\";",
        "otlp-endpoint \"https://127.0.0.1:4318/v1/traces\";",
        "otlp-endpoint \"http://127.0.0.1:0/v1/traces\";",
        "otlp-endpoint \"http://127.0.0.1:4318//v1/traces\";",
        "access-log #true; access-log #false;",
        "unknown #true;",
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; observability {{ {fields} }}; }}\nsite \"example.test\" {{ proxy \"http://127.0.0.1:3000\"; }}\n"
            ),
        )?;
        let error = build(&entry, &ConfigurationOnly).expect_err("invalid observability accepted");
        assert!(!error.to_string().contains("secret"), "{fields}: {error}");
        assert!(
            matches!(error.code, "CONFIG_TYPE" | "DUPLICATE" | "CONFIG_UNKNOWN"),
            "{fields}: {error}"
        );
    }
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; observability {}; observability {}; }\nsite \"example.test\" { proxy \"http://127.0.0.1:3000\"; }\n",
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly)
            .expect_err("duplicate block accepted")
            .code,
        "DUPLICATE"
    );
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn chain_configuration_compiles_and_rejects_invalid_candidates() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-chain-{}-{unique}.kdl", std::process::id()));
    let prefix = "server { http \"127.0.0.1:8080\"; }\nsite \"example.test\" { ";
    let cases = [
        (
            "chain { rule \"x\" { match { all { method \"GET\"; any { path-glob \"/a/*\"; path-regex \"^/b\"; }; not { client-cidr \"192.0.2.0/24\"; }; }; }; branch { when { match { header-prefix \"accept\" \"text/\"; }; respond 200 text=\"ok\"; }; otherwise { respond 404; }; }; }; }",
            None,
        ),
        (
            "chain { rule { respond 200; }; }; use-snippet \"missing\";",
            Some("POLICY_MODE_CONFLICT"),
        ),
        (
            "chain { rule { match { error-status 404; }; respond 200; }; }",
            Some("POLICY_INVALID_MATCHER"),
        ),
        (
            "chain { rule { respond \"invalid\" 200; }; }",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "chain { rule { respond 200 201; }; }",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "chain { rule { respond 204 text=\"body\"; }; }",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "chain { rule { match { path-regex \"a{1001}\"; }; respond 200; }; }",
            Some("POLICY_INVALID_REGEX"),
        ),
        (
            "chain { rule { header \"response\" { }; }; }",
            Some("POLICY_INVALID_CHAIN"),
        ),
        (
            "chain { rule { respond 200; respond 201; }; }",
            Some("POLICY_INVALID_CHAIN"),
        ),
        (
            "chain { rule { continue; respond 200; }; }",
            Some("POLICY_INVALID_CHAIN"),
        ),
        (
            "chain { rule { branch { otherwise { reenter; respond 200; }; }; }; }",
            Some("POLICY_INVALID_CHAIN"),
        ),
        (
            "chain { rule { branch { otherwise { }; }; }; }",
            Some("POLICY_INVALID_CHAIN"),
        ),
    ];
    for (site, expected) in cases {
        fs::write(&entry, format!("{prefix}{site} }}\n"))?;
        match (build(&entry, &ConfigurationOnly), expected) {
            (Ok(snapshot), None) => assert_eq!(snapshot.resources.policy.regexes.len(), 2),
            (Err(error), Some(code)) => assert_eq!(error.code, code),
            (result, expected) => panic!("unexpected result {result:?}; expected {expected:?}"),
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn policy_regex_captures_do_not_leak_from_failed_matches() -> TestResult {
    use rill::http_policy::execute::{MatchContext, Outcome, execute};
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-captures-{}-{unique}.kdl", std::process::id()));
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nsite \"example.test\" { chain { rule { match { all { path-regex \"^/(.*)\"; method \"POST\"; }; }; respond 200; }; rule { match { not { path-regex \"^/other/(.*)\"; }; }; respond 401; }; }; }",
    )?;
    let snapshot = build(&entry, &ConfigurationOnly)?;
    let chain = snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("missing chain")?;
    let mut headers = http::HeaderMap::new();
    let mut ctx = MatchContext {
        method: &http::Method::GET,
        path: "/secret".into(),
        headers: &mut headers,
        state: None,
        client_ip: "127.0.0.1".parse()?,
        steps_left: 1024,
        reentries: 0,
        error_status: None,
        captures: [None; 9],
    };
    assert!(matches!(
        execute(chain, &snapshot.resources.policy, &mut ctx),
        Outcome::Respond { status: 401, .. }
    ));
    assert_eq!(ctx.captures, [None; 9]);
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn policy_steps_short_circuit_and_branch_are_bounded() -> TestResult {
    use rill::http_policy::execute::{MatchContext, Outcome, execute};
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-steps-{}-{unique}.kdl", std::process::id()));
    let prefix = "server { http \"127.0.0.1:8080\"; }\nsite \"example.test\" { chain { ";
    let rules = "rule { continue; }; ".repeat(511);
    fs::write(
        &entry,
        format!("{prefix}{rules}rule {{ respond 200; }}; }}; }}"),
    )?;
    let snapshot = build(&entry, &ConfigurationOnly)?;
    let chain = snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("missing chain")?;
    let mut headers = http::HeaderMap::new();
    let mut ctx = MatchContext {
        method: &http::Method::GET,
        path: "/value".into(),
        headers: &mut headers,
        state: None,
        client_ip: "127.0.0.1".parse()?,
        steps_left: 1024,
        reentries: 0,
        error_status: None,
        captures: [None; 9],
    };
    assert!(matches!(
        execute(chain, &snapshot.resources.policy, &mut ctx),
        Outcome::Respond { status: 200, .. }
    ));
    assert_eq!(ctx.steps_left, 0);
    ctx.steps_left = 1023;
    assert!(matches!(
        execute(chain, &snapshot.resources.policy, &mut ctx),
        Outcome::BudgetExceeded
    ));
    fs::write(
        &entry,
        format!(
            "{prefix}rule {{ match {{ any {{ method \"GET\"; path-regex \"^/(.*)\"; }}; }}; branch {{ when {{ match {{ method \"GET\"; }}; respond 201; }}; when {{ match {{ method \"GET\"; }}; respond 202; }}; otherwise {{ respond 203; }}; }}; }}; }}; }}"
        ),
    )?;
    let snapshot = build(&entry, &ConfigurationOnly)?;
    let chain = snapshot.configuration.sites[0]
        .chain
        .as_ref()
        .ok_or("missing chain")?;
    ctx.steps_left = 8;
    assert!(matches!(
        execute(chain, &snapshot.resources.policy, &mut ctx),
        Outcome::Respond { status: 201, .. }
    ));
    assert_eq!(ctx.steps_left, 1);
    assert_eq!(ctx.captures, [None; 9]);
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn header_policy_schema_and_templates() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-headers-{}-{unique}.kdl", std::process::id()));
    let cases = [
        (
            r#"header "request" real-client-ip="X-Real-IP" { set "X-Test" "{client-ip}/{site}/{path}/{capture.1}/{upstream-authority}"; add "X-Test" "value"; replace "X-Test" "" "x"; delete "X-Old"; }; header "response" when-content-type="text/html" when-status="200..399" { vary "Accept-Encoding"; }; continue;"#,
            None,
        ),
        (
            r#"header "request" when-status="200..399" {}; continue;"#,
            Some("CONFIG_UNKNOWN"),
        ),
        (
            r#"header "response" when-status="400..200" {}; continue;"#,
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            r#"header "response" when-status="199..200" {}; continue;"#,
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            r#"header "response" unexpected="x" {}; continue;"#,
            Some("CONFIG_UNKNOWN"),
        ),
        (
            r#"header "request" real-client-ip="X-Other" {}; continue;"#,
            Some("POLICY_PROTECTED_HEADER"),
        ),
        (
            r#"header "response" { delete "content-length"; }; continue;"#,
            Some("POLICY_PROTECTED_HEADER"),
        ),
        (
            r#"header "request" { set "X-Forwarded-For" "x"; }; continue;"#,
            Some("POLICY_PROTECTED_HEADER"),
        ),
        (
            r#"header "request" { set "X-Test" "{secret}"; }; continue;"#,
            Some("POLICY_INVALID_TEMPLATE"),
        ),
        (
            r#"header "request" { set "X-Test" "oops{path"; }; continue;"#,
            Some("POLICY_INVALID_TEMPLATE"),
        ),
        (
            r#"header "request" { set "X-Test" "bad\r\nvalue"; }; continue;"#,
            Some("POLICY_INVALID_TEMPLATE"),
        ),
        (
            r#"header "request" { vary "Accept"; }; continue;"#,
            Some("POLICY_INVALID_RESPONSE"),
        ),
    ];
    for (body, expected) in cases {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}
site \"example.test\" {{ chain {{ rule {{ {body} }}; }}; }}"
            ),
        )?;
        match (build(&entry, &ConfigurationOnly), expected) {
            (Ok(_), None) => (),
            (Err(error), Some(code)) => assert_eq!(error.code, code),
            (result, expected) => panic!("unexpected result {result:?}; expected {expected:?}"),
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn chain_path_actions_validate_templates_and_prepare_roots() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry =
        std::env::temp_dir().join(format!("rill-paths-{}-{unique}.kdl", std::process::id()));
    let cases = [
        (
            "rewrite \"https://example.org/\"; continue;",
            Some("POLICY_INVALID_PATH"),
        ),
        ("rewrite \"/a?x=1\"; continue;", Some("POLICY_INVALID_PATH")),
        (
            "rewrite \"/{client-ip}\"; continue;",
            Some("POLICY_INVALID_TEMPLATE"),
        ),
        (
            "strip-prefix \"/a/../b\"; continue;",
            Some("POLICY_INVALID_PATH"),
        ),
        (
            "try-files root=\".\"; continue;",
            Some("POLICY_INVALID_PATH"),
        ),
        (
            "try-files root=\".\" \"/../x\"; continue;",
            Some("POLICY_INVALID_PATH"),
        ),
        (
            "try-files root=\".\" \"/{site}\"; continue;",
            Some("POLICY_INVALID_TEMPLATE"),
        ),
        (
            "try-files root=\"rill-nonexistent-root\" \"/x\"; continue;",
            Some("RESOURCE_IO"),
        ),
        (
            "rewrite \"/safe/{capture.1}\"; strip-prefix \"/safe\"; continue;",
            None,
        ),
        ("rewrite \"/%{capture.1}\"; continue;", None),
        (
            "try-files root=\".\" \"/missing\" \"/{path}\"; continue;",
            None,
        ),
    ];
    for (body, expected) in cases {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ chain {{ rule {{ {body} }}; }}; }}"
            ),
        )?;
        match (build(&entry, &ConfigurationOnly), expected) {
            (Ok(_), None) => (),
            (Err(error), Some(code)) => assert_eq!(error.code, code, "{body}"),
            (result, expected) => panic!("unexpected result {result:?}; expected {expected:?}"),
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn error_chain_schema_rejects_success_and_invalid_matchers() -> TestResult {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let entry = std::env::temp_dir().join(format!(
        "rill-error-chain-{}-{unique}.kdl",
        std::process::id()
    ));
    let cases = [
        (
            "rule { respond 404; }; handle-errors { rule { match { error-status 404; }; respond 404; }; };",
            None,
        ),
        (
            "rule { respond 404; }; handle-errors { rule { match { error-status 502; }; respond 500; }; };",
            None,
        ),
        (
            "rule { respond 404; }; handle-errors { rule { match { error-status 399; }; respond 404; }; };",
            Some("POLICY_INVALID_MATCHER"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { match { error-status 600; }; respond 404; }; };",
            Some("POLICY_INVALID_MATCHER"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { match { error-status 404 405; }; respond 404; }; };",
            Some("POLICY_INVALID_MATCHER"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { respond 200; }; };",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { respond 304; }; };",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { branch { otherwise { respond 200; }; }; }; };",
            Some("POLICY_INVALID_RESPONSE"),
        ),
        (
            "rule { respond 404; }; handle-errors { rule { respond 404; }; }; rule { respond 404; };",
            Some("POLICY_INVALID_CHAIN"),
        ),
        (
            "rule { respond 404; }; handle-errors { handle-errors { rule { respond 404; }; }; };",
            Some("POLICY_INVALID_CHAIN"),
        ),
    ];
    for (body, expected) in cases {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ chain {{ {body} }}; }}"
            ),
        )?;
        match (build(&entry, &ConfigurationOnly), expected) {
            (Ok(_), None) => (),
            (Err(error), Some(code)) => assert_eq!(error.code, code, "{body}"),
            (result, expected) => panic!("unexpected result {result:?}; expected {expected:?}"),
        }
    }
    fs::remove_file(entry)?;
    Ok(())
}

#[test]
fn authentication_preparation_rejects_bad_inputs_and_tracks_token() -> TestResult {
    use rill::config::prepare::build_tracked;
    use std::os::unix::fs::PermissionsExt;
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("rill-auth-{}-{unique}", std::process::id()));
    fs::create_dir(&dir)?;
    let entry = dir.join("config.kdl");
    let token = dir.join("token");
    fs::write(&token, "dummy-value\n")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let config = |action: &str| {
        format!(
            "server {{ http \"127.0.0.1:8080\"; }}\nsite \"example.test\" {{ chain {{ rule {{ {action}; continue; }}; }}; }}\n"
        )
    };
    fs::write(
        &entry,
        config("static-token header=\"X-Api-Token\" file=\"token\""),
    )?;
    let result = build_tracked(&entry, &ConfigurationOnly).map_err(|e| e.error)?;
    assert!(result.dependencies.consumed.contains_key(&token));
    assert_eq!(result.prepared.resources.policy.tokens.len(), 1);
    assert!(!format!("{:?}", result.prepared.resources).contains("dummy-value"));
    fs::set_permissions(&token, fs::Permissions::from_mode(0o644))?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_INVALID_CREDENTIAL"
    );
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    fs::write(&token, "a\nb")?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_INVALID_CREDENTIAL"
    );
    fs::write(&token, vec![b'x'; 4097])?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_INVALID_CREDENTIAL"
    );
    let valid_hash = bcrypt::hash("dummy", 10)?;
    fs::write(
        &entry,
        config(&format!(
            "basic-auth realm=\"private\" {{ user \"alice\" hash=\"{valid_hash}\"; }}"
        )),
    )?;
    build(&entry, &ConfigurationOnly)?;
    fs::write(
        &entry,
        config(&format!(
            "basic-auth realm=\"private\" {{ user \"alice\" hash=\"{valid_hash}\"; user \"alice\" hash=\"{valid_hash}\"; }}"
        )),
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_INVALID_CREDENTIAL"
    );
    fs::write(
        &entry,
        config("static-token header=\"Authorization\" file=\"token\""),
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_PROTECTED_HEADER"
    );
    fs::write(
        &entry,
        config("static-token header=\"X-Token\" file=\"token\" unexpected=\"secret\""),
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "CONFIG_UNKNOWN"
    );
    let secret = "dummy-plaintext-secret";
    fs::write(
        &entry,
        config(&format!(
            "basic-auth realm=\"private\" {{ user \"alice\" hash=\"{secret}\"; }}"
        )),
    )?;
    let error = build(&entry, &ConfigurationOnly).unwrap_err();
    assert_eq!(error.code, "AUTH_INVALID_HASH");
    assert!(!format!("{error:?} {error}").contains(secret));
    fs::write(
        &entry,
        config(
            "branch { when { match { method \"GET\"; }; static-token header=\"X-Api-Token\" file=\"token\"; continue; }; otherwise { header \"request\" { set \"X-Api-Token\" \"fake\"; }; continue; }; }",
        ),
    )?;
    // branch itself terminates its containing sequence
    let text = fs::read_to_string(&entry)?.replace("; }; continue; }; }; }", "; }; }; }; }");
    fs::write(&entry, text)?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "POLICY_PROTECTED_HEADER"
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn authentication_hash_formats_and_fifo_fail_closed() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let bcrypt = bcrypt::hash("dummy", 10)?;
    assert!(rill::auth::hash::valid(&bcrypt));
    assert!(!rill::auth::hash::valid(
        &bcrypt.replace("$2b$10$", "$2b$09$")
    ));
    assert!(!rill::auth::hash::valid(&bcrypt.replace("$2b$", "$2a$")));
    let argon =
        "$argon2id$v=19$m=8192,t=1,p=1$c29tZXNhbHQ$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert!(rill::auth::hash::valid(argon));
    assert!(!rill::auth::hash::valid(
        &argon.replace("m=8192", "m=65537")
    ));
    assert!(!rill::auth::hash::valid(&argon.replace("p=1", "p=1,p=1")));
    assert!(!rill::auth::hash::valid(
        &argon.replace("argon2id", "argon2i")
    ));
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("rill-auth-fifo-{}-{unique}", std::process::id()));
    fs::create_dir(&dir)?;
    let fifo = dir.join("token");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600))?;
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o600))?;
    let entry = dir.join("config.kdl");
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nsite \"example.test\" { chain { rule { static-token header=\"X-Token\" file=\"token\"; continue; }; }; }\n",
    )?;
    assert_eq!(
        build(&entry, &ConfigurationOnly).unwrap_err().code,
        "AUTH_INVALID_CREDENTIAL"
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn authentication_token_resolves_relative_to_import_and_tracks_symlink() -> TestResult {
    use rill::config::prepare::build_tracked;
    use std::os::unix::fs::{PermissionsExt, symlink};
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir =
        std::env::temp_dir().join(format!("rill-auth-import-{}-{unique}", std::process::id()));
    let sub = dir.join("sub");
    fs::create_dir_all(&sub)?;
    let entry = dir.join("config.kdl");
    let imported = sub.join("site.kdl");
    let target = sub.join("secret");
    let link = sub.join("token");
    fs::write(&target, "dummy\n")?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    symlink("secret", &link)?;
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }\nimport \"sub/site.kdl\"\n",
    )?;
    fs::write(
        &imported,
        "site \"example.test\" { chain { rule { static-token header=\"X-Token\" file=\"token\"; continue; }; }; }\n",
    )?;
    let tracked = build_tracked(&entry, &ConfigurationOnly).map_err(|e| e.error)?;
    assert!(tracked.dependencies.paths.contains_key(&link));
    assert!(tracked.dependencies.paths.contains_key(&target));
    assert!(tracked.dependencies.consumed.contains_key(&link));
    fs::remove_file(&link)?;
    symlink("missing", &link)?;
    assert!(!tracked.dependencies.unchanged());
    fs::remove_dir_all(dir)?;
    Ok(())
}
