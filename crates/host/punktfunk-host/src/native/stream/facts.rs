//! The parts of [`StreamState`](super::state::StreamState) that bring-up hands over once and no
//! rebuild replaces: the session's facts, the handles other threads read, and the loop's ends of
//! the control plane.

use super::*;
use crate::native::bitrate::EncoderCeiling;

/// What the session negotiated and was handed at bring-up. Nothing here changes while it runs.
pub(super) struct SessionFacts {
    /// The session's `seconds` cap: the loop ends here.
    pub(super) deadline: std::time::Instant,
    /// The handshake's encode resolution. Every rebuild opens on it, so an Encoder-row change
    /// applies at the next session.
    pub(super) encoder: Arc<crate::encode::Resolved>,
    /// The knobs read at admission; a console change applies at the next session.
    pub(super) knobs: crate::native::knobs::SessionKnobs,
    /// The flags every thread reads, and `end`, which the path that knows why calls first:
    /// a game exit, an operator stop, the peer's own close beat the loop's clean tail.
    pub(super) life: crate::lifecycle::Lifecycle,
    /// Session totals; this loop notes every encoder rate it adopts.
    pub(super) counters: Arc<crate::session_status::SessionCounters>,
    pub(super) conn: super::super::link::SessionLink,
    /// The client's ask. Encoders fit to it; the display may deliver another size.
    pub(super) negotiated: punktfunk_core::Mode,
    pub(super) bitrate_auto: bool,
    pub(super) audio_reserved_kbps: u32,
    pub(super) shard_payload: u16,
    /// PyroWave: parity rides each frame's head ([`EncDerive::head_only_hz`]), and an
    /// explicit-rate stream paces at the link.
    ///
    /// [`EncDerive::head_only_hz`]: crate::native::bitrate::EncDerive::head_only_hz
    pub(super) pyrowave: bool,
    pub(super) streamed_wire: bool,
    pub(super) perf: bool,
    pub(super) client_hdr: Option<pf_frame::HdrMeta>,
    pub(super) bringup: Arc<crate::bringup::Trace>,
    pub(super) stats: Arc<StatsRecorder>,
    /// What the handshake's depth, HDR and chroma rule read, for a codec change to re-run it.
    pub(super) format_facts: super::super::handshake::FormatFacts,
}

/// What the control task, the send and input threads, the clipboard and `/status` read while the
/// loop writes it, or the other way round.
pub(super) struct Shared {
    /// [`crate::native::wiring::SessionShared::acked`] and `lossy_window`.
    pub(super) acked: Arc<AtomicU64>,
    pub(super) lossy_window: Arc<AtomicBool>,
    /// [`crate::native::wiring::SessionShared::arrival_poke`]: the running capturer's, for
    /// the control task.
    pub(super) arrival_poke: crate::native::wiring::ArrivalPoke,
    /// The Windows driver's pool drops, handed to the send thread's recorder sample.
    pub(super) driver_dropped: Arc<AtomicU64>,
    pub(super) screen_wire: Arc<crate::native::wiring::ScreenWire>,
    pub(super) send_spread_us: Arc<AtomicU32>,
    /// [`super::recovery::KeyframeGate::new`]'s storm, read by the send thread's pacer.
    pub(super) idr_storm: Arc<AtomicBool>,
    pub(super) wire_rekeys: Arc<AtomicU32>,
    pub(super) live_mode: Arc<AtomicU64>,
    pub(super) force_idr: Arc<AtomicBool>,
    pub(super) capture_health: Arc<std::sync::Mutex<Option<pf_capture::CaptureHealth>>>,
    /// The live encoder's framing: forwarded cursor positions map through it, and so does
    /// the input thread's absolute input. Written on every encoder open.
    pub(super) frame_map: super::super::input::FrameMap,
    /// The session gamescope's X displays, for the clipboard. Written on every capture attach.
    #[cfg(target_os = "linux")]
    pub(super) gamescope_xwayland: pf_clipboard::GamescopeXwayland,
    pub(super) resize_ms: Arc<AtomicU32>,
    pub(super) live_bitrate: Arc<AtomicU32>,
    pub(super) encoder_ceiling: Arc<std::sync::Mutex<EncoderCeiling>>,
    pub(super) cadence_degraded: Arc<AtomicBool>,
    pub(super) cadence_behind_score: Arc<AtomicU32>,
    pub(super) client_packets_received: Arc<AtomicU32>,
    pub(super) cursor_client_draws: Arc<AtomicBool>,
}

/// The loop's ends of the control task's and the input thread's channels.
pub(super) struct LoopEnds {
    pub(super) cursor_shape_tx:
        tokio::sync::watch::Sender<Option<punktfunk_core::quic::CursorShape>>,
    #[cfg(target_os = "linux")]
    pub(super) input_tx: std::sync::mpsc::SyncSender<super::super::input::ClientInput>,
    #[cfg(target_os = "linux")]
    pub(super) input_route: super::super::input::InputRoute,
    #[cfg(target_os = "linux")]
    pub(super) inj_shared_tx: std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>,
    #[cfg(target_os = "linux")]
    pub(super) inj_session_tx: Option<std::sync::mpsc::Sender<punktfunk_core::input::InputEvent>>,
    pub(super) reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    pub(super) keyframe: std::sync::mpsc::Receiver<()>,
    pub(super) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    pub(super) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    /// Replaced when the loop starts its session watcher.
    pub(super) session_rx: std::sync::mpsc::Receiver<SessionSwitch>,
    pub(super) reconfig_result_tx:
        tokio::sync::mpsc::UnboundedSender<crate::native::wiring::Delivered>,
    pub(super) retarget_tx: tokio::sync::mpsc::UnboundedSender<(u32, AckReason)>,
    pub(super) gap_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    /// A codec ask the control task vetted, and where its outcome goes.
    pub(super) format_rx: std::sync::mpsc::Receiver<crate::encode::Codec>,
    pub(super) format_result_tx:
        tokio::sync::mpsc::UnboundedSender<crate::native::wiring::FormatOutcome>,
}
