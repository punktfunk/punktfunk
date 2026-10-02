//! What the connect path hands the tokio worker, plus typed close-code
//! classification.
//!
//! [`WorkerArgs`] holds the dial's [`ConnectParams`], the worker's plane
//! endpoints, and the [`ClientShared`] cells both sides read and write.
//! [`reject_from_close`] maps a QUIC application close onto
//! [`crate::reject::RejectReason`]; transport and local closes keep the original error.

use super::*;
use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::Mode;
use crate::error::Result;
use crate::input::InputEvent;
use crate::quic::{HdrMeta, HidOutput, PadAudioFrame};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

/// Cells [`NativeClient`] and the worker's tasks share, one `Arc` per dial.
///
/// `clock_offset` and `shutdown` stay `Arc`s: embedders and the data-punch thread hold
/// those cells on their own.
pub(crate) struct ClientShared {
    pub(crate) frames: FrameChannel,
    pub(crate) shutdown: Arc<AtomicBool>,
    /// [`PunktfunkEndReason`] as `u8`, latched beside `shutdown`.
    pub(crate) end_reason: AtomicU8,
    /// [`NativeClient::disconnect_quit`] → [`crate::quic::QUIT_CLOSE_CODE`] (skip keep-alive
    /// linger). A plain drop leaves this false → close code 0.
    pub(crate) quit: AtomicBool,
    /// Welcome mode, then every accepted switch the control task applies.
    pub(crate) mode: Mutex<Mode>,
    pub(crate) probe: Mutex<ProbeState>,
    /// Unrecoverable AUs. Watch for increases to request a keyframe: infinite GOP conceals
    /// reference-missing frames, so a decode-error trigger misses them.
    pub(crate) frames_dropped: AtomicU64,
    /// Parity-repaired shards. HUD windows by diffing successive reads.
    pub(crate) fec_recovered: AtomicU64,
    /// The pinned rate this client could not hold, kbps; `0` until it sheds its backlog
    /// [`super::frame_channel::PIN_SHEDS_TO_WARN`] times.
    pub(crate) unsustainable_pin_kbps: AtomicU32,
    /// The pump counts wire sends and stale-shed drops; the producer counts queue-full drops.
    pub(crate) mic_stats: MicUplinkCounters,
    /// Pump tid plus [`NativeClient::register_hot_thread`] ids. Android feeds ADPF.
    pub(crate) hot_tids: Mutex<Vec<i32>>,
    /// Live host−client offset (ns). Seeded at connect, refreshed by the control task's
    /// re-syncs.
    pub(crate) clock_offset: Arc<AtomicI64>,
    /// Smoothed QUIC round trip (µs), sampled by the worker. `0` until the first sample.
    pub(crate) rtt_us: AtomicU32,
    /// Embedder decode-latency samples. The pump drains a window mean into ABR.
    pub(crate) decode_lat: Mutex<DecodeLatAcc>,
    /// Closed ABR windows, newest last ([`NativeClient::take_abr_windows`]).
    pub(crate) abr_windows: Mutex<std::collections::VecDeque<crate::abr::WindowRecord>>,
    /// What the bring-up ramp measured, once it stopped.
    pub(crate) abr_ramp: Mutex<Option<crate::abr::RampRecord>>,
    /// The host's latest answer about the delivery profile; `None` until one arrives, which
    /// toward an older host is for ever.
    pub(crate) delivery: Mutex<Option<crate::quic::DeliveryChanged>>,
    /// What the host said about its end of the path, when the dial asked.
    pub(crate) host_facts: Mutex<Option<crate::quic::HostFacts>>,
    /// What this dial asked about delivery, so a routine knows the session's kind.
    pub(crate) delivery_ask: Mutex<Option<crate::quic::DeliveryAsk>>,
    /// A clone of the data socket: the same socket as the pump's, so its receive drops and
    /// buffer grant can be read on demand without touching the pump.
    pub(crate) data_sock: Mutex<Option<std::net::UdpSocket>>,
    /// Live encoder target (kbps): the Welcome seed, then every `BitrateChanged` ack.
    pub(crate) live_bitrate_kbps: AtomicU32,
    /// [`crate::hud::RateCut`] code the pump publishes each window; `0` = no standing cut.
    pub(crate) rate_cut: AtomicU8,
    /// RFIs the control task sent, aged at each overlay read.
    pub(crate) recent_rfis: Mutex<RecentRfis>,
    /// What each frame the pump skipped past still lacked, for the RFI line.
    pub(crate) short_frames: Mutex<ShortFrames>,
    /// Loss asks: the decode side's gaps and the pump's short tails, one throttle.
    pub(crate) rfi: Mutex<RfiRecovery>,
    /// Per-pad render caps (bit0 haptics, bit1 speaker). OR'd into GamepadArrival flags
    /// (bits 8/9) toward a `HOST_CAP_PAD_AUDIO` host only.
    pub(crate) pad_audio_caps: [AtomicU8; crate::input::MAX_PADS],
    /// Pads the embedder switched to controller mouse.
    pub(crate) pad_mouse: super::pad_mouse::PadMouseShared,
    /// Live invert-scroll toggle; the input task applies it once at the outbound seam.
    pub(crate) scroll_invert: AtomicBool,
    /// [`AUDIO_MUTE_LOCAL`] | [`AUDIO_MUTE_HOST`]. The embedder owns its bit, the control task
    /// the host's; clearing one leaves the other standing.
    pub(crate) audio_mute: AtomicU8,
    /// OS pad slots this session holds, one bit each ([`crate::quic::PadSlots`]).
    pub(crate) pad_slots: AtomicU16,
    /// Latest launch verdict the host sent ([`crate::quic::LaunchOutcome`]).
    pub(crate) launch_outcome: Mutex<Option<crate::quic::LaunchOutcome>>,
    /// Live grants: the Welcome seed, then every `AccessUpdate` (latest wins).
    pub(crate) access_grants: AtomicU32,
    /// Client-wall unix seconds; `0` = permanent. Re-anchored by every `AccessUpdate`.
    pub(crate) access_deadline_unix: AtomicU64,
    /// Mid-session [`crate::reject::RejectReason`] close code; `0` = none.
    pub(crate) end_reject_code: AtomicU32,
    /// The host's own sentence for that close. Set once, only with non-empty text.
    pub(crate) end_reject_said: std::sync::OnceLock<String>,
}

impl ClientShared {
    /// Pre-handshake state. `GRANT_ALL` / permanent is a placeholder the pump overwrites from
    /// Welcome before the embedder can read it.
    pub(crate) fn new(mode: Mode) -> Self {
        ClientShared {
            frames: FrameChannel::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            end_reason: AtomicU8::new(PunktfunkEndReason::None as u8),
            quit: AtomicBool::new(false),
            mode: Mutex::new(mode),
            probe: Mutex::default(),
            frames_dropped: AtomicU64::new(0),
            fec_recovered: AtomicU64::new(0),
            unsustainable_pin_kbps: AtomicU32::new(0),
            mic_stats: MicUplinkCounters::default(),
            hot_tids: Mutex::default(),
            clock_offset: Arc::new(AtomicI64::new(0)),
            rtt_us: AtomicU32::new(0),
            decode_lat: Mutex::default(),
            abr_windows: Mutex::default(),
            abr_ramp: Mutex::default(),
            delivery: Mutex::default(),
            host_facts: Mutex::default(),
            delivery_ask: Mutex::default(),
            data_sock: Mutex::default(),
            live_bitrate_kbps: AtomicU32::new(0),
            rate_cut: AtomicU8::new(0),
            recent_rfis: Mutex::default(),
            short_frames: Mutex::default(),
            rfi: Mutex::default(),
            pad_audio_caps: std::array::from_fn(|_| AtomicU8::new(0)),
            pad_mouse: Default::default(),
            scroll_invert: AtomicBool::new(false),
            audio_mute: AtomicU8::new(0),
            pad_slots: AtomicU16::new(0),
            launch_outcome: Mutex::default(),
            access_grants: AtomicU32::new(crate::quic::GRANT_ALL),
            access_deadline_unix: AtomicU64::new(0),
            end_reject_code: AtomicU32::new(0),
            end_reject_said: std::sync::OnceLock::new(),
        }
    }
}

pub(crate) struct WorkerArgs {
    /// The dial's ask. `client_caps` already carries the bits core adds
    /// ([`advertised_client_caps`]); `cancel` stays with the connect call.
    pub(crate) params: ConnectParams,
    pub(crate) shared: Arc<ClientShared>,
    pub(crate) audio_tx: SyncSender<AudioPacket>,
    pub(crate) rumble_tx: SyncSender<RumbleUpdate>,
    /// Feed half of the rumble policy engine. Its `Drop` (demux task end) marks the
    /// engine closed, so the command API always sees teardown.
    pub(crate) rumble_feed: super::rumble::RumbleFeed,
    pub(crate) hidout_tx: SyncSender<HidOutput>,
    /// Inbound `0xD1` pad-audio frames (voice-coil haptics + speaker).
    pub(crate) pad_audio_tx: SyncSender<PadAudioFrame>,
    pub(crate) hdr_meta_tx: SyncSender<HdrMeta>,
    pub(crate) host_timing_tx: SyncSender<crate::quic::HostTiming>,
    pub(crate) cursor_shape_tx: super::planes::ShapeSender,
    pub(crate) cursor_state_tx: SyncSender<crate::quic::CursorState>,
    pub(crate) input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    pub(crate) mic_rx: tokio::sync::mpsc::Receiver<(u32, u64, Vec<u8>)>,
    /// Pre-encoded `0xCC` datagrams — rich input and pen batches share this queue.
    pub(crate) rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    /// Touchpad contacts of controller-mouse pads; the input task turns them into pointer motion.
    pub(crate) pad_touch_rx: tokio::sync::mpsc::UnboundedReceiver<super::pad_touch::Contact>,
    pub(crate) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(crate) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Clipboard event plane: control task pushes ClipState/ClipOffer, clipboard
    /// task pushes fetch data.
    pub(crate) clip_event_tx: SyncSender<ClipEventCore>,
    pub(crate) clip_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClipCommand>,
    pub(crate) ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    /// Pushed by the control task only AFTER it has folded the update into
    /// `shared.access_grants` / `shared.access_deadline_unix`.
    pub(crate) access_tx: SyncSender<crate::quic::AccessUpdate>,
}

/// The host's stated rejection and the sentence it sent with it, if the connection
/// closed with a typed application code. `None` for local errors, bare/legacy closes
/// (including our own `LocallyClosed`), and transport failures — those keep their
/// original error.
///
/// The sentence is empty whenever the host had no wording of its own; the caller
/// falls back to ours. See [`sanitize_reason`] for why it is not taken as sent.
pub(crate) fn reject_from_close(
    conn: &quinn::Connection,
) -> Option<(crate::reject::RejectReason, String)> {
    match conn.close_reason()? {
        quinn::ConnectionError::ApplicationClosed(ac) => u32::try_from(u64::from(ac.error_code))
            .ok()
            .and_then(crate::reject::RejectReason::from_close_code)
            .map(|r| (r, sanitize_reason(&ac.reason))),
        _ => None,
    }
}

/// Make host-controlled close bytes safe to render. A host we have paired with is
/// not thereby trusted to fill a client's screen: keep printable characters only,
/// and cap at the wire's one-sentence budget.
pub(crate) fn sanitize_reason(bytes: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let text = text.trim();
    let mut cut = text.len().min(crate::quic::REFUSED_REASON_MAX);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

#[cfg(test)]
mod reason_tests {
    use super::sanitize_reason;

    #[test]
    fn a_hostile_reason_cannot_fill_the_screen_or_move_the_cursor() {
        let long = "é".repeat(4000);
        let out = sanitize_reason(long.as_bytes());
        assert!(
            out.len() <= crate::quic::REFUSED_REASON_MAX,
            "{}",
            out.len()
        );
        assert!(out.chars().all(|c| c == 'é'), "kept only the text");

        assert_eq!(sanitize_reason(b"one\x1b[2Jtwo\n"), "one[2Jtwo");
        assert_eq!(sanitize_reason(b"  padded  "), "padded");
        assert_eq!(sanitize_reason(b""), "");
        // Invalid UTF-8 degrades to replacement characters, never a panic.
        assert!(!sanitize_reason(&[0xff, 0xfe]).is_empty());
    }
}
