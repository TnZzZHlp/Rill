use super::model::TokenDigest;
use crate::config::{
    ConfigError, Result,
    dependencies::{DependencyPlan, Fingerprint, PathFingerprint},
    source::Located,
};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

pub fn read(
    path: &Located<PathBuf>,
    dependencies: Option<&mut DependencyPlan>,
) -> Result<TokenDigest> {
    let fail = || {
        ConfigError::new(
            "AUTH_INVALID_CREDENTIAL",
            "static-token",
            "invalid token file",
            &path.origin,
        )
    };
    let mut dependencies = dependencies;
    if let Some(plan) = dependencies.as_deref_mut() {
        plan.track_path(&path.value, &path.origin)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(&path.value)
        .map_err(|_| fail())?;
    let before = file.metadata().map_err(|_| fail())?;
    if !before.is_file() || before.mode() & 0o044 != 0 || before.len() > 4096 {
        return Err(fail());
    }
    if let Some(plan) = dependencies {
        let resolved = fs::canonicalize(&path.value).map_err(|_| fail())?;
        if plan.paths.get(&resolved)
            != Some(&PathFingerprint::Present(Fingerprint::from_metadata(
                &before,
            )))
        {
            return Err(fail());
        }
        plan.record_consumed(&path.value, &before, &path.origin)?;
    }
    let mut buffer = [0u8; 4097];
    let mut length = 0;
    let mut reader = &file;
    while length < buffer.len() {
        match reader.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(n) => length += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(fail()),
        }
    }
    let after = file.metadata().map_err(|_| fail())?;
    if Fingerprint::from_metadata(&before) != Fingerprint::from_metadata(&after)
        || fs::metadata(&path.value).map_or(true, |m| {
            Fingerprint::from_metadata(&before) != Fingerprint::from_metadata(&m)
        })
        || length > 4096
    {
        return Err(fail());
    }
    if length > 0 && buffer[length - 1] == b'\n' {
        length -= 1;
    }
    let bytes = &buffer[..length];
    if bytes.is_empty() || bytes.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(fail());
    }
    std::str::from_utf8(bytes).map_err(|_| fail())?;
    Ok(TokenDigest(openssl::sha::sha256(bytes)))
}
