//! The client certificate/key pair, kept as PEMs next to the document.
//!
//! Outside `settings.json` on purpose: a document that fails to parse can fall back to defaults
//! without silently discarding the identity every host has pinned.
use std::path::Path;

use anyhow::{Context, Result};

/// Loads the identity, generating and writing one on first run, or when the pair on disk
/// cannot be used at all. A file that exists but can't be read is an error, and the pair
/// stays: a new one would need pairing again on every host.
pub fn load_or_create_identity() -> Result<(String, String)> {
    identity_in(&super::app_dir())
}

fn identity_in(dir: &Path) -> Result<(String, String)> {
    let (cert_path, key_path) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Some(cert), Some(key)) = (read_pem(&cert_path)?, read_pem(&key_path)?) {
        if usable(&cert, &key) {
            return Ok((cert, key));
        }
        // A first-run write torn by a power cut, say: it could never have connected.
        tracing::warn!("client identity on disk does not parse — generating a new one; hosts will need pairing again");
    }
    let (cert, key) = punktfunk_core::quic::endpoint::generate_identity().context("generate_identity")?;
    // Write-then-rename, as the document is: a cut mid-write must not leave a half PEM.
    crate::services::atomic::write(&cert_path, &cert, "client-cert.pem")?;
    crate::services::atomic::write(&key_path, &key, "client-key.pem")?;
    Ok((cert, key))
}

/// A PEM file's text, or `None` when it is missing or not text: a pair that can't connect.
fn read_pem(path: &Path) -> Result<Option<String>> {
    use std::io::ErrorKind::{InvalidData, NotFound};
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if matches!(e.kind(), NotFound | InvalidData) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Whether the pair parses the way every consumer (the mTLS agent, the QUIC endpoint) reads it.
/// Parsing only — nothing here can tell a valid pair this host never pinned from one it did.
fn usable(cert: &str, key: &str) -> bool {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    CertificateDer::from_pem_slice(cert.as_bytes()).is_ok() && PrivateKeyDer::from_pem_slice(key.as_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key that exists but can't be read is an error, and the cert beside it stays.
    #[test]
    fn an_unreadable_key_keeps_the_pair() {
        let dir = std::env::temp_dir().join(format!("pf-webos-identity-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("client-key.pem")).unwrap();
        std::fs::write(dir.join("client-cert.pem"), "cert").unwrap();
        assert!(identity_in(&dir).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("client-cert.pem")).unwrap(), "cert");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
