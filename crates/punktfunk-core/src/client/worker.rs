//! Constructor bag the connect path hands the tokio worker, plus typed close-code
//! classification.
//!
//! [`WorkerArgs`] holds Hello fields, event planes, and the live slots the control
//! task mutates. [`reject_from_close`] maps a QUIC application close onto
//! [`crate::reject::RejectReason`]; transport and local closes keep the original error.

use super::*;
use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::Result;
use crate::input::InputEvent;
use crate::quic::{HdrMeta, HidOutput, PadAudioFrame};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

pub(crate) struct WorkerArgs {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) mode: Mode,
    pub(crate) compositor: CompositorPref,
    pub(crate) gamepad: GamepadPref,
    pub(crate) bitrate_kbps: u32,
    pub(crate) video_caps: u8,
    pub(crate) audio_channels: u8,
    /// Hello request, never the device format. The host answers in `Welcome`; open
    /// the device from that. Anything other than 48 kHz/16-bit also sets
    /// [`crate::quic::CLIENT_CAP_AUDIO_HIRES`].
    pub(crate) audio_rate_hz: u32,
    pub(crate) audio_bits: u8,
    /// Surround coupling asked for; the host answers in `Welcome::audio_layout`.
    pub(crate) audio_layout: crate::audio::AudioLayout,
    /// How this client fills its view; the host reframes a shared frame to it.
    pub(crate) video_fit: crate::video_fit::VideoFit,
    pub(crate) video_codecs: u8,
    pub(crate) preferred_codec: u8,
    pub(crate) display_hdr: Option<HdrMeta>,
    pub(crate) client_caps: u8,
    /// Slice-progressive [`crate::session::Frame::part`] opt-in. Ignored on all-intra
    /// (PyroWave) sessions — newest-wins draining needs whole AUs.
    pub(crate) frame_parts: bool,
    pub(crate) launch: Option<String>,
    /// Display name in `Hello` — the host's approval-list / trust-store label.
    pub(crate) name: Option<String>,
    pub(crate) pin: Option<[u8; 32]>,
    pub(crate) identity: Option<(String, String)>,
    /// Same budget `connect` bounds `ready_rx` with. The dial loop re-dials inside it
    /// so a host still coming up from Wake-on-LAN is not a first-attempt failure.
    pub(crate) connect_timeout: std::time::Duration,
    pub(crate) frames: Arc<FrameChannel>,
    pub(crate) audio_tx: SyncSender<AudioPacket>,
    pub(crate) rumble_tx: SyncSender<RumbleUpdate>,
    /// Feed half of the rumble policy engine. Its `Drop` (demux task end) marks the
    /// engine closed, so the command API always sees teardown.
    pub(crate) rumble_feed: super::rumble::RumbleFeed,
    pub(crate) hidout_tx: SyncSender<HidOutput>,
    /// Inbound `0xD1` pad-audio frames (voice-coil haptics + speaker).
    pub(crate) pad_audio_tx: SyncSender<PadAudioFrame>,
    /// Per-pad render caps (bit0 haptics, bit1 speaker). OR'd into GamepadArrival
    /// flags (bits 8/9) toward a `HOST_CAP_PAD_AUDIO` host only.
    pub(crate) pad_audio_caps: Arc<[AtomicU8; crate::input::MAX_PADS]>,
    /// Pads the embedder switched to controller mouse.
    pub(crate) pad_mouse: Arc<super::pad_mouse::PadMouseShared>,
    pub(crate) hdr_meta_tx: SyncSender<HdrMeta>,
    pub(crate) host_timing_tx: SyncSender<crate::quic::HostTiming>,
    pub(crate) cursor_shape_tx: SyncSender<crate::quic::CursorShape>,
    pub(crate) cursor_state_tx: SyncSender<crate::quic::CursorState>,
    pub(crate) input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    pub(crate) mic_rx: tokio::sync::mpsc::Receiver<(u32, u64, Vec<u8>)>,
    /// Pre-encoded `0xCC` datagrams — rich input and pen batches share this queue.
    pub(crate) rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub(crate) ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    pub(crate) ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Clipboard event plane: control task pushes ClipState/ClipOffer, clipboard
    /// task pushes fetch data.
    pub(crate) clip_event_tx: SyncSender<ClipEventCore>,
    pub(crate) clip_cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClipCommand>,
    pub(crate) ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    pub(crate) shutdown: Arc<AtomicBool>,
    /// [`crate::client::PunktfunkEndReason`] as `u8`, latched beside `shutdown`.
    pub(crate) end_reason: Arc<AtomicU8>,
    /// When set, the worker closes with the deliberate-quit code rather than a generic end.
    pub(crate) quit: Arc<AtomicBool>,
    pub(crate) mode_slot: Arc<std::sync::Mutex<Mode>>,
    pub(crate) probe: Arc<Mutex<ProbeState>>,
    pub(crate) frames_dropped: Arc<AtomicU64>,
    pub(crate) fec_recovered: Arc<AtomicU64>,
    pub(crate) unsustainable_pin_kbps: Arc<AtomicU32>,
    /// Pump mic task counts wire sends and stale-shed drops; the producer counts
    /// queue-full drops.
    pub(crate) mic_stats: Arc<MicUplinkCounters>,
    pub(crate) hot_tids: Arc<Mutex<Vec<i32>>>,
    /// Seeded with the connect-time estimate; the control task's mid-stream re-syncs
    /// update it.
    pub(crate) clock_offset: Arc<AtomicI64>,
    /// Smoothed QUIC round trip (µs) for the overlay; a pump task samples it.
    pub(crate) rtt_us: Arc<AtomicU32>,
    /// Embedder decode-stage samples. The pump drains a window mean into the ABR
    /// decode signal.
    pub(crate) decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Encoder-target mirror. Seeded from Welcome; updated on every `BitrateChanged` ack.
    pub(crate) live_bitrate: Arc<AtomicU32>,
    /// Closed ABR windows, newest last, for an embedder recording a trajectory.
    pub(crate) abr_windows: Arc<Mutex<std::collections::VecDeque<crate::abr::WindowRecord>>>,
    /// Mute mask the control task ORs [`crate::client::AUDIO_MUTE_HOST`] into on every
    /// `AudioState`. The embedder's own bit rides the same cell.
    pub(crate) audio_mute: Arc<AtomicU8>,
    /// OS pad slots this session holds, one bit each ([`crate::quic::PadSlots`]).
    /// The player number the overlay names; `0` until the first pad has a device.
    pub(crate) pad_slots: Arc<AtomicU16>,
    /// Latest launch verdict the host sent ([`crate::quic::LaunchOutcome`]).
    pub(crate) launch_outcome: Arc<Mutex<Option<crate::quic::LaunchOutcome>>>,
    /// Live grants. Seeded from the Welcome advert; every `AccessUpdate` overwrites
    /// (latest wins).
    pub(crate) access_grants: Arc<AtomicU32>,
    /// Client-wall-clock unix seconds; `0` = permanent. Seeded from Welcome
    /// `expires_in_secs`, re-anchored by every `AccessUpdate`.
    pub(crate) access_deadline_unix: Arc<AtomicU64>,
    /// Pushed by the control task only AFTER it has folded the update into the two
    /// live slots above.
    pub(crate) access_tx: SyncSender<crate::quic::AccessUpdate>,
    /// Typed mid-session close from [`crate::reject::RejectReason`]; `0` = none.
    /// Latched beside `end_reason` so an access-expiry close is not rendered as a
    /// generic host error.
    pub(crate) end_reject_code: Arc<AtomicU32>,
    /// The host's own sentence for that close, when it sent one. Set once — a
    /// connection closes once — and only ever with non-empty text.
    pub(crate) end_reject_said: Arc<std::sync::OnceLock<String>>,
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
