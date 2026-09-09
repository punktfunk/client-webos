//! Game-library fetch from the host's management REST API: `GET
//! https://<host>:<mgmt_port>/api/v1/library`, mTLS-authenticated by this device's
//! paired identity (no bearer token — the host authorizes by client certificate).
//! A trimmed port of `pf-client-core::library` (same wire shape, same mTLS pinning
//! verifier) rather than a dependency on that crate — see `session.rs`'s module docs
//! for why this client doesn't pull in `pf-client-core` at all.
use std::sync::Arc;

use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Connector as _, TcpConnector};

use crate::services::pinned_tls::PinnedTlsConnector;

pub use crate::core::model::GameEntry;

/// The management API's default port — matches the host's `mgmt::DEFAULT_PORT`. A
/// discovered host may advertise a different one via its mDNS `mgmt` TXT record
/// (`discovery::DiscoveredHost::mgmt_port`); saved-but-not-advertising hosts (or an
/// older host with no mgmt TXT at all) fall back here.
pub const DEFAULT_MGMT_PORT: u16 = 47990;

/// Errors surfaced to the UI so it can explain what to do next.
#[derive(Debug)]
pub enum LibraryError {
    /// The host rejected our certificate — this device isn't on its paired list.
    NotPaired,
    /// The host's certificate didn't hash to the pinned fingerprint.
    PinMismatch,
    Http(u16),
    Unreachable(String),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPaired => f.write_str("Not paired — pair with the host first."),
            Self::PinMismatch => f.write_str("Host certificate changed — re-pair with a PIN."),
            Self::Http(code) => write!(f, "Management API returned HTTP {code}."),
            Self::Unreachable(why) => write!(f, "Couldn't reach the host's management API: {why}."),
        }
    }
}

/// `https://addr:port`, IPv6 literals bracketed.
pub(crate) fn base_url(addr: &str, mgmt_port: u16) -> String {
    if addr.contains(':') {
        format!("https://[{addr}]:{mgmt_port}")
    } else {
        format!("https://{addr}:{mgmt_port}")
    }
}

/// Builds mTLS `ureq::Agent` reusable across requests (avoids repeated TLS handshakes).
/// Exposed for art.rs to build once outside its per-game loop.
pub fn agent(identity: &(String, String), pin: Option<[u8; 32]>) -> Result<ureq::Agent, LibraryError> {
    agent_within(identity, pin, crate::services::budget::REQUEST)
}

/// [`agent`] with an explicit whole-request budget, for the one caller that cannot wait the
/// default: the exit action, which the process blocks on while quitting.
pub fn agent_within(
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    budget: std::time::Duration,
) -> Result<ureq::Agent, LibraryError> {
    use rustls::pki_types::pem::PemObject;
    let bad = |what: &str, e: &dyn std::fmt::Display| LibraryError::Unreachable(format!("{what}: {e}"));
    // aws-lc-rs, matching punktfunk-core's QUIC for consistent crypto — the invariant this
    // comment always claimed, now that core has moved off ring too. Naming a provider here
    // (rather than letting rustls infer one) is also what keeps this path working if a second
    // backend ever re-enters the tree: with both compiled, rustls declines to guess and the
    // inferring constructors panic. `builder_with_provider` never has to guess.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("tls config", &e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerify { pin }));
    let cert = rustls::pki_types::CertificateDer::from_pem_slice(identity.0.as_bytes())
        .map_err(|e| bad("client cert pem", &e))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(identity.1.as_bytes())
        .map_err(|e| bad("client key pem", &e))?;
    let cfg = builder
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| bad("client auth", &e))?;

    // WHY: ureq's TlsConfig doesn't hook custom fingerprint pinning, so the config above
    // (with `PinVerify` installed) goes through `services::pinned_tls` instead.
    let connector = TcpConnector::default().chain(PinnedTlsConnector::new(Arc::new(cfg)));
    let config = ureq::Agent::config_builder()
        // Connect is capped by the whole budget too: a 5 s connect inside a 200 ms request is
        // just a slower way to hit the same wall.
        .timeout_connect(Some(budget.min(crate::services::budget::PROBE)))
        .timeout_global(Some(budget))
        .build();
    Ok(ureq::Agent::with_parts(config, connector, DefaultResolver::default()))
}

/// One JSON GET on the management lane under an explicit whole-request `budget`, errors
/// pre-classified for the UI (401/403→NotPaired, a pin mismatch→PinMismatch, everything else
/// by status). Every mgmt read goes through here, so the classification and the "couldn't
/// read/parse" wording are stated once.
pub(crate) fn get_json<T: serde::de::DeserializeOwned>(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    path: &str,
    budget: std::time::Duration,
) -> Result<T, LibraryError> {
    let agent = agent_within(identity, pin, budget)?;
    let url = format!("{}{path}", base_url(addr, mgmt_port));
    let body = match agent.get(url.as_str()).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// Fetch the host's library.
pub(crate) fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    budget: std::time::Duration,
) -> Result<Vec<GameEntry>, LibraryError> {
    get_json(addr, mgmt_port, identity, pin, "/api/v1/library", budget)
}

/// One `fetch_games` result with `host/port/mgmt_port` (so `drain_games` can start art loading).
pub struct GamesLoaded {
    pub host: String,
    pub port: u16,
    pub mgmt_port: u16,
    pub result: Result<Vec<GameEntry>, LibraryError>,
}

/// Spawns background thread to run `fetch_games` under `budget` (avoids UI freeze from network
/// blocking). Safe to switch hosts before finish: receiver drop causes thread's send to fail.
pub fn load_games_async(
    host: String,
    port: u16,
    mgmt_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
    budget: std::time::Duration,
) -> std::sync::mpsc::Receiver<GamesLoaded> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("punktfunk-webos-library".into())
        .spawn(move || {
            let result = fetch_games(&host, mgmt_port, &identity, fingerprint, budget);
            let _ = tx.send(GamesLoaded {
                host,
                port,
                mgmt_port,
                result,
            });
        })
        .expect("spawn library-fetch thread");
    rx
}

/// Fetches one piece of cover art's raw bytes (JPEG/PNG, undecoded) from a
/// host-relative `art_path` (one of `GameEntry::art`'s fields), reusing an
/// already-built `agent` (see `fetch_games`) to avoid a fresh mTLS handshake per
/// cover. Decoding happens in `art.rs`, off this module's REST concern.
pub fn fetch_art(agent: &ureq::Agent, addr: &str, mgmt_port: u16, art_path: &str) -> Result<Vec<u8>, LibraryError> {
    // Some hosts hand back a full external URL (e.g. a SteamGridDB CDN link) instead
    // of a host-relative path — that can't go through the pinned agent (wrong CA,
    // and prefixing it with base_url would double up the authority).
    if art_path.starts_with("http://") || art_path.starts_with("https://") {
        return fetch_external_art(art_path);
    }
    let url = format!("{}{art_path}", base_url(addr, mgmt_port));
    match agent.get(url.as_str()).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| LibraryError::Unreachable(format!("read art body: {e}"))),
        Err(e) => Err(classify(e)),
    }
}

/// Fetches art from a full external URL with the system's default CA trust (no
/// client cert) — the host's pinned `agent` would reject this CA.
fn fetch_external_art(url: &str) -> Result<Vec<u8>, LibraryError> {
    let agent = ureq::Agent::new_with_defaults();
    match agent.get(url).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| LibraryError::Unreachable(format!("read external art body: {e}"))),
        Err(e) => Err(classify(e)),
    }
}

pub(crate) fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::StatusCode(401 | 403) => LibraryError::NotPaired,
        ureq::Error::StatusCode(code) => LibraryError::Http(code),
        // The one rejection our own `PinVerify` (below) actually raises on a mismatch —
        // matched on the typed `rustls::Error` ureq 3.x's `Error::Rustls` now carries,
        // instead of the string-matching `Transport(t)` message-sniffing ureq 2.x forced.
        ureq::Error::Rustls(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        )) => LibraryError::PinMismatch,
        other => LibraryError::Unreachable(other.to_string()),
    }
}

/// Fingerprint-pinning verifier — trust is the SHA-256 of the host's self-signed leaf
/// cert (via `punktfunk_core::quic::endpoint::cert_fingerprint`, the same hash the
/// QUIC session pinning uses), not a CA chain. The handshake signatures are still
/// verified for real: skipping that would let an active MITM replay the host's
/// (public) certificate and complete the handshake with its own key.
#[derive(Debug)]
struct PinVerify {
    pin: Option<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = self.pin {
            let fp = punktfunk_core::quic::endpoint::cert_fingerprint(end_entity.as_ref());
            if fp != expected {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ));
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
