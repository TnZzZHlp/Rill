use std::fs;
use std::net::TcpListener;
use std::process::Command;

use super::support::*;

#[test]
fn check_validates_certificates_without_binding_or_contacting_a_service() -> TestResult {
    let directory = Directory::new()?;
    let cert = Certificate::issue(&["app.test", "alias.test"], None, false, (-60, 3600))?;
    cert.save(&directory.0, "default", &[])?;
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let config = configuration(
        &directory.0,
        None,
        occupied.local_addr()?,
        "127.0.0.1:3000".parse()?,
        "site \"app.test\" \"alias.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
    );
    let path = directory.0.join("config.kdl");
    fs::write(&path, config)?;
    let before = fs::read(directory.0.join("default.key"))?;
    let output = Command::new(env!("CARGO_BIN_EXE_rill"))
        .args(["check", "--config"])
        .arg(&path)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("TLS certificates"));
    assert_eq!(fs::read(directory.0.join("default.key"))?, before);
    Ok(())
}

#[test]
fn invalid_certificates_and_keys_fail_with_located_redacted_errors() -> TestResult {
    for case in [
        "expired",
        "future",
        "mismatch",
        "uncovered",
        "cn-only",
        "encrypted",
        "malformed",
        "extra-key",
        "san-trailing-dot",
    ] {
        let directory = Directory::new()?;
        let validity = match case {
            "expired" => (-7200, -3600),
            "future" => (3600, 7200),
            _ => (-60, 3600),
        };
        let names: &[&str] = match case {
            "cn-only" => &[],
            "san-trailing-dot" => &["app.test."],
            _ => &["app.test"],
        };
        let cert = Certificate::issue(names, None, false, validity)?;
        cert.save(&directory.0, "default", &[])?;
        if case == "mismatch" {
            let other = Certificate::issue(&["app.test"], None, false, (-60, 3600))?;
            fs::write(
                directory.0.join("default.key"),
                other.key.private_key_to_pem_pkcs8()?,
            )?;
        }
        if case == "encrypted" {
            fs::write(
                directory.0.join("default.key"),
                cert.key.private_key_to_pem_pkcs8_passphrase(
                    openssl::symm::Cipher::aes_256_cbc(),
                    b"secret-key-password",
                )?,
            )?;
        }
        if case == "malformed" {
            fs::write(directory.0.join("default.pem"), b"not-a-certificate-secret")?;
        }
        if case == "extra-key" {
            let mut bytes = cert.key.private_key_to_pem_pkcs8()?;
            bytes.extend(cert.key.private_key_to_pem_pkcs8()?);
            fs::write(directory.0.join("default.key"), bytes)?;
        }
        let domain = if case == "uncovered" {
            "other.test"
        } else {
            "app.test"
        };
        let config = configuration(
            &directory.0,
            None,
            "127.0.0.1:8443".parse()?,
            "127.0.0.1:3000".parse()?,
            &format!("site \"{domain}\" {{ tls \"default\"; proxy \"http://$UPSTREAM\"; }}"),
        );
        let path = directory.0.join("config.kdl");
        fs::write(&path, config)?;
        let output = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config"])
            .arg(&path)
            .output()?;
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{case}: {error}");
        assert!(output.stdout.is_empty());
        assert!(
            error.contains("TLS_INVALID") && error.contains(":"),
            "{case}: {error}"
        );
        assert!(
            !error.contains("PRIVATE KEY")
                && !error.contains("secret-key-password")
                && !error.contains("not-a-certificate-secret"),
            "{case}: {error}"
        );
    }
    Ok(())
}

#[test]
fn domain_mapping_never_falls_back_to_an_unassigned_certificate() -> TestResult {
    let directory = Directory::new()?;
    let first = Certificate::issue(&["a.test", "b.test"], None, false, (-60, 3600))?;
    let second = Certificate::issue(&["b.test"], None, false, (-60, 3600))?;
    first.save(&directory.0, "default", &[])?;
    second.save(&directory.0, "second", &[])?;
    let entry = directory.0.join("config.kdl");
    for target in ["second", "missing"] {
        let mut config = configuration(
            &directory.0,
            None,
            "127.0.0.1:8443".parse()?,
            "127.0.0.1:3000".parse()?,
            &format!(
                "site {{ domains {{ \"a.test\" {{ tls \"{target}\"; }}; \"b.test\" {{ tls \"second\"; }}; }}; chain {{ rule {{ respond 200; }}; }}; }}"
            ),
        );
        config.push_str(&format!(
            "tls \"second\" {{ cert \"{}\"; key \"{}\"; }}\n",
            directory.0.join("second.pem").display(),
            directory.0.join("second.key").display()
        ));
        fs::write(&entry, config)?;
        let error = rill::config::prepare::build(&entry, &rill::config::prepare::ConfigurationOnly)
            .expect_err("unassigned certificate used as fallback");
        assert_eq!(error.code, "TLS_INVALID");
    }
    Ok(())
}

#[test]
fn wildcard_coverage_requires_the_entire_configured_namespace() -> TestResult {
    for (names, accepted) in [
        (vec!["*.example.test"], true),
        (vec!["app.example.test", "www.example.test"], false),
        (vec!["*.test"], false),
    ] {
        let directory = Directory::new()?;
        let cert = Certificate::issue(&names, None, false, (-60, 3600))?;
        cert.save(&directory.0, "default", &[])?;
        let path = directory.0.join("config.kdl");
        fs::write(
            &path,
            configuration(
                &directory.0,
                None,
                "127.0.0.1:8443".parse()?,
                "127.0.0.1:3000".parse()?,
                "site \"*.example.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
            ),
        )?;
        let output = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config"])
            .arg(path)
            .output()?;
        assert_eq!(
            output.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
fn malformed_or_expired_intermediate_chains_are_not_ignored() -> TestResult {
    let root = Certificate::issue(&[], None, true, (-3600, 7200))?;
    let intermediate = Certificate::issue(&[], Some(&root), true, (-60, 3600))?;
    let leaf = Certificate::issue(&["app.test"], Some(&intermediate), false, (-60, 3600))?;
    let expired = Certificate::issue(&[], Some(&root), true, (-7200, -3600))?;
    let expired_leaf = Certificate::issue(&["app.test"], Some(&expired), false, (-60, 3600))?;
    for (leaf, chain, accepted) in [
        (&leaf, vec![&intermediate, &root], true),
        (&leaf, vec![&root, &intermediate], false),
        (&expired_leaf, vec![&expired, &root], false),
    ] {
        let directory = Directory::new()?;
        leaf.save(&directory.0, "default", &chain)?;
        let path = directory.0.join("config.kdl");
        fs::write(
            &path,
            configuration(
                &directory.0,
                None,
                "127.0.0.1:8443".parse()?,
                "127.0.0.1:3000".parse()?,
                "site \"app.test\" { tls \"default\"; proxy \"http://$UPSTREAM\"; }",
            ),
        )?;
        let output = Command::new(env!("CARGO_BIN_EXE_rill"))
            .args(["check", "--config"])
            .arg(path)
            .output()?;
        assert_eq!(
            output.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
