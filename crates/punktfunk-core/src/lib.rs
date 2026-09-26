//! Shared protocol, transport, FEC, and C-ABI core for Punktfunk hosts and clients.
//!
//! Platform capture, encode, decode, presentation, and input injection live elsewhere.
//! This crate owns wire framing and reassembly ([`packet`]), erasure coding ([`fec`]),
//! encryption ([`crypto`]), host/client data-plane state ([`session`]), packet I/O
//! ([`transport`]), shared configuration and event vocabularies, and the [C ABI](crate::abi).
//! The optional `quic` feature adds the native control plane, pairing, clock sync, adaptive
//! bitrate, clipboard transport, and the embeddable client worker.
//!
//! Per-frame processing never enters an async runtime; `tokio` and `quinn` are confined to
//! the optional control plane.

// `unsafe` is crate-denied. Parsers of network bytes stay safe Rust. Carve-outs are
// only `abi`/`client` (`extern "C"`) and transport syscall shims that move caller-owned
// buffers (`udp/{apple,linux,windows}`, `qos_windows`). A wire parser may not add a
// carve-out; SAFETY proofs sit next to each `unsafe` (mostly `abi.rs`).
#![deny(unsafe_code)]
#![forbid(unsafe_op_in_unsafe_fn)]

// For the cdylib/staticlib embedders — Swift, Kotlin, C — and no browser is one. Cargo builds
// the cdylib on wasm regardless, and these `#[no_mangle]` roots are what make it fail there:
// wasm-ld exports the mangled symbols they reach, and emcc rejects the first name that is not a
// JS identifier.
#[cfg(not(target_family = "wasm"))]
pub mod abi;
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
/// cbindgen:ignore
#[cfg(feature = "quic")]
pub mod demo_host;
pub mod discovery;
pub mod error;
pub mod fec;
// The stats overlay every client draws: window, snapshot, formatter. The ABI exports it from `abi`.
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
pub mod session;
pub mod stats;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;
// Placement every client twins by hand; the C header stays out of it.
/// cbindgen:ignore
pub mod video_fit;
pub mod wol;

pub use config::{CompositorPref, Config, FecConfig, FecScheme, Mode, ProtocolPhase, Role};
pub use error::{PunktfunkError, PunktfunkStatus, Result};
pub use session::{Frame, Session};
pub use stats::Stats;

/// C-ABI generation. Mirrors `punktfunk_abi_version()`; embedders abort on mismatch.
///
/// Bump on any breaking change to the [C ABI](crate::abi). Additive bumps add
/// symbols and leave every existing function's signature and behaviour alone.
/// New connect options append to [`abi::PunktfunkConnectOpts`] behind `struct_size`;
/// do not mint another `connect_ex*` or grow `PunktfunkAudioPcm` / `PunktfunkStats`
/// (no size guard, allocated by value). v27 is the exception: `PunktfunkHidOutput`
/// grew 19 → 85 bytes and the version check is the overrun guard — a second pull
/// symbol would fork the hidout drain forever.
///
/// Not [`WIRE_VERSION`]. The C surface can grow without a wire byte changing.
/// Pin the integer in `abi.rs` (`abi_version_is_pinned`). Per-bump notes live
/// in `CHANGELOG.md`.
pub const ABI_VERSION: u32 = 39;

/// punktfunk/1 wire version. `Hello`/`Welcome` carry it; hosts equality-check it.
///
/// Separate from [`ABI_VERSION`]: the C surface can grow without a wire byte changing.
/// Bump only when the handshake or a plane changes incompatibly. Riding a C-only bump
/// onto the wire locks new clients out of every deployed host.
pub const WIRE_VERSION: u32 = 2;
