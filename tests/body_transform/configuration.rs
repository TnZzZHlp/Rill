use crate::support::*;
use rill::config::{prepare::ConfigurationOnly, prepare::build};
use std::fs;

#[test]
fn body_transform_dsl_rejects_unknown_types_duplicates_and_possible_path_limits() -> TestResult {
    let dir = Directory::new()?;
    let entry = dir.0.join("config.kdl");
    let check = |actions: &str| {
        fs::write(&entry, format!("server {{ http \"127.0.0.1:8080\"; }}; site \"app.test\" {{ chain {{ rule {{ {actions} respond 200; }}; }}; }}")).unwrap();
        build(&entry, &ConfigurationOnly)
    };
    for invalid in [
        "body-replace { literal \"a\" \"b\"; };",
        "body-replace mime=3 { literal \"a\" \"b\"; };",
        "body-replace mime=\"text/*\" { literal \"a\" \"b\"; };",
        "body-replace mime=\"text/html\" extra=\"bad\" { literal \"a\" \"b\"; };",
        "body-replace mime=\"text/html\" { mime \"text/html\"; literal \"a\" \"b\"; };",
        "body-replace mime=\"text/html\" { literal \"a\" \"b\" extra=1; };",
        "body-replace mime=\"text/html\" { regex \"a\" \"$10\"; };",
        "body-replace mime=\"text/html\" { regex \"(?=a)\" \"b\"; };",
        "body-replace mime=\"text/html\" { regex \"a{1001}\" \"b\"; };",
        "encode mime=\"text/html\" {};",
        "encode mime=\"text/html\" { gzip level=9; };",
        "encode mime=\"text/html\" { gzip level=\"3\"; };",
        "encode mime=\"text/html\" { gzip level=3 level=3; };",
        "encode mime=\"text/html\" { gzip level=3; gzip level=3; };",
        "encode mime=\"text/html\" { gzip level=3; }; encode mime=\"text/plain\" { zstd level=3; };",
    ] {
        assert!(check(invalid).is_err(), "accepted {invalid}");
    }
    assert!(check("body-replace mime=\"text/html\" { mime \"application/json\"; literal \"a\" \"b\"; regex \"(?-u:.)\" \"$9!$$\"; }; encode mime=\"text/html\" { gzip level=3; zstd level=3; };").is_ok());
    let rules = "literal \"a\" \"b\";".repeat(9);
    assert!(check(&format!("body-replace mime=\"text/html\" {{ {rules} }}; body-replace mime=\"text/html\" {{ {rules} }};")).is_err());
    fs::write(
        &entry,
        format!(
            "server {{ http \"127.0.0.1:8080\"; }}; site \"app.test\" {{ chain {{ rule {{ body-replace mime=\"text/html\" {{ {rules} }}; continue; }}; rule {{ serve-files root={:?}; }}; handle-errors {{ rule {{ body-replace mime=\"text/html\" {{ {rules} }}; respond 404; }}; }}; }}; }}",
            dir.0
        ),
    )?;
    assert!(build(&entry, &ConfigurationOnly).is_err());
    let declaration = format!("body-replace mime=\"text/html\" {{ {rules} }};");
    for (chain, accepted) in [
        (
            format!(
                "rule {{ branch {{ when {{ match {{ method \"GET\"; }}; {declaration} respond 200; }}; otherwise {{ {declaration} respond 200; }}; }}; }};"
            ),
            true,
        ),
        (
            format!(
                "rule {{ {declaration} continue; }}; handle-errors {{ rule {{ {declaration} respond 404; }}; }};"
            ),
            false,
        ),
        (
            format!(
                "rule {{ {declaration} branch {{ when {{ match {{ method \"GET\"; }}; respond 200; }}; }}; }}; handle-errors {{ rule {{ {declaration} respond 404; }}; }};"
            ),
            false,
        ),
        (
            format!(
                "rule {{ body-replace mime=\"text/html\" {{ {} }}; reenter; }};",
                "literal \"a\" \"b\";".repeat(4)
            ),
            false,
        ),
        (
            format!(
                "rule {{ {declaration} strip-prefix \"/required\"; respond 200; }}; handle-errors {{ rule {{ {declaration} respond 404; }}; }};"
            ),
            false,
        ),
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}; site \"app.test\" {{ chain {{ {chain} }}; }}"
            ),
        )?;
        assert_eq!(
            build(&entry, &ConfigurationOnly).is_ok(),
            accepted,
            "{chain}"
        );
    }
    Ok(())
}
