//! The host's capture sources: pixel sources the stream loop encodes, and remote-encoder
//! sources whose encoder runs in another process. Contract: `docs/adr/0001`.
//!
//! Roles: a source is a [`Capturer`] plus its roles. The host passes facts, not capture choices.
//! Model and shell: a model (enum state, `step`, table test) calls no OS; its shell does that.
//! Knobs: every `PUNKTFUNK_*` read is in `knobs.rs`; a few arrive resolved from other crates.
//!
//! - `capturer.rs`: the contract, its roles and events.
//! - `open.rs`: the request structs and the `open_*` entry points.
//! - `knobs.rs`: `Knobs`, every switch the crate reads, with its parser.
//! - `synthetic.rs`: sources with no display behind them.
//! - `fake.rs`: the stream loops' scripted source, behind `test-support`.
//! - `pts_provenance.rs`: which clock a Linux frame's wire stamp comes from.
//! - `pwinit.rs`: PipeWire's one-time init, shared with audio capture.
//! - `linux/`: the cells a capture shares with its thread, one buffer-to-frame delivery.
//! - `linux/pipewire/`: the PipeWire stream, from the portal monitor or a virtual output.
//! - `linux/direct/`: `ext-image-copy-capture-v1` into dmabufs this side allocates.
//! - `windows/`: the pure models every target tests, WGC, the GPU-preference hook.
//! - `windows/remote/`: one source for every remote encoder: transport, AU proxy, supervisor.
//! - `windows/idd/`: IDD-push, where the pf-vdisplay driver encodes.

// The frame vocabulary, at the crate root for every module's `crate::` imports.
#[cfg(any(target_os = "linux", target_os = "windows"))]
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

mod capturer;
#[cfg(any(test, feature = "test-support"))]
mod fake;
mod knobs;
mod open;
mod synthetic;
pub use capturer::*;
#[cfg(any(test, feature = "test-support"))]
pub use fake::{FakeCapturer, FakeCursor, FakeFeed, FakeSupervised};
#[cfg(target_os = "linux")]
pub use knobs::direct_capture;
pub use open::*;
pub use synthetic::{FastSyntheticCapturer, SyntheticCapturer};

// The per-source HDR latch lives in pf-frame so pf-vdisplay reads it without this crate.
#[cfg(target_os = "linux")]
pub use pf_frame::hdr::{
    clear_virtual_output_hdr_latch, hdr_capture_failed, note_hdr_capture_failed, HdrSource,
};

// One-time PipeWire library init, shared by video (portal) and audio capture.
#[cfg(target_os = "linux")]
pub mod pwinit;

// Which clock the wire's `pts_ns` comes from. Linux-only consumer; arithmetic
// is platform-independent so tests run everywhere.
#[cfg(any(target_os = "linux", test))]
mod pts_provenance;

mod windows;
#[cfg(target_os = "windows")]
pub use self::windows::remote::encoder::DriverEncodeOpenError;
#[cfg(target_os = "windows")]
pub use self::windows::wgc::{open_wgc, WgcSource, WorkerProcess};
#[cfg(target_os = "windows")]
pub use self::windows::{gpu_pref, synthetic_nv12};
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
// The direct capture's frame-loop model is pure, so its table runs on every target.
#[cfg(all(test, not(target_os = "linux")))]
#[path = "linux/direct/frame_loop.rs"]
mod direct_frame_loop;
// GNOME BT.2100 colour-mode probe — host gate for offering HDR on the portal
// monitor path (`open_portal_monitor` `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
