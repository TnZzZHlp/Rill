use std::sync::Arc;

use arc_swap::ArcSwap;
use openssl::asn1::{Asn1Integer, Asn1Time};
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509, X509NameBuilder};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type ActiveSnapshot = Arc<ArcSwap<Snapshot>>;

pub struct Snapshot {
    pub generation: u8,
    pub route: &'static str,
    pub certificate: X509,
    pub key: PKey<Private>,
}

impl Snapshot {
    pub fn prepare(generation: u8, route: &'static str) -> TestResult<Arc<Self>> {
        let key = PKey::from_rsa(Rsa::generate(2048)?)?;
        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", &format!("rill-generation-{generation}"))?;
        let name = name.build();
        let mut certificate = X509::builder()?;
        certificate.set_version(2)?;
        let serial_number = BigNum::from_u32(u32::from(generation))?;
        let serial = Asn1Integer::from_bn(&serial_number)?;
        certificate.set_serial_number(&serial)?;
        certificate.set_subject_name(&name)?;
        certificate.set_issuer_name(&name)?;
        certificate.set_pubkey(&key)?;
        let not_before = Asn1Time::days_from_now(0)?;
        let not_after = Asn1Time::days_from_now(1)?;
        certificate.set_not_before(&not_before)?;
        certificate.set_not_after(&not_after)?;
        let san = SubjectAlternativeName::new()
            .dns("app.example.test")
            .build(&certificate.x509v3_context(None, None))?;
        certificate.append_extension(san)?;
        certificate.sign(&key, MessageDigest::sha256())?;
        Ok(Arc::new(Self {
            generation,
            route,
            certificate: certificate.build(),
            key,
        }))
    }
}
