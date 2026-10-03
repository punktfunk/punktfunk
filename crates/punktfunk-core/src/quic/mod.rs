//! `punktfunk/1` — the control plane's wire vocabulary, and the quinn transport that carries it.
//!
//! **The messages are not behind the `quic` feature; the transport is.** A browser speaks the
//! same protocol over WebTransport, where quinn cannot go, so only [`endpoint`], [`io`],
//! [`clipstream`] and [`pake`] need a feature.
//!
//! One QUIC bidirectional stream (quinn, tokio — control only, never the
//! per-frame path) carries a length-prefixed handshake:
//!
//! ```text
//!   client → host  Hello   { abi_version }
//!   host → client  Welcome { abi_version, session: Config + mode + UDP port }
//!   client → host  Start   { client_udp_port }
//! ```
//!
//! Both sides then open a [`crate::session::Session`] over
//! [`UdpTransport`](crate::transport::udp) (native threads). Welcome carries
//! the negotiated data-plane config (FEC, shard size, key/salt). The host
//! presents a long-lived self-signed cert; the client pins its SHA-256
//! fingerprint (no pin = TOFU). Data-plane AES-GCM sits on top. Integers
//! little-endian; every message is `u16 length || payload`.

/// Protocol magic + version; first bytes of Hello/Welcome/Start.
pub const MAGIC: &[u8; 4] = b"PKF1";

/// Magic for typed post-handshake / pairing messages. Distinct from [`MAGIC`] so a
/// `Hello` (abi_version where a type byte would sit) cannot parse as control, and
/// vice versa.
pub const CTL_MAGIC: &[u8; 4] = b"PKFc";

mod access;
mod caps;
mod clock;
mod control;
mod datagram;
mod handshake;
mod pairing;
mod pen;
mod wire;

/// The `punktfunk/2` codecs (ALPN `pkf2`). Ungated like the v1 vocabulary: the browser speaks it too.
/// cbindgen:ignore
pub mod v2;

/// quinn endpoint constructors: host identity ([`endpoint::server_with_identity`]),
/// client pin / TOFU ([`endpoint::client_pinned`]).
#[cfg(feature = "quic")]
pub mod endpoint;

#[cfg(feature = "quic")]
pub mod io;

/// Per-transfer clipboard fetch streams (`PKFs` + kind, then request/response).
/// Transport only; wire codecs in [`control`], state per side.
#[cfg(feature = "quic")]
pub mod clipstream;

/// SPAKE2 over Ed25519 for pairing. Both certificate fingerprints are the SPAKE2
/// identities, so a MITM that presents different certs on each leg cannot share a key.
/// Its own feature, so a client that pairs over a different transport can have the ceremony
/// without quinn.
#[cfg(feature = "pake")]
pub mod pake;

pub use access::*;
pub use caps::*;
pub use clock::*;
pub use control::*;
pub use datagram::*;
pub use handshake::*;
pub use pairing::*;
pub use pen::*;

// Close codes + [`RejectReason`] live in `crate::reject` (ungated: the error enum
// names them even without `quic`) and re-export here next to QUIT/APP_EXITED.
pub use crate::reject::*;

// `quic` as well as `test`: this hands out `quinn` endpoints, and `endpoint` above is
// feature-gated. Gated on `test` alone it broke `cargo test -p punktfunk-core`, which
// resolves default features and has no quinn.
#[cfg(all(test, feature = "quic"))]
pub(crate) mod test_util;
