//! The client certificate/key pair, kept as PEMs next to the document.
//!
//! Outside `settings.json` on purpose: a document that fails to parse can fall back to defaults
//! without silently discarding the identity every host has pinned.
use anyhow::{Context, Result};

/// Loads the identity, generating and writing one on first run — or when the pair on disk
/// cannot be used at all.
pub fn load_or_create_identity() -> Result<(String, String)> {
    let dir = super::app_dir();
    let (cert_path, key_path) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(cert), Ok(key)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
        if usable(&cert, &key) {
            return Ok((cert, key));
        }
        // Existing but unusable — a first-run write torn by a power cut, say. Kept as it was,
        // every mTLS request and every connect would fail for good, with nothing to clear it.
        tracing::warn!("client identity on disk does not parse — generating a new one; hosts will need pairing again");
    }
    let (cert, key) = punktfunk_core::quic::endpoint::generate_identity().context("generate_identity")?;
    // Write-then-rename, as the document is: a cut mid-write must not leave a half PEM.
    crate::services::atomic::write(&cert_path, &cert, "client-cert.pem")?;
    crate::services::atomic::write(&key_path, &key, "client-key.pem")?;
    Ok((cert, key))
}

/// Whether the pair parses the way every consumer (the mTLS agent, the QUIC endpoint) reads it.
/// Parsing only — nothing here can tell a valid pair this host never pinned from one it did.
fn usable(cert: &str, key: &str) -> bool {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    CertificateDer::from_pem_slice(cert.as_bytes()).is_ok() && PrivateKeyDer::from_pem_slice(key.as_bytes()).is_ok()
}
