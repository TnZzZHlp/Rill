use std::fs;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslConnector, SslMethod, SslStream};
use openssl::x509::extension::{
    BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
};
use openssl::x509::{X509, X509NameBuilder};

pub use crate::transport::*;

pub struct Certificate {
    pub cert: X509,
    pub key: PKey<Private>,
}

impl Certificate {
    pub fn issue(
        names: &[&str],
        issuer: Option<&Self>,
        ca: bool,
        validity: (i64, i64),
    ) -> TestResult<Self> {
        Self::issue_inner(names, None, issuer, ca, validity)
    }

    pub fn issue_ip(ip: IpAddr, issuer: &Self, validity: (i64, i64)) -> TestResult<Self> {
        Self::issue_inner(&[], Some(ip), Some(issuer), false, validity)
    }

    fn issue_inner(
        names: &[&str],
        ip: Option<IpAddr>,
        issuer: Option<&Self>,
        ca: bool,
        validity: (i64, i64),
    ) -> TestResult<Self> {
        let key = PKey::ec_gen("prime256v1")?;
        let mut subject = X509NameBuilder::new()?;
        static NEXT_CA: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let issuer_name = format!(
            "test-ca-{}",
            NEXT_CA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        subject.append_entry_by_text(
            "CN",
            names
                .first()
                .copied()
                .unwrap_or(if ca { &issuer_name } else { "app.test" }),
        )?;
        let subject = subject.build();
        let mut builder = X509::builder()?;
        builder.set_version(2)?;
        let mut number = BigNum::new()?;
        number.rand(128, openssl::bn::MsbOption::MAYBE_ZERO, false)?;
        let serial = Asn1Integer::from_bn(&number)?;
        builder.set_serial_number(&serial)?;
        builder.set_subject_name(&subject)?;
        builder.set_issuer_name(issuer.map_or(&subject, |issuer| issuer.cert.subject_name()))?;
        builder.set_pubkey(&key)?;
        let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
        let before = Asn1Time::from_unix(now + validity.0)?;
        let after = Asn1Time::from_unix(now + validity.1)?;
        builder.set_not_before(&before)?;
        builder.set_not_after(&after)?;
        let mut basic = BasicConstraints::new();
        basic.critical();
        if ca {
            basic.ca();
        }
        builder.append_extension(basic.build()?)?;
        let mut usage = KeyUsage::new();
        usage.digital_signature();
        if ca {
            usage.key_cert_sign().crl_sign();
        }
        builder.append_extension(usage.build()?)?;
        if !ca {
            builder.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;
        }
        if !names.is_empty() || ip.is_some() {
            let mut san = SubjectAlternativeName::new();
            for name in names {
                san.dns(name);
            }
            if let Some(ip) = ip {
                san.ip(&ip.to_string());
            }
            let san = san
                .build(&builder.x509v3_context(issuer.map(|issuer| issuer.cert.as_ref()), None))?;
            builder.append_extension(san)?;
        }
        builder.sign(
            issuer.map_or(&key, |issuer| &issuer.key),
            MessageDigest::sha256(),
        )?;
        Ok(Self {
            cert: builder.build(),
            key,
        })
    }

    pub fn save(&self, directory: &Path, name: &str, chain: &[&Certificate]) -> TestResult {
        let mut bytes = self.cert.to_pem()?;
        for issuer in chain {
            bytes.extend(issuer.cert.to_pem()?);
        }
        fs::write(directory.join(format!("{name}.pem")), bytes)?;
        let path = directory.join(format!("{name}.key"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = options.mode(0o600).open(&path)?;
        file.write_all(&self.key.private_key_to_pem_pkcs8()?)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

pub fn tls_connect(
    address: SocketAddr,
    name: &str,
    certificates: &[&Certificate],
    sni: bool,
) -> TestResult<SslStream<TcpStream>> {
    tls_connect_with_alpn(address, name, certificates, sni, b"\x08http/1.1")
}

pub fn tls_connect_with_alpn(
    address: SocketAddr,
    name: &str,
    certificates: &[&Certificate],
    sni: bool,
    alpn: &[u8],
) -> TestResult<SslStream<TcpStream>> {
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    for certificate in certificates {
        builder
            .cert_store_mut()
            .add_cert(certificate.cert.clone())?;
    }
    builder.set_alpn_protos(alpn)?;
    let mut connection = builder.build().configure()?;
    connection.set_use_server_name_indication(sni);
    Ok(connection.connect(name, connect(address)?)?)
}

pub fn tls_request(
    stream: &mut SslStream<TcpStream>,
    host: &str,
    path: &str,
) -> TestResult<(u16, String, Vec<u8>)> {
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;
    let headers = head(stream)?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("missing status")?
        .parse()?;
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    std::io::Read::read_exact(stream, &mut body)?;
    Ok((status, headers, body))
}

pub fn configuration(
    directory: &Path,
    http: Option<SocketAddr>,
    https: SocketAddr,
    upstream: SocketAddr,
    sites: &str,
) -> String {
    let http = http.map_or(String::new(), |http| format!("http \"{http}\";"));
    format!("server {{ {http} https \"{https}\"; }}\ntls \"default\" {{ cert \"{}\"; key \"{}\"; }}\n{sites}\n", directory.join("default.pem").display(), directory.join("default.key").display()).replace("$UPSTREAM", &upstream.to_string())
}
