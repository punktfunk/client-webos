//! `ureq` transport plumbing for TLS against a self-signed peer.
//!
//! The host is authenticated by its certificate rather than by a CA chain (a SHA-256
//! fingerprint pinned at pairing time), and `ureq`'s own `TlsConfig` has no hook for installing
//! a custom `rustls` verifier. So the caller builds a `rustls::ClientConfig` itself and hands
//! it here.
//!
//! Modeled directly on ureq 3.x's own (crate-private) `RustlsConnector`
//! (`ureq` crate, `src/tls/rustls.rs`), minus its `TlsConfig`-driven `build_config` step.
use std::io::{Read as _, Write as _};
use std::sync::Arc;

use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport, TransportAdapter,
};

/// Wraps a chained (TCP) transport in TLS using a caller-supplied `rustls::ClientConfig`
/// verbatim.
pub struct PinnedTlsConnector {
    config: Arc<rustls::ClientConfig>,
}

impl PinnedTlsConnector {
    pub fn new(config: Arc<rustls::ClientConfig>) -> Self {
        Self { config }
    }
}

impl std::fmt::Debug for PinnedTlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsConnector").finish()
    }
}

impl<In: Transport> Connector<In> for PinnedTlsConnector {
    type Out = Either<In, PinnedTlsTransport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            panic!("PinnedTlsConnector requires a chained transport");
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }

        let host = details.uri.authority().expect("uri authority for tls").host();
        // An IPv6 literal comes back bracketed (`[2001:db8::5]`), which `ServerName` rejects;
        // ureq's own connector strips them the same way (`host_bare`).
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        let name: rustls::pki_types::ServerName<'_> =
            host.try_into().map_err(|_| ureq::Error::Tls("invalid DNS name"))?;
        let conn = rustls::ClientConnection::new(self.config.clone(), name.to_owned())?;
        let stream = rustls::StreamOwned {
            conn,
            sock: TransportAdapter::new(transport.boxed()),
        };
        let buffers = LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size());
        Ok(Some(Either::B(PinnedTlsTransport { buffers, stream })))
    }
}

pub struct PinnedTlsTransport {
    buffers: LazyBuffers,
    stream: rustls::StreamOwned<rustls::ClientConnection, TransportAdapter>,
}

impl std::fmt::Debug for PinnedTlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsTransport").finish()
    }
}

impl Transport for PinnedTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}
