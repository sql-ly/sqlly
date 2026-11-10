//! Which root certificates a verifying Redis TLS connection trusts.
//!
//! By default (Verify CA / Verify full, no CA file) the driver trusts the
//! bundled public roots only — never the machine's own store, so a profile
//! verifies the same way on every machine. A CA file is trusted alongside
//! the bundled roots (as the profile field promises, and as the PostgreSQL
//! driver does), so a private CA verifies without cutting off servers with
//! public certificates. "Also trust the OS keychain roots" adds the
//! operating system's trust store (the macOS keychain, the Windows root
//! store, the Linux CA bundle), so a company CA that IT installed on the
//! machine verifies without a CA file; with a CA file as well, all three
//! are trusted. If the store cannot be read or is empty, the connection
//! keeps the bundled roots (and any CA file) rather than trusting nothing —
//! the option never loosens verification.
//!
//! The client library takes one PEM root set that replaces its bundled
//! roots, so whenever anything is added the bundled roots are written into
//! that set too (from the same Mozilla list, as certificates).

use std::sync::OnceLock;

use base64::Engine as _;
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use sqlly_core::{ConnectionProfile, StudioError, StudioResult};

/// The OS trust store, read once per process (reading the macOS keychain
/// takes a noticeable moment).
fn os_roots() -> &'static [CertificateDer<'static>] {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let loaded = rustls_native_certs::load_native_certs();
        for error in &loaded.errors {
            tracing::debug!(%error, "skipped part of the OS trust store");
        }
        loaded.certs
    })
}

fn pem_of(cert: &CertificateDer<'_>) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(cert.as_ref());
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// What the profile trusts, for the log and the test trace.
pub(crate) fn trust_label(profile: &ConnectionProfile, os_used: bool) -> String {
    match (&profile.tls_ca_file, os_used) {
        (Some(path), true) => format!(
            "the CA file {}, the bundled public roots and the OS trust store",
            path.display()
        ),
        (Some(path), false) => format!(
            "the CA file {} and the bundled public roots",
            path.display()
        ),
        (None, true) => "the bundled public roots and the OS trust store".to_string(),
        (None, false) => "the bundled public roots".to_string(),
    }
}

/// The PEM the driver hands the client library as its root set, or `None`
/// for the library's bundled public roots. When the profile adds a CA file
/// or the OS store, the set is that CA file, then the bundled roots, then
/// the OS store. The bool says whether the OS store is part of it.
pub(crate) fn root_material(profile: &ConnectionProfile) -> StudioResult<(Option<Vec<u8>>, bool)> {
    let ca = match &profile.tls_ca_file {
        Some(path) => Some(super::read_pem(path, "CA")?),
        None => None,
    };
    let os = if profile.tls_trust_os_roots {
        os_roots()
    } else {
        &[]
    };
    if ca.is_none() && os.is_empty() {
        return Ok((None, false));
    }
    let mut pem = ca.unwrap_or_default();
    if !pem.is_empty() && !pem.ends_with(b"\n") {
        pem.push(b'\n');
    }
    for cert in webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().chain(os) {
        pem.extend_from_slice(pem_of(cert).as_bytes());
    }
    Ok((Some(pem), !os.is_empty()))
}

/// The same trust set as a rustls root store (for the connection test's
/// TLS stage), with its label.
pub(crate) fn root_store(profile: &ConnectionProfile) -> StudioResult<(RootCertStore, String)> {
    let (material, os_used) = root_material(profile)?;
    let label = trust_label(profile, os_used);
    let mut roots = RootCertStore::empty();
    match material {
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(pem) => {
            for cert in CertificateDer::pem_slice_iter(&pem) {
                let cert = cert.map_err(|e| {
                    StudioError::InvalidProfile(format!("cannot parse {label}: {e}"))
                })?;
                // One unusable certificate in a large OS store must not
                // sink the rest; the CA file was parsed by the same rules.
                let _ = roots.add(cert);
            }
        }
    }
    Ok((roots, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "t",
            "h",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p
    }

    #[test]
    fn verify_full_without_options_uses_the_bundled_roots_only() {
        let p = profile();
        let (material, os) = root_material(&p).unwrap();
        assert!(material.is_none(), "the client library's bundled roots");
        assert!(!os);
        let (store, label) = root_store(&p).unwrap();
        assert_eq!(store.len(), webpki_roots::TLS_SERVER_ROOTS.len());
        assert_eq!(label, "the bundled public roots");
    }

    #[test]
    fn the_os_store_is_added_when_asked_and_its_pem_parses_back() {
        let mut p = profile();
        p.tls_trust_os_roots = true;
        let (material, os) = root_material(&p).unwrap();
        if os_roots().is_empty() {
            // A machine with no readable store keeps the bundled roots.
            assert!(material.is_none() && !os);
            return;
        }
        assert!(os);
        let pem = material.expect("PEM");
        let parsed = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let bundled = webpki_root_certs::TLS_SERVER_ROOT_CERTS.len();
        assert_eq!(parsed.len(), bundled + os_roots().len());
        assert_eq!(parsed[bundled].as_ref(), os_roots()[0].as_ref());
        let (store, label) = root_store(&p).unwrap();
        assert!(store.len() >= webpki_roots::TLS_SERVER_ROOTS.len());
        assert_eq!(label, "the bundled public roots and the OS trust store");
    }

    #[test]
    fn a_ca_file_and_the_os_store_are_both_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        // Any well-formed certificate stands in for a private CA.
        let sample = os_roots().first().map(pem_of).unwrap_or_else(|| {
            "-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n".into()
        });
        std::fs::write(&path, sample.trim_end()).unwrap();
        let mut p = profile();
        p.tls_ca_file = Some(path.clone());
        let bundled = webpki_root_certs::TLS_SERVER_ROOT_CERTS.len();
        let (material, os) = root_material(&p).unwrap();
        assert!(!os);
        let pem = String::from_utf8(material.unwrap()).unwrap();
        assert!(pem.starts_with(sample.trim_end()));
        assert_eq!(pem.matches("BEGIN CERTIFICATE").count(), 1 + bundled);
        assert!(trust_label(&p, false).contains("and the bundled public roots"));
        p.tls_trust_os_roots = true;
        let (material, os) = root_material(&p).unwrap();
        if os {
            let pem = String::from_utf8(material.unwrap()).unwrap();
            assert!(pem.starts_with(sample.trim_end()));
            assert_eq!(
                pem.matches("BEGIN CERTIFICATE").count(),
                1 + bundled + os_roots().len()
            );
            assert!(trust_label(&p, true).contains("and the OS trust store"));
        }
    }

    /// The profile field's promise: a CA file is trusted alongside the
    /// bundled roots, so a server with a public certificate still verifies
    /// once a private CA is added.
    #[test]
    fn a_ca_file_adds_to_the_bundled_roots_instead_of_replacing_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-ca.pem");
        let private = pem_of(&webpki_root_certs::TLS_SERVER_ROOT_CERTS[0]);
        std::fs::write(&path, &private).unwrap();
        let mut p = profile();
        p.tls_ca_file = Some(path);
        let (store, label) = root_store(&p).unwrap();
        // Every bundled root is still a trust anchor.
        let anchors: std::collections::HashSet<Vec<u8>> = store
            .roots
            .iter()
            .map(|a| a.subject.as_ref().to_vec())
            .collect();
        for bundled in webpki_roots::TLS_SERVER_ROOTS {
            assert!(
                anchors.contains(bundled.subject.as_ref()),
                "a bundled root went missing once a CA file was set"
            );
        }
        assert!(label.contains("the bundled public roots"), "{label}");
    }
}
