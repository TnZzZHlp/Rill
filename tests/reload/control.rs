use super::support::{Fixture, Result};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[test]
fn malformed_frames_do_not_publish_and_valid_peer_can_reload() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    fs::write(&f.entry, f.config(&root, "max-age=10"))?;
    f.start()?;
    assert_eq!(fs::metadata(&f.dir)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&f.socket)?.permissions().mode() & 0o777, 0o600);
    let frames: Vec<Vec<u8>> = vec![
        b"{\"version\":1,\"id\":\"x\",\"operation\":\"reload\",\"extra\":1}\n".to_vec(),
        b"{\"version\":1,\"id\":\"x\",\"id\":\"y\",\"operation\":\"reload\"}\n".to_vec(),
        b"{\"version\":2,\"id\":\"x\",\"operation\":\"reload\"}\n".to_vec(),
        b"{\"version\":1,\"id\":\"x\",\"operation\":\"other\"}\n".to_vec(),
        b"{\"version\":1,\"id\":\"bad id\",\"operation\":\"reload\"}\n".to_vec(),
        vec![0xff, b'\n'],
        vec![b'x'; 65537],
    ];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    for frame in frames {
        runtime.block_on(async {
            let mut stream = tokio::net::UnixStream::connect(&f.socket).await?;
            let _ = stream.write_all(&frame).await;
            let result = tokio::time::timeout(
                Duration::from_secs(6),
                rill::control::protocol::read_frame(&mut stream),
            )
            .await?;
            if let Ok(reply) = result {
                let response = rill::control::protocol::decode_response(&reply, "invalid")?;
                assert_eq!(response.code.as_deref(), Some("PROTOCOL"));
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })?;
    }
    assert_eq!(f.reload()?.generation, Some(2));
    assert_eq!(f.cli(&[])?.status.code(), Some(0));
    f.signal()?;
    assert!(!f.socket.exists());
    Ok(())
}

#[test]
fn socket_bind_rejects_existing_entries_and_unsafe_parents() -> Result {
    let f = Fixture::new()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();
    fs::write(&f.socket, "preserve")?;
    assert!(rill::control::socket::ControlSocket::bind(&f.socket).is_err());
    assert_eq!(fs::read(&f.socket)?, b"preserve");
    fs::remove_file(&f.socket)?;
    symlink("missing", &f.socket)?;
    assert!(rill::control::socket::ControlSocket::bind(&f.socket).is_err());
    assert!(fs::symlink_metadata(&f.socket)?.file_type().is_symlink());
    fs::remove_file(&f.socket)?;
    let listener = UnixListener::bind(&f.socket)?;
    assert!(rill::control::socket::ControlSocket::bind(&f.socket).is_err());
    drop(listener);
    fs::remove_file(&f.socket)?;
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o755))?;
    assert!(rill::control::socket::ControlSocket::bind(&f.socket).is_err());
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o700))?;
    let link = f.dir.join("link");
    let parent = f.dir.join("parent");
    fs::create_dir(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    symlink(&parent, &link)?;
    assert!(rill::control::socket::ControlSocket::bind(&link.join("sock")).is_err());
    Ok(())
}

#[test]
fn replacement_file_survives_server_shutdown() -> Result {
    let mut f = Fixture::new()?;
    let root = f.dir.join("root");
    fs::create_dir(&root)?;
    fs::write(&f.entry, f.config(&root, "max-age=10"))?;
    f.start()?;
    fs::remove_file(&f.socket)?;
    fs::write(&f.socket, "replacement")?;
    f.signal()?;
    assert_eq!(fs::read(&f.socket)?, b"replacement");
    Ok(())
}

#[test]
fn cli_exit_codes_for_invalid_arguments_and_missing_instance() -> Result {
    let f = Fixture::new()?;
    assert_eq!(f.cli(&[])?.status.code(), Some(3));
    for args in [
        &["--timeout", "0"][..],
        &["--timeout", "-1s"],
        &["--timeout", "999999999999999999999999999s"],
        &["--unknown"],
    ] {
        assert_eq!(f.cli(args)?.status.code(), Some(2));
    }
    Ok(())
}
