use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use openssl::asn1::Asn1Time;
use openssl::error::ErrorStack;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{
    SslAcceptor, SslAcceptorBuilder, SslMethod, SslOptions, SslSessionCacheMode, SslVersion,
};
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::verify::X509VerifyFlags;
use openssl::x509::{X509, X509PurposeId, X509StoreContext};

use crate::config::model::ValidatedConfig;
use crate::config::source::{Located, Origin};
use crate::config::{ConfigError, Result, normalize_domain};

pub struct Certificate {
    pub(crate) leaf: X509,
    pub(crate) chain: Vec<X509>,
    pub(crate) key: PKey<Private>,
    names: Vec<String>,
    _context: SslAcceptor,
}

impl std::fmt::Debug for Certificate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Certificate")
            .field("names", &self.names)
            .field("chain_length", &self.chain.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct PreparedTls {
    bindings: BTreeMap<String, Arc<Certificate>>,
}

pub(crate) fn builder() -> std::result::Result<SslAcceptorBuilder, ErrorStack> {
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_session_cache_mode(SslSessionCacheMode::OFF);
    builder.set_options(SslOptions::NO_TICKET | SslOptions::NO_RENEGOTIATION);
    builder.set_num_tickets(0)?;
    builder.set_alpn_select_callback(|_, protocols| {
        openssl::ssl::select_next_proto(b"\x02h2\x08http/1.1", protocols)
            .ok_or(openssl::ssl::AlpnError::NOACK)
    });
    Ok(builder)
}

fn invalid(item: &'static str, reason: &'static str, origin: &Origin) -> ConfigError {
    ConfigError::new("TLS_INVALID", item, reason, origin)
}

fn read(
    path: &Located<std::path::PathBuf>,
    limit: u64,
    item: &'static str,
    dependencies: Option<&mut crate::config::dependencies::DependencyPlan>,
) -> Result<Vec<u8>> {
    let fail = || {
        ConfigError::new(
            "RESOURCE_IO",
            item,
            "cannot read a regular PEM resource",
            &path.origin,
        )
    };
    if !fs::metadata(&path.value).map_err(|_| fail())?.is_file() {
        return Err(fail());
    }
    let file = File::open(&path.value).map_err(|_| fail())?;
    let before = file.metadata().map_err(|_| fail())?;
    if !before.is_file() {
        return Err(fail());
    }
    if let Some(dependencies) = dependencies {
        use crate::config::dependencies::{Fingerprint, PathFingerprint};
        let resolved = fs::canonicalize(&path.value).map_err(|_| fail())?;
        if dependencies.paths.get(&resolved)
            != Some(&PathFingerprint::Present(Fingerprint::from_metadata(
                &before,
            )))
        {
            return Err(ConfigError::new(
                "RESOURCE_CHANGED",
                item,
                "PEM resource changed before reading",
                &path.origin,
            ));
        }
        dependencies.record_consumed(&path.value, &before, &path.origin)?;
    }
    let mut bytes = Vec::new();
    (&file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| fail())?;
    if bytes.len() as u64 > limit {
        return Err(ConfigError::new(
            "CONFIG_LIMIT",
            item,
            "PEM resource exceeds size limit",
            &path.origin,
        ));
    }
    let unchanged = |after: &std::fs::Metadata| {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    };
    if !unchanged(&file.metadata().map_err(|_| fail())?)
        || !unchanged(&fs::metadata(&path.value).map_err(|_| fail())?)
    {
        return Err(ConfigError::new(
            "RESOURCE_CHANGED",
            item,
            "PEM resource changed while reading",
            &path.origin,
        ));
    }
    Ok(bytes)
}

fn chain(bytes: &[u8], origin: &Origin) -> Result<Vec<X509>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid("cert", "certificate chain must be PEM", origin))?;
    let mut text = text.trim();
    let mut chain = Vec::new();
    while !text.is_empty() {
        if !text.starts_with("-----BEGIN CERTIFICATE-----") {
            return Err(invalid(
                "cert",
                "certificate file may contain only PEM certificates",
                origin,
            ));
        }
        let end = text
            .find("-----END CERTIFICATE-----")
            .ok_or_else(|| invalid("cert", "unterminated PEM certificate", origin))?
            + "-----END CERTIFICATE-----".len();
        chain.push(
            X509::from_pem(&text.as_bytes()[..end])
                .map_err(|_| invalid("cert", "cannot parse PEM certificate", origin))?,
        );
        if chain.len() > 16 {
            return Err(invalid(
                "cert",
                "certificate chain exceeds 16 entries",
                origin,
            ));
        }
        text = text[end..].trim();
    }
    if chain.is_empty() {
        return Err(invalid("cert", "certificate chain is empty", origin));
    }
    Ok(chain)
}

fn covers(pattern: &str, domain: &str) -> bool {
    if domain.starts_with("*.") {
        return pattern == domain;
    }
    pattern == domain
        || pattern.strip_prefix("*.").is_some_and(|suffix| {
            domain
                .split_once('.')
                .is_some_and(|(label, rest)| !label.is_empty() && suffix == rest)
        })
}

impl PreparedTls {
    pub(crate) fn certificate(&self, server_name: &str) -> Option<&Arc<Certificate>> {
        self.bindings.get(server_name).or_else(|| {
            let (_, suffix) = server_name.split_once('.')?;
            self.bindings.get(&format!("*.{suffix}"))
        })
    }

    pub fn prepare(configuration: &ValidatedConfig) -> Result<Self> {
        Self::prepare_inner(configuration, None)
    }

    pub fn prepare_tracked(
        configuration: &ValidatedConfig,
        dependencies: &mut crate::config::dependencies::DependencyPlan,
    ) -> Result<Self> {
        Self::prepare_inner(configuration, Some(dependencies))
    }

    fn prepare_inner(
        configuration: &ValidatedConfig,
        mut dependencies: Option<&mut crate::config::dependencies::DependencyPlan>,
    ) -> Result<Self> {
        let mut certificates = BTreeMap::new();
        for definition in configuration.certificates.values() {
            let fail = |reason| invalid("cert", reason, &definition.cert.origin);
            let mut parsed = chain(
                &read(
                    &definition.cert,
                    1024 * 1024,
                    "cert",
                    dependencies.as_deref_mut(),
                )?,
                &definition.cert.origin,
            )?;
            let now = Asn1Time::days_from_now(0)
                .map_err(|_| fail("cannot determine certificate verification time"))?;
            for certificate in &parsed {
                if certificate
                    .not_before()
                    .compare(&now)
                    .map_err(|_| fail("invalid certificate validity time"))?
                    == Ordering::Greater
                    || certificate
                        .not_after()
                        .compare(&now)
                        .map_err(|_| fail("invalid certificate validity time"))?
                        != Ordering::Greater
                {
                    return Err(fail("certificate chain is expired or not yet valid"));
                }
            }
            for pair in parsed.windows(2) {
                let issuer = pair[1]
                    .public_key()
                    .map_err(|_| fail("invalid issuer public key"))?;
                if pair[0]
                    .issuer_name()
                    .to_der()
                    .map_err(|_| fail("invalid issuer name"))?
                    != pair[1]
                        .subject_name()
                        .to_der()
                        .map_err(|_| fail("invalid subject name"))?
                    || !pair[0]
                        .verify(&issuer)
                        .map_err(|_| fail("invalid certificate chain signature"))?
                {
                    return Err(fail(
                        "certificate chain is unordered or has invalid signatures",
                    ));
                }
            }
            let leaf = parsed.remove(0);
            let bytes = read(
                &definition.key,
                64 * 1024,
                "key",
                dependencies.as_deref_mut(),
            )?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| invalid("key", "private key must be PEM", &definition.key.origin))?
                .trim();
            let label = [
                "PRIVATE KEY",
                "RSA PRIVATE KEY",
                "EC PRIVATE KEY",
                "DSA PRIVATE KEY",
            ]
            .into_iter()
            .find(|label| text.starts_with(&format!("-----BEGIN {label}-----")))
            .ok_or_else(|| {
                invalid(
                    "key",
                    "expected one unencrypted PEM private key",
                    &definition.key.origin,
                )
            })?;
            let end = format!("-----END {label}-----");
            if !text.ends_with(&end) || text.find(&end) != Some(text.len() - end.len()) {
                return Err(invalid(
                    "key",
                    "private key file must contain exactly one PEM key",
                    &definition.key.origin,
                ));
            }
            let key = PKey::private_key_from_pem_callback(&bytes, |_| Err(ErrorStack::get()))
                .map_err(|_| {
                    invalid(
                        "key",
                        "cannot read an unencrypted PEM private key",
                        &definition.key.origin,
                    )
                })?;
            if !leaf
                .public_key()
                .map_err(|_| fail("invalid leaf public key"))?
                .public_eq(&key)
            {
                return Err(invalid(
                    "key",
                    "certificate and private key do not match",
                    &definition.key.origin,
                )
                .related(&definition.cert.origin));
            }
            let mut store = X509StoreBuilder::new()
                .map_err(|_| fail("cannot construct certificate verification store"))?;
            store
                .add_cert(parsed.last().unwrap_or(&leaf).clone())
                .map_err(|_| fail("cannot prepare certificate chain anchor"))?;
            store
                .set_flags(X509VerifyFlags::PARTIAL_CHAIN)
                .map_err(|_| fail("cannot set certificate verification policy"))?;
            store
                .set_purpose(X509PurposeId::SSL_SERVER)
                .map_err(|_| fail("cannot set certificate purpose"))?;
            let mut intermediates =
                Stack::new().map_err(|_| fail("cannot prepare certificate intermediates"))?;
            for certificate in &parsed {
                intermediates
                    .push(certificate.clone())
                    .map_err(|_| fail("cannot prepare certificate intermediates"))?;
            }
            let verified = X509StoreContext::new()
                .map_err(|_| fail("cannot construct certificate verification context"))?
                .init(&store.build(), &leaf, &intermediates, |context| {
                    context.verify_cert()
                })
                .map_err(|_| fail("cannot verify certificate chain"))?;
            if !verified {
                return Err(fail(
                    "certificate chain or server authentication purpose is invalid",
                ));
            }
            let mut names = Vec::new();
            for name in leaf.subject_alt_names().into_iter().flatten() {
                if let Some(name) = name.dnsname() {
                    names.push(
                        normalize_domain(name, true)
                            .filter(|normalized| normalized.eq_ignore_ascii_case(name))
                            .ok_or_else(|| {
                                fail("certificate contains an invalid DNS subjectAltName")
                            })?,
                    );
                }
            }
            if names.is_empty() {
                return Err(fail(
                    "certificate requires a valid DNS subjectAltName; CN fallback is not supported",
                ));
            }
            let mut context = builder().map_err(|_| fail("cannot build TLS context"))?;
            context
                .set_certificate(&leaf)
                .map_err(|_| fail("certificate is incompatible with TLS context"))?;
            context.set_private_key(&key).map_err(|_| {
                invalid(
                    "key",
                    "private key is incompatible with TLS context",
                    &definition.key.origin,
                )
            })?;
            for certificate in &parsed {
                context
                    .add_extra_chain_cert(certificate.clone())
                    .map_err(|_| fail("cannot build TLS certificate chain"))?;
            }
            context.check_private_key().map_err(|_| {
                invalid(
                    "key",
                    "TLS certificate and key check failed",
                    &definition.key.origin,
                )
            })?;
            certificates.insert(
                definition.name.value.clone(),
                Arc::new(Certificate {
                    leaf,
                    chain: parsed,
                    key,
                    names,
                    _context: context.build(),
                }),
            );
        }
        let mut bindings = BTreeMap::new();
        for site in &configuration.sites {
            if let Some(tls) = &site.tls {
                for domain in &site.domains {
                    let reference = tls.reference(&domain.value).ok_or_else(|| {
                        invalid(
                            "domains",
                            "domain has no certificate binding",
                            &domain.origin,
                        )
                    })?;
                    let certificate = certificates.get(&reference.value).ok_or_else(|| {
                        invalid(
                            "tls",
                            "certificate reference does not exist",
                            &reference.origin,
                        )
                    })?;
                    if !certificate
                        .names
                        .iter()
                        .any(|pattern| covers(pattern, &domain.value))
                    {
                        return Err(invalid(
                            "tls",
                            "certificate SAN does not cover the entire site domain",
                            &reference.origin,
                        )
                        .related(&domain.origin));
                    }
                    bindings.insert(domain.value.clone(), certificate.clone());
                }
            }
        }
        Ok(Self { bindings })
    }
}
