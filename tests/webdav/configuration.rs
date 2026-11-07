use crate::support::{Directory, TestResult};
use rill::config::prepare::{ConfigurationOnly, build};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn webdav_strict_dsl_requires_auth_on_every_possible_path_and_check_is_read_only() -> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::write(directory.0.join("token"), "configuration-fixture")?;
    fs::set_permissions(directory.0.join("token"), fs::Permissions::from_mode(0o600))?;
    let entry = directory.0.join("config.kdl");
    let auth = "static-token header=\"X-Token\" file=\"token\"";
    let dav = "webdav root=\"root\" storage-id=\"documents\"";
    for (rules, expected) in [
        (format!("rule {{ {auth}; {dav}; }};"), None),
        (format!("rule {{ {dav}; }};"), Some("DAV_AUTH_REQUIRED")),
        (
            format!(
                "rule {{ match {{ path-glob \"/private/*\"; }}; {auth}; continue; }}; rule {{ {dav}; }};"
            ),
            Some("DAV_AUTH_REQUIRED"),
        ),
        (
            format!(
                "rule {{ branch {{ when {{ match {{ method \"GET\"; }}; {auth}; continue; }}; otherwise {{ continue; }}; }}; }}; rule {{ {dav}; }};"
            ),
            Some("DAV_AUTH_REQUIRED"),
        ),
        (
            format!("rule {{ {auth}; continue; }}; rule {{ {dav}; }};"),
            None,
        ),
        (
            format!("rule {{ respond 404; }}; handle-errors {{ rule {{ {dav}; }}; }};"),
            Some("DAV_AUTH_REQUIRED"),
        ),
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}; site \"dav.test\" {{ chain {{ {rules} }}; }}"
            ),
        )?;
        match (build(&entry, &ConfigurationOnly), expected) {
            (Ok(_), None) => (),
            (Err(error), Some(code)) => assert_eq!(error.code, code),
            (result, expected) => panic!("unexpected build {result:?}, expected {expected:?}"),
        }
        assert!(!root.join(".rill-dav").exists());
    }
    for invalid in [
        "webdav storage-id=\"documents\"",
        "webdav root=\"root\"",
        "webdav root=#true storage-id=\"documents\"",
        "webdav root=\"root\" storage-id=#true",
        "webdav root=\"root\" storage-id=\"documents\" anonymous=#true",
        "webdav \"root\" storage-id=\"documents\"",
        "webdav root=\"root\" root=\"root\" storage-id=\"documents\"",
        "webdav root=\"root\" storage-id=\"documents\" storage-id=\"other\"",
        "webdav root=\"root\" storage-id=\"documents\" {}",
    ] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}; site \"dav.test\" {{ chain {{ rule {{ {auth}; {invalid}; }}; }}; }}"
            ),
        )?;
        assert!(build(&entry, &ConfigurationOnly).is_err());
        assert!(!root.join(".rill-dav").exists());
    }
    Ok(())
}

#[tokio::test]
async fn webdav_activation_denies_private_names_and_inode_aliases_to_old_static_snapshot()
-> TestResult {
    use rill::runtime::state::{HttpPreparer, PreparedRoute, RuntimeState};
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), b"ordinary")?;
    let entry = directory.0.join("config.kdl");
    let original = format!(
        "server {{http \"127.0.0.1:8080\"; static-cache-bytes 65536;}}\nsite \"static.test\" {{root \"{}\";browse #true;}}",
        root.display()
    );
    fs::write(&entry, &original)?;
    let state = RuntimeState::new(build(&entry, &HttpPreparer)?)?;
    let snapshot = state.snapshot();
    let PreparedRoute::Static(route) = &snapshot.routes[0][0] else {
        return Err("missing static root".into());
    };
    let mut request = pingora::http::RequestHeader::build("GET", b"/file", None)?;
    assert_eq!(
        rill::static_files::prepare(route.clone(), &request, "/file", false, false)
            .await?
            .header
            .status
            .as_u16(),
        200
    );
    let token = directory.0.join("token");
    fs::write(&token, "snapshot-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    fs::write(
        &entry,
        format!(
            "{original}\nsite \"dav.test\" {{chain {{rule {{static-token header=\"X-Token\" file=\"{}\";webdav root=\"{}\" storage-id=\"old-static-proof\";}};}};}}",
            token.display(),
            root.display()
        ),
    )?;
    let _dav = RuntimeState::new(build(&entry, &HttpPreparer)?)?;
    std::os::unix::fs::symlink(".rill-dav/.owner", root.join("private-link"))?;
    fs::hard_link(root.join(".rill-dav/.owner"), root.join("private-hardlink"))?;
    for path in ["/.rill-dav/.owner", "/private-link", "/private-hardlink"] {
        request.set_raw_path(path.as_bytes())?;
        assert_eq!(
            rill::static_files::prepare(route.clone(), &request, path, false, false)
                .await?
                .header
                .status
                .as_u16(),
            403
        );
    }
    fs::remove_file(root.join("private-hardlink"))?;
    request.set_raw_path(b"/file")?;
    assert_eq!(
        rill::static_files::prepare(route.clone(), &request, "/file", false, false)
            .await?
            .header
            .status
            .as_u16(),
        200
    );
    Ok(())
}

#[test]
fn webdav_preparation_rejects_parallel_lock_domains_and_unowned_metadata() -> TestResult {
    let directory = Directory::new()?;
    fs::create_dir(directory.0.join("root"))?;
    fs::create_dir(directory.0.join("root/child"))?;
    fs::write(directory.0.join("token"), "configuration-fixture")?;
    fs::set_permissions(directory.0.join("token"), fs::Permissions::from_mode(0o600))?;
    let entry = directory.0.join("config.kdl");
    for second in ["root", "root/child"] {
        fs::write(
            &entry,
            format!(
                "server {{ http \"127.0.0.1:8080\"; }}; site \"dav.test\" {{ chain {{ rule {{ static-token header=\"X-Token\" file=\"token\"; branch {{ when {{ match {{ path-glob \"/a/*\"; }}; webdav root=\"root\" storage-id=\"one\"; }}; otherwise {{ webdav root=\"{second}\" storage-id=\"two\"; }}; }}; }}; }}; }}"
            ),
        )?;
        assert_eq!(
            build(&entry, &ConfigurationOnly).unwrap_err().code,
            "DAV_ROOT_OVERLAP"
        );
        assert!(!directory.0.join("root/.rill-dav").exists());
    }
    fs::create_dir(directory.0.join("root/.rill-dav"))?;
    fs::set_permissions(
        directory.0.join("root/.rill-dav"),
        fs::Permissions::from_mode(0o700),
    )?;
    fs::write(directory.0.join("root/.rill-dav/user-file"), "untouched")?;
    fs::write(
        &entry,
        "server { http \"127.0.0.1:8080\"; }; site \"dav.test\" { chain { rule { static-token header=\"X-Token\" file=\"token\"; webdav root=\"root\" storage-id=\"documents\"; }; }; }",
    )?;
    assert!(build(&entry, &ConfigurationOnly).is_err());
    assert_eq!(
        fs::read(directory.0.join("root/.rill-dav/user-file"))?,
        b"untouched"
    );
    Ok(())
}
