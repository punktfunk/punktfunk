//! Shared protocol, transport, and FEC core for Punktfunk hosts and clients.
//!
//! Platform capture, encode, decode, presentation, and input injection live elsewhere.
//! This crate owns wire framing and reassembly ([`packet`]), erasure coding ([`fec`]),
//! encryption ([`crypto`]), host/client data-plane state ([`session`]), packet I/O
//! ([`transport`]), and shared configuration and event vocabularies. The C ABI over it is
//! the `punktfunk-ffi` crate.
//! The optional `quic` feature adds the native control plane, pairing, clock sync, adaptive
//! bitrate, clipboard transport, and the embeddable client worker.
//!
//! Per-frame processing never enters an async runtime; `tokio` and `quinn` are confined to
//! the optional control plane.

// `unsafe` is crate-denied. Parsers of network bytes stay safe Rust. Carve-outs are
// only `client` (`extern "C"`) and transport syscall shims that move caller-owned
// buffers (`udp/{apple,linux,windows}`, `qos_windows`) and the socket and interface
// readers (`ifinfo`, `sockstat`). A wire parser may not add a
// carve-out; SAFETY proofs sit next to each `unsafe`.
#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

/// cbindgen:ignore
pub mod abr;
pub mod audio;
#[cfg(feature = "quic")]
pub mod client;
/// Client-side shared-clipboard transport: the per-session task that runs the fetch-stream accept
/// loop, drives outbound fetches, and serves inbound ones — surfaced to the embedder as poll
/// events. Wire codecs live in [`quic`]; the OS pasteboard integration lives in the native client.
#[cfg(feature = "quic")]
pub mod clipboard;
pub mod config;
/// The unhandled-SEH filter every Windows process installs after logging init.
#[cfg(windows)]
#[path = "crash_windows.rs"]
pub mod crash;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod fec;
/// cbindgen:ignore
pub mod fp;
// The stats overlay every client draws: window, snapshot, formatter. `punktfunk-ffi` exports it.
/// cbindgen:ignore
pub mod hud;
pub mod input;
pub mod packet;
pub mod phase;
pub mod quic;
pub mod reanchor;
pub mod reject;
pub mod render_scale;
/// cbindgen:ignore
pub mod resolutions;
/// The rumble policy every client runs. Outside `client` because the browser, which has no
/// quinn, runs it too.
pub mod rumble;
pub mod session;
pub mod stats;
/// cbindgen:ignore
pub mod time;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;
// Placement every client twins by hand; the C header stays out of it.
/// cbindgen:ignore
pub mod video_fit;
pub mod wol;

pub use config::{CompositorPref, Config, FecConfig, FecScheme, Mode, Role};
pub use error::{PunktfunkError, PunktfunkStatus, Result};
pub use session::{Frame, Session};
pub use stats::Stats;

/// C-ABI generation. Mirrors `punktfunk_abi_version()`; embedders abort on mismatch.
///
/// Bump on any breaking change to the C ABI (`punktfunk-ffi`). Additive bumps add
/// symbols and leave every existing function's signature and behaviour alone.
/// New connect options append to `PunktfunkConnectOpts` behind `struct_size`;
/// do not mint another `connect_ex*` or grow `PunktfunkAudioPcm` / `PunktfunkStats`
/// (no size guard, allocated by value). Growing a struct the library writes whole into
/// the caller's buffer is a bump: the version check is the overrun guard
/// (`PunktfunkHidOutput` at 27, `PunktfunkProbeResult` at 43).
///
/// The wire is versioned by ALPN, not by this. Pin the integer in `punktfunk-ffi`
/// (`abi_version_is_pinned`). Per-bump notes live in `CHANGELOG.md`.
pub const ABI_VERSION: u32 = 47;
