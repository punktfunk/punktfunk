//! The IDD-push display manager's touch points for a session: the connector-slot guard held
//! across `create`, the topology re-assert generation the stream loop follows, and the
//! in-driver encoder. `main.rs` carries the no-op twin for every other OS, so a session
//! path reads the same on both.

use anyhow::{Context, Result};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const REJECTED_SLOT: &str = "pf-vdisplay refused this process's connector slot; see the log for the PUNKTFUNK_SEAT_DISPLAY_SLOT or reservation problem";

/// Reserve this client's connector slot before `create`, so a prior session on the same slot
/// is the only one preempted. A rejected slot means this host would drive a connector that
/// belongs to another one; refusing the session beats attaching to someone else's display.
/// `None` for every other capture backend. GameStream passes no identity and takes the
/// anonymous slot, which under a seats reservation resolves to this host's own connector.
pub(crate) fn setup_guard(
    capture: crate::session_plan::CaptureBackend,
    identity: Option<[u8; 32]>,
    size: (u32, u32),
    stop: &Arc<AtomicBool>,
) -> Result<Option<std::sync::MutexGuard<'static, ()>>> {
    if capture != crate::session_plan::CaptureBackend::IddPush {
        return Ok(None);
    }
    let slot = crate::vdisplay::manager::slot_id_for(identity, size).context(REJECTED_SLOT)?;
    Ok(Some(
        crate::vdisplay::manager::vdm().begin_idd_setup(slot, stop.clone()),
    ))
}

/// The v5 IddCx hardware-cursor channel is up: the client may draw the pointer itself.
pub(crate) fn hw_cursor_capable() -> bool {
    crate::vdisplay::manager::hw_cursor_capable()
}

/// The manager's topology re-assert generation. A stream loop that sees it move re-attaches.
pub(crate) fn topology_reassert_gen() -> u64 {
    crate::vdisplay::manager::topology_reassert_gen()
}

/// The in-driver encoder for an IDD-push session ([`crate::capture::open_driver_encoder`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_driver_encoder(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn crate::capture::Capturer,
    size: (u32, u32),
    fps: u32,
    bitrate_bps: u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Box<dyn crate::encode::Encoder>> {
    crate::capture::open_driver_encoder(
        plan,
        capturer,
        size,
        fps,
        bitrate_bps,
        bit_depth,
        client_hdr,
        wire_seq_base,
    )
}
