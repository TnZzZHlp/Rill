use std::net::IpAddr;
use std::sync::Arc;

use pingora::protocols::Digest;
use pingora::protocols::tls::HandshakeCompleteHook;
use pingora::upstreams::peer::{HttpPeer, Peer};
use pingora::{Error, ErrorType};

#[derive(Debug)]
pub struct VerifiedIp(pub IpAddr);

pub fn ip_hook(ip: IpAddr) -> HandshakeCompleteHook {
    Arc::new(move |tls| {
        let certificate = tls.peer_certificate()?;
        let names = certificate.subject_alt_names()?;
        let matches = names
            .iter()
            .filter_map(|name| name.ipaddress())
            .any(|bytes| match ip {
                IpAddr::V4(address) => bytes == address.octets(),
                IpAddr::V6(address) => bytes == address.octets(),
            });
        matches.then(|| Arc::new(VerifiedIp(ip)) as Arc<dyn std::any::Any + Send + Sync>)
    })
}

pub fn verify(peer: &HttpPeer, digest: Option<&Digest>) -> pingora::Result<()> {
    if !peer.tls() {
        return Ok(());
    }
    let Ok(ip) = peer.sni().parse::<IpAddr>() else {
        return Ok(());
    };
    let verified = digest
        .and_then(|d| d.ssl_digest.as_ref())
        .and_then(|d| d.extension.get::<VerifiedIp>())
        .is_some_and(|value| value.0 == ip);
    if verified {
        Ok(())
    } else {
        Err(Error::explain(
            ErrorType::InvalidCert,
            "upstream certificate IP identity mismatch",
        )
        .into_up())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct SystemTrustPaths {
    pub file: std::path::PathBuf,
    pub directories: std::ffi::OsString,
}

pub fn system_trust_paths() -> Option<SystemTrustPaths> {
    let probe = openssl_probe::probe();
    let file = probe
        .cert_file
        .map(std::path::PathBuf::into_os_string)
        .or_else(|| std::env::var_os("SSL_CERT_FILE"));
    let directories = probe
        .cert_dir
        .map(std::path::PathBuf::into_os_string)
        .or_else(|| std::env::var_os("SSL_CERT_DIR"));
    let default = if file.is_none() || directories.is_none() {
        let text = openssl::version::dir();
        let path = text.strip_prefix("OPENSSLDIR: \"")?.strip_suffix('\"')?;
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() {
            return None;
        }
        Some(path)
    } else {
        None
    };
    let file = match file {
        Some(file) => std::path::PathBuf::from(file),
        None => default.as_ref()?.join("cert.pem"),
    };
    let directories = match directories {
        Some(directories) => directories,
        None => default?.join("certs").into_os_string(),
    };
    Some(SystemTrustPaths { file, directories })
}

#[cfg(test)]
mod tests {
    use super::system_trust_paths;

    #[test]
    fn system_trust_paths_match_probe_initialization() {
        const CHILD: &str = "RILL_TRUST_PATH_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let probe = openssl_probe::probe();
            let paths = system_trust_paths().expect("OpenSSL default paths");
            let file = probe
                .cert_file
                .map(std::path::PathBuf::into_os_string)
                .or_else(|| std::env::var_os("SSL_CERT_FILE"));
            let dir = probe
                .cert_dir
                .map(std::path::PathBuf::into_os_string)
                .or_else(|| std::env::var_os("SSL_CERT_DIR"));
            if let Some(file) = file {
                assert_eq!(paths.file.as_os_str(), file);
            }
            if let Some(dir) = dir {
                assert_eq!(paths.directories, dir);
            }
            return;
        }
        let executable = std::env::current_exe().expect("test executable");
        for value in ["", "/nonexistent/rill-ca", "/nonexistent/a:/nonexistent/b"] {
            let status = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "proxy::upstream_tls::tests::system_trust_paths_match_probe_initialization",
                ])
                .env(CHILD, "1")
                .env("SSL_CERT_FILE", value)
                .env("SSL_CERT_DIR", value)
                .status()
                .expect("child test");
            assert!(status.success());
        }
    }
}
