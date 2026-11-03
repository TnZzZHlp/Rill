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
