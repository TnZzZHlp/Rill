use super::support::request;
use crate::support::{Directory, Proxy, TestResult, vacant};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn dav_get_filters_keep_original_metadata_range_head_and_write_response_semantics() -> TestResult {
    let directory = Directory::new()?;
    let root = directory.0.join("root");
    fs::create_dir(&root)?;
    let token = directory.0.join("token");
    fs::write(&token, "filter-fixture")?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let address = vacant()?;
    let config = format!(
        r#"server {{http "{address}";}}
site "app.test" {{chain {{rule {{static-token header="X-Token" file="{}";body-replace mime="application/octet-stream" {{literal "old" "new";}};header "response" {{set "X-Dav-Test" "frozen";}};webdav root="{}" storage-id="documents";}};handle-errors {{rule {{respond 404 text="not-success";}};}};}};}}"#,
        token.display(),
        root.display()
    );
    let _proxy = Proxy::start(address, &config)?;
    let auth = "X-Token: filter-fixture\r\n";
    let (status, _, body) = request(address, "PUT", "/file", auth, b"old content")?;
    assert_eq!(status, 201);
    assert!(body.is_empty());
    assert_eq!(fs::read(root.join("file"))?, b"old content");
    let (status, headers, body) = request(
        address,
        "GET",
        "/file",
        &format!("{auth}If-None-Match: *\r\n"),
        b"",
    )?;
    assert_eq!(status, 200);
    assert_eq!(body, b"new content");
    assert!(!headers.to_ascii_lowercase().contains("etag:"));
    assert!(!headers.to_ascii_lowercase().contains("last-modified:"));
    assert!(headers.to_ascii_lowercase().contains("x-dav-test: frozen"));
    let (status, _, body) = request(
        address,
        "GET",
        "/file",
        &format!("{auth}Range: bytes=0-2\r\n"),
        b"",
    )?;
    assert_eq!(status, 206);
    assert_eq!(body, b"old");
    let (status, headers, body) = request(address, "HEAD", "/file", auth, b"")?;
    assert_eq!(status, 200);
    assert!(body.is_empty());
    assert!(!headers.to_ascii_lowercase().contains("content-length:"));
    assert_eq!(
        request(
            address,
            "PUT",
            "/file",
            &format!("{auth}If-Match: \"wrong\"\r\n"),
            b"not committed"
        )?
        .0,
        412
    );
    assert_eq!(fs::read(root.join("file"))?, b"old content");
    Ok(())
}
