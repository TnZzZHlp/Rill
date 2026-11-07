use std::any::Any;
use std::sync::Arc;

use crate::config::normalize_domain;
use crate::observability::logging::Level;
use crate::runtime::state::{RuntimeSnapshot, RuntimeState};
use async_trait::async_trait;
use openssl::ex_data::Index;
use openssl::ssl::{NameType, Ssl, SslAcceptor};
use pingora::listeners::TlsAccept;
use pingora::protocols::Stream;
use pingora::protocols::tls::TlsRef;
use pingora::protocols::tls::server::handshake_with_callback;

#[derive(Debug)]
pub struct Handshake {
    pub server_name: String,
    pub snapshot: Arc<RuntimeSnapshot>,
}

pub struct Resolver {
    state: Arc<RuntimeState>,
    selected: Index<Ssl, Arc<Handshake>>,
    acceptor: SslAcceptor,
}

impl Resolver {
    pub fn new(state: Arc<RuntimeState>) -> Result<Arc<Self>, openssl::error::ErrorStack> {
        Ok(Arc::new(Self {
            state,
            selected: Ssl::new_ex_index()?,
            acceptor: super::cert::builder()?.build(),
        }))
    }

    pub async fn accept<S: pingora::protocols::IO + 'static>(
        &self,
        stream: S,
    ) -> pingora::Result<Stream> {
        let stream = handshake_with_callback(&self.acceptor, stream, self).await?;
        let identity = stream
            .ssl_digest()
            .is_some_and(|digest| digest.extension.get::<Handshake>().is_some());
        if !identity {
            return Err(pingora::Error::explain(
                pingora::ErrorType::TLSHandshakeFailure,
                "missing TLS handshake identity",
            ));
        }
        Ok(Box::new(stream))
    }
}

#[async_trait]
impl TlsAccept for Resolver {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        let snapshot = self.state.snapshot();
        let Some(server_name) = ssl
            .servername(NameType::HOST_NAME)
            .and_then(|name| normalize_domain(name, false))
        else {
            return;
        };
        let Some(site) = snapshot
            .router
            .site(&server_name)
            .and_then(|index| snapshot.configuration.sites.get(index))
        else {
            return;
        };
        if site.tls.is_none() {
            return;
        }
        let Some(certificate) = snapshot.tls.certificate(&server_name) else {
            return;
        };
        let installed = ssl
            .set_certificate(&certificate.leaf)
            .and_then(|_| ssl.set_private_key(&certificate.key))
            .and_then(|_| {
                for issuer in &certificate.chain {
                    ssl.add_chain_cert(issuer.clone())?;
                }
                Ok(())
            });
        if installed.is_err() {
            snapshot
                .logger
                .event(Level::Error, "tls_install_failed", &[]);
            return;
        }
        ssl.set_ex_data(
            self.selected,
            Arc::new(Handshake {
                server_name,
                snapshot,
            }),
        );
    }

    async fn handshake_complete_callback(
        &self,
        ssl: &TlsRef,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        ssl.ex_data(self.selected)
            .map(|identity| identity.clone() as Arc<dyn Any + Send + Sync>)
    }
}
