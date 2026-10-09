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
    /// Welcome mode, then each switch the host delivers.
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
    /// What the host said about its end of the path in its latest `StreamConfig`.
    pub(crate) host_link: Mutex<crate::quic::HostLink>,
    /// The feedback datagram's numbering, its open ask and the levels it carries.
    pub(crate) feedback: Mutex<super::recovery::FeedbackOut>,
    /// Where feedback datagrams leave: the session's connection, from when the pump has one.
    pub(crate) feedback_tx: std::sync::OnceLock<FeedbackTx>,
    /// Keyframe asks sent; the pump drains them per report window as the ABR recovery signal.
    pub(crate) recovery_kf: AtomicU32,
    /// This device's end of the path, as the dial read it.
    pub(crate) client_link: Mutex<crate::quic::LinkFacts>,
    /// The link rate the host paces at and where it came from, as last told.
    pub(crate) link: Mutex<(u32, crate::abr::LinkSource)>,
    /// The wake shape is on.
    pub(crate) wake_shape: AtomicBool,
    /// The last window's socket dropped packets: the drain signature holds.
    pub(crate) draining: AtomicBool,
    /// This dial asked for probes only ([`crate::quic::EXT_DELIVERY_PROBE_ONLY`]).
    pub(crate) probe_only: AtomicBool,
    /// A clone of the data socket: the same socket as the pump's, so its receive drops and
    /// buffer grant can be read on demand without touching the pump.
    pub(crate) data_sock: Mutex<Option<std::net::UdpSocket>>,
    /// The demux thread's counters for that socket: its full queue drops packets here too.
    pub(crate) demux: Mutex<Option<Arc<crate::transport::shared::SharedStats>>>,
    /// The address the host's packets arrive at, where `data_sock` is unconnected
    /// (`punktfunk/2`'s shared socket) and its own address names no interface.
    pub(crate) local_ip: Mutex<Option<std::net::IpAddr>>,
    /// The `punktfunk/2` session id the host issued, kept for a resume if the link is lost.
    pub(crate) v2_session: Mutex<Option<[u8; 16]>>,
    /// Moves [`Self::mode`] at the first frame of each configured epoch.
    pub(crate) anchor: Mutex<super::pump::anchor::ModeAnchor>,
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
            host_link: Mutex::default(),
            feedback: Mutex::default(),
            feedback_tx: std::sync::OnceLock::new(),
            recovery_kf: AtomicU32::new(0),
            client_link: Mutex::default(),
            link: Mutex::new((crate::abr::LINK_FLOOR_KBPS, crate::abr::LinkSource::Floor)),
            wake_shape: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            probe_only: AtomicBool::new(false),
            data_sock: Mutex::default(),
            demux: Mutex::default(),
            local_ip: Mutex::default(),
            v2_session: Mutex::default(),
            anchor: Mutex::default(),
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

    /// A diagnostic session: the dial asked for probes only, so no video ever comes.
    pub(crate) fn probe_only(&self) -> bool {
        self.probe_only.load(Ordering::Relaxed)
    }

    /// Send `fb` on the session's connection; nothing goes before the pump has one.
    pub(crate) fn send_feedback(&self, fb: &crate::quic::v2::dgram::Feedback) {
        if let Some(tx) = self.feedback_tx.get() {
            tx(fb);
        }
    }

    /// Ask for a keyframe. Every keyframe ask leaves here, so here it is counted.
    pub(crate) fn ask_keyframe(&self) {
        self.recovery_kf.fetch_add(1, Ordering::Relaxed);
        let fb = self
            .feedback
            .lock()
            .unwrap()
            .ask(None, true, Instant::now());
        self.send_feedback(&fb);
    }

    /// Ask the host to send a frame's missing shards again.
    pub(crate) fn ask_nack(&self, nack: crate::quic::v2::dgram::Nack) {
        let ask = self.feedback.lock().unwrap().nack(nack, Instant::now());
        self.send_feedback(&ask);
    }

    /// The decoder took frame `index`. A whole AU decoded clean is acknowledged; an anchor
    /// ends its gap without an ask.
    pub(crate) fn decoder_took(&self, index: u32, flags: u32, whole: bool) {
        if flags & crate::packet::USER_FLAG_RECOVERY_ANCHOR != 0 {
            self.rfi.lock().unwrap().anchored(index);
        }
        if !whole {
            return;
        }
        let ack = self.feedback.lock().unwrap().decoded(index, flags);
        if let Some(fb) = ack {
            self.send_feedback(&fb);
        }
    }

    /// Ask the host to stop referencing frames `first..=last`.
    pub(crate) fn ask_rfi(&self, first: u32, last: u32) {
        self.recent_rfis.lock().unwrap().note(Instant::now());
        let ask = self
            .feedback
            .lock()
            .unwrap()
            .ask(Some((first, last)), false, Instant::now());
        self.send_feedback(&ask);
    }
}

/// Sends one feedback datagram ([`ClientShared::feedback_tx`]).
pub(crate) type FeedbackTx = Box<dyn Fn(&crate::quic::v2::dgram::Feedback) + Send + Sync>;

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
