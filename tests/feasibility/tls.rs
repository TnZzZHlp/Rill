use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use openssl::ex_data::Index;
use openssl::ssl::{Ssl, SslAcceptor, SslConnector, SslMethod, SslOptions, SslSessionCacheMode};
use pingora::listeners::TlsAccept;
use pingora::listeners::tls::TlsSettings;
use pingora::protocols::tls::server::handshake_with_callback;
use pingora::protocols::tls::{SslStream, TlsRef};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Notify;
use tokio::time::timeout;

use super::snapshot::{ActiveSnapshot, Snapshot, TestResult};

struct SelectionGate {
    selected: Notify,
    resume: Notify,
}

struct CertificateSelector {
    active: ActiveSnapshot,
    selected: Index<Ssl, Arc<Snapshot>>,
    gate: Option<Arc<SelectionGate>>,
}

impl CertificateSelector {
    fn new(active: ActiveSnapshot, gate: Option<Arc<SelectionGate>>) -> TestResult<Self> {
        Ok(Self {
            active,
            selected: Ssl::new_ex_index()?,
            gate,
        })
    }
}

#[async_trait]
impl TlsAccept for CertificateSelector {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        if ssl.servername(openssl::ssl::NameType::HOST_NAME) != Some("app.example.test") {
            return;
        }
        let snapshot = self.active.load_full();
        if let Some(gate) = &self.gate {
            gate.selected.notify_one();
            gate.resume.notified().await;
        }
        if ssl.set_certificate(&snapshot.certificate).is_err()
            || ssl.set_private_key(&snapshot.key).is_err()
        {
            return;
        }
        ssl.set_ex_data(self.selected, snapshot);
    }

    async fn handshake_complete_callback(
        &self,
        ssl: &TlsRef,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        ssl.ex_data(self.selected)
            .map(|snapshot| snapshot.clone() as Arc<dyn Any + Send + Sync>)
    }
}

fn acceptor() -> TestResult<SslAcceptor> {
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
    builder.set_session_cache_mode(SslSessionCacheMode::OFF);
    builder.set_options(SslOptions::NO_TICKET | SslOptions::NO_RENEGOTIATION);
    builder.set_alpn_select_callback(|_, protocols| {
        openssl::ssl::select_next_proto(b"\x02h2\x08http/1.1", protocols)
            .ok_or(openssl::ssl::AlpnError::NOACK)
    });
    Ok(builder.build())
}

fn client(
    stream: DuplexStream,
    trusted: &[Arc<Snapshot>],
    name: &str,
    alpn: &[u8],
) -> TestResult<SslStream<DuplexStream>> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    for snapshot in trusted {
        builder
            .cert_store_mut()
            .add_cert(snapshot.certificate.clone())?;
    }
    builder.set_alpn_protos(alpn)?;
    let ssl = builder.build().configure()?.into_ssl(name)?;
    Ok(SslStream::new(ssl, stream)?)
}

async fn connect(
    selector: &CertificateSelector,
    trusted: &[Arc<Snapshot>],
    alpn: &[u8],
) -> TestResult<(SslStream<DuplexStream>, SslStream<DuplexStream>)> {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let mut client = client(client_io, trusted, "app.example.test", alpn)?;
    let acceptor = acceptor()?;
    let (server, connected) = tokio::join!(
        handshake_with_callback(&acceptor, server_io, selector),
        client.connect()
    );
    connected?;
    Ok((client, server?))
}

fn selected_generation(stream: &SslStream<DuplexStream>) -> TestResult<u8> {
    let digest = stream.ssl_digest().ok_or("missing TLS digest")?;
    let snapshot = digest
        .extension
        .get::<Snapshot>()
        .ok_or("missing selected snapshot")?;
    let generation = snapshot.generation;
    let certificate = stream.ssl().certificate().ok_or("missing certificate")?;
    if certificate.to_der()? != snapshot.certificate.to_der()? {
        return Err("certificate and snapshot generation differ".into());
    }
    if generation == 0 {
        return Err("invalid snapshot generation".into());
    }
    Ok(generation)
}

#[tokio::test]
async fn new_handshakes_use_new_certificates_without_closing_existing_streams() -> TestResult {
    timeout(Duration::from_secs(20), async {
        let old = Snapshot::prepare(1, "old-backend")?;
        let new = Snapshot::prepare(2, "new-backend")?;
        let active = Arc::new(ArcSwap::from(old.clone()));
        let selector = CertificateSelector::new(active.clone(), None)?;
        let trust = [old.clone(), new.clone()];
        let (mut old_client, mut old_server) = connect(&selector, &trust, b"\x08http/1.1").await?;
        assert_eq!(
            old_client.ssl().selected_alpn_protocol(),
            Some(&b"http/1.1"[..])
        );
        assert_eq!(selected_generation(&old_server)?, 1);
        assert_eq!(
            old_client
                .ssl()
                .peer_certificate()
                .ok_or("missing peer")?
                .to_der()?,
            old.certificate.to_der()?
        );
        let in_flight_request = active.load_full();
        active.store(new.clone());
        let (new_client, new_server) = connect(&selector, &trust, b"\x02h2").await?;
        assert_eq!(new_client.ssl().selected_alpn_protocol(), Some(&b"h2"[..]));
        assert_eq!(selected_generation(&new_server)?, 2);
        assert_eq!(
            new_client
                .ssl()
                .peer_certificate()
                .ok_or("missing peer")?
                .to_der()?,
            new.certificate.to_der()?
        );
        assert_eq!(in_flight_request.route, "old-backend");
        assert_eq!(active.load_full().route, "new-backend");
        let send = async {
            old_client.write_all(b"a").await?;
            old_client.flush().await?;
            let mut response = [0; 1];
            old_client.read_exact(&mut response).await?;
            TestResult::Ok(response)
        };
        let respond = async {
            let mut request = [0; 1];
            old_server.read_exact(&mut request).await?;
            let current_request = active.load_full();
            old_server.write_all(&[current_request.generation]).await?;
            old_server.flush().await?;
            TestResult::Ok(())
        };
        let (response, served) = tokio::join!(send, respond);
        served?;
        assert_eq!(response?, [2]);
        assert_eq!(selected_generation(&old_server)?, 1);
        TestResult::Ok(())
    })
    .await?
}

#[tokio::test]
async fn handshake_crossing_publication_keeps_its_original_selection() -> TestResult {
    timeout(Duration::from_secs(20), async {
        let old = Snapshot::prepare(1, "old-backend")?;
        let new = Snapshot::prepare(2, "new-backend")?;
        let active = Arc::new(ArcSwap::from(old.clone()));
        let gate = Arc::new(SelectionGate {
            selected: Notify::new(),
            resume: Notify::new(),
        });
        let selector = CertificateSelector::new(active.clone(), Some(gate.clone()))?;
        let trust = [old.clone(), new.clone()];
        let publish = async {
            gate.selected.notified().await;
            active.store(new.clone());
            gate.resume.notify_one();
        };
        let (connected, ()) = tokio::join!(connect(&selector, &trust, b"\x02h2"), publish);
        let (client, server) = connected?;
        assert_eq!(selected_generation(&server)?, 1);
        assert_eq!(
            client
                .ssl()
                .peer_certificate()
                .ok_or("missing peer")?
                .to_der()?,
            old.certificate.to_der()?
        );
        assert_eq!(active.load_full().generation, 2);
        TestResult::Ok(())
    })
    .await?
}

#[tokio::test]
async fn unknown_sni_cannot_receive_another_sites_certificate() -> TestResult {
    timeout(Duration::from_secs(20), async {
        let snapshot = Snapshot::prepare(1, "backend")?;
        let active = Arc::new(ArcSwap::from(snapshot.clone()));
        let selector = CertificateSelector::new(active, None)?;
        let (client_io, server_io) = tokio::io::duplex(4096);
        let mut client = client(client_io, &[snapshot], "unknown.example.test", b"\x02h2")?;
        let acceptor = acceptor()?;
        let (server, client) = tokio::join!(
            handshake_with_callback(&acceptor, server_io, &selector),
            client.connect()
        );
        assert!(server.is_err());
        assert!(client.is_err());
        TestResult::Ok(())
    })
    .await?
}

#[test]
fn pingora_listener_exposes_the_same_callbacks_and_http2_settings() -> TestResult {
    println!("TLS library: {}", openssl::version::version());
    let active = Arc::new(ArcSwap::from(Snapshot::prepare(1, "backend")?));
    let mut settings =
        TlsSettings::with_callbacks(Box::new(CertificateSelector::new(active, None)?))?;
    settings.enable_h2();
    settings.set_session_cache_mode(SslSessionCacheMode::OFF);
    settings.set_options(SslOptions::NO_TICKET | SslOptions::NO_RENEGOTIATION);
    Ok(())
}
