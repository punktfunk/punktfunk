//! A blocking [`ureq::Agent`] over a caller-supplied [`rustls::ClientConfig`].
//! ureq's `TlsConfig` exposes roots, a client cert, and an off-switch, but no
//! [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier) hook, so
//! [`PinVerify`](super::PinVerify) cannot use the default agent.
//!
//! The default agent validates against webpki roots, which a self-signed host
//! cert never satisfies. Handshake, verifier, and cipher suites live in the
//! `ClientConfig` the caller hands in; this module is transport glue.

use std::io::{Read as _, Write as _};
use std::sync::Arc;

use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, TcpConnector,
    Transport, TransportAdapter,
};

/// HTTPS via `tls` verbatim. Other knobs (timeouts, redirects, buffers) come
/// from `config`, built by the caller via [`ureq::Agent::config_builder`].
pub fn agent(tls: Arc<rustls::ClientConfig>, config: ureq::config::Config) -> ureq::Agent {
    let connector = TcpConnector::default().chain(PinnedTlsConnector { config: tls });
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

struct PinnedTlsConnector {
    config: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for PinnedTlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsConnector").finish()
    }
}

impl<In: Transport> Connector<In> for PinnedTlsConnector {
    type Out = Either<In, PinnedTlsTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            // Unreachable via `agent()` above, which always chains onto a TcpConnector.
            return Err(ureq::Error::Tls("no chained transport to wrap in TLS"));
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }

        let host = details
            .uri
            .authority()
            .ok_or(ureq::Error::Tls("uri has no authority"))?
            .host();
        // An IPv6 literal keeps its URI brackets in `host()`; a server name never has them.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let name: rustls::pki_types::ServerName<'_> = host
            .try_into()
            .map_err(|_| ureq::Error::Tls("invalid DNS name"))?;
        let conn = rustls::ClientConnection::new(self.config.clone(), name.to_owned())?;
        let stream = rustls::StreamOwned {
            conn,
            sock: TransportAdapter::new(transport.boxed()),
        };
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(PinnedTlsTransport { buffers, stream })))
    }
}

struct PinnedTlsTransport {
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
