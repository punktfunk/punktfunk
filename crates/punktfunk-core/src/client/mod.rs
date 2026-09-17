//! Embeddable `punktfunk/1` client connector, behind the `quic` feature.
//!
//! [`NativeClient::connect`] runs QUIC handshake ([`crate::quic`]), UDP data plane
//! ([`crate::session::Session`] on a native thread), and input datagrams. The surface is
//! pull reassembled access units, push input. Platform clients link via the C ABI
//! (`punktfunk_connect` in [`crate::abi`]); `punktfunk-probe` is the Rust-native consumer.
//!
//! One worker owns a tokio runtime (QUIC control plane only) plus a blocking data-plane
//! pump. Frames cross to the embedder on a bounded channel. Methods are safe from any
//! single embedder thread.

// Carve-out with `abi`: thread ids and QoS pins, each with a `// SAFETY:` proof.
// Host code never runs this module.
#![allow(unsafe_code)]

use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::quic::{
    endpoint, ClipControl, ClipKind, ClipOffer, ColorInfo, HdrMeta, HidOutput, PadAudioFrame,
    ProbeRequest, RfiRequest, RichInput,
};
use crate::session::Frame;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod control;
mod frame_channel;
mod pad_mouse;
mod pairing;
mod planes;
mod probe;
mod pump;
mod recovery;
mod rumble;
mod worker;

pub use self::frame_channel::{ADAPT_REPORT_INTERVAL, FLUSH_COOLDOWN, NO_VIDEO_RETRY};
pub use self::planes::AudioPacket;
pub use self::probe::ProbeOutcome;

/// This client silenced its own speakers ([`NativeClient::set_audio_muted`]). The host keeps
/// sending, so a session joined to the same sink still hears the game.
pub const AUDIO_MUTE_LOCAL: u8 = 1 << 0;
/// The operator muted this session from the console ([`crate::quic::AudioState`]). The host
/// stopped encoding this session's audio, so a local unmute brings nothing back.
pub const AUDIO_MUTE_HOST: u8 = 1 << 1;

/// The sentence the overlay shows for a mute mask; `None` when the stream is audible. One
/// place, so no client invents its own wording for whose mute it is.
pub fn audio_mute_label(mask: u8) -> Option<&'static str> {
    match (mask & AUDIO_MUTE_HOST != 0, mask & AUDIO_MUTE_LOCAL != 0) {
        (true, true) => Some("Muted by the host and on this device"),
        (true, false) => Some("Muted by the host"),
        (false, true) => Some("Muted on this device"),
        (false, false) => None,
    }
}

/// Set or clear one bit of a mute mask. Read-modify-write on the atomic: the embedder and
/// the control task own different bits and never wait on each other.
pub(crate) fn set_mute_bit(cell: &AtomicU8, bit: u8, on: bool) {
    if on {
        cell.fetch_or(bit, Ordering::Relaxed);
    } else {
        cell.fetch_and(!bit, Ordering::Relaxed);
    }
}
pub use self::rumble::{ActuatorQuirks, RumbleCommand};

use self::control::{CtrlRequest, Negotiated};
use self::frame_channel::{DecodeLatAcc, FrameChannel, FramePop};
use self::planes::{
    RumbleUpdate, AUDIO_QUEUE, CLIP_EVENT_QUEUE, CURSOR_SHAPE_QUEUE, CURSOR_STATE_QUEUE,
    HDR_META_QUEUE, HIDOUT_QUEUE, HOST_TIMING_QUEUE, PAD_AUDIO_QUEUE, RUMBLE_QUEUE,
};
use self::probe::ProbeState;
use self::pump::run_pump;
use self::recovery::{RecoveryAsk, RfiRecovery};
use self::worker::WorkerArgs;

/// What this client calls itself in the host's `handshake complete` line: build plus the shell
/// and path that dialled. Process-wide because it describes the embedder, not one session; set it
/// before the dial. Empty (the default) sends no `Start` extension at all.
static CLIENT_LABEL: Mutex<String> = Mutex::new(String::new());

/// Set the label [`EXT_TAG_CLIENT`](crate::quic::EXT_TAG_CLIENT) carries. Bounded and stripped
/// on the way in, so the wire never has to trust the caller.
pub fn set_client_label(label: &str) {
    *CLIENT_LABEL.lock().unwrap_or_else(|e| e.into_inner()) = crate::quic::client_label(label);
}

/// The label a dial should send, already bounded.
pub(crate) fn client_label() -> String {
    CLIENT_LABEL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Bracket a bare IPv6 literal so `SocketAddr` parse succeeds (`fd00::1` → `[fd00::1]:4770`).
/// Without brackets the joined string never parses and the error blames the caller's input.
/// V4, hostnames, and already-bracketed input pass through. A v6 dial still fails at connect
/// while the sockets are IPv4-bound.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Hard cap, not working depth: 12 × 10–20 ms frames ≈ 120–240 ms. A filled tokio mpsc
/// can only drop the fresh frame. The pump sheds oldest-first past [`MIC_BACKLOG_MAX`];
/// overflow still drops the fresh frame.
const MIC_QUEUE: usize = 12;

/// Shed oldest-first past ~60 ms of 10 ms frames. Slack for an encode hiccup; more
/// becomes standing delay.
pub(crate) const MIC_BACKLOG_MAX: usize = 6;

/// Shared producer ([`NativeClient::send_mic`]) / pump counters. Monotonic for the session;
/// a HUD windows them by diffing [`NativeClient::mic_stats`] snapshots.
#[derive(Debug, Default)]
pub(crate) struct MicUplinkCounters {
    /// Past every client-side queue, handed to QUIC send.
    pub(crate) sent: AtomicU64,
    /// Enqueue drop: worker queue at [`MIC_QUEUE`].
    pub(crate) dropped_full: AtomicU64,
    /// Pump shed: stale-oldest past [`MIC_BACKLOG_MAX`].
    pub(crate) dropped_stale: AtomicU64,
}

/// Cumulative mic uplink counts per stage; a HUD diffs successive reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct MicUplinkStats {
    pub sent: u64,
    pub dropped_full: u64,
    pub dropped_stale: u64,
}

/// Sparse requests (mode, keyframe, ~1.3 loss reports/s). 32 is hours of headroom;
/// full means the control task is wedged — callers treat that as a closed session.
const CTRL_QUEUE: usize = 32;

/// Console edits and expiry warnings — a handful per session. Live grants/deadline
/// slots hold the truth, so a full queue drops news the embedder would re-derive.
const ACCESS_QUEUE: usize = 8;

/// Client-wall unix seconds from a relative remaining; `0` stays `0` (permanent).
/// Anchor on the client clock: the wire is relative, so host/client skew must not
/// move a countdown rendered from this.
pub(crate) fn access_deadline_from(now_ns: u64, remaining_secs: u32) -> u64 {
    if remaining_secs == 0 {
        0
    } else {
        now_ns / 1_000_000_000 + u64::from(remaining_secs)
    }
}

/// Why a session ended — [`NativeClient::end_reason`], `punktfunk_connection_end_reason` on C.
///
/// Discriminator for a UI: normal finish vs alarm. Values are C ABI: append only, never
/// renumber. Ordered from user-initiated to fault.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunktfunkEndReason {
    /// Not ended, or unknown future ABI value: an older reader degrades to "no opinion".
    None = 0,
    /// This client closed it (stop or drop). The UI already knows.
    Local = 1,
    /// Host launched-game exit ([`crate::quic::APP_EXITED_CLOSE_CODE`]). Normal; a launcher
    /// should return to the library, not host selection.
    GameExited = 2,
    /// Host ended cleanly (operator End, or session finished). Normal.
    HostEnded = 3,
    /// Host closed with a failure. Host log has the detail.
    HostError = 4,
    /// Link died (idle timeout, reset, network). The only "host may be asleep, wake it" case.
    Lost = 5,
}

impl PunktfunkEndReason {
    /// Decode the wire/ABI byte. Unknown values become [`Self::None`]: the writer may be newer.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Local,
            2 => Self::GameExited,
            3 => Self::HostEnded,
            4 => Self::HostError,
            5 => Self::Lost,
            _ => Self::None,
        }
    }

    /// Ordinary finish vs alarm. [`Self::None`] is normal: no evidence of trouble is not
    /// evidence of it.
    pub fn is_normal(self) -> bool {
        !matches!(self, Self::HostError | Self::Lost)
    }
}

#[cfg(feature = "quic")]
impl From<&quinn::ConnectionError> for PunktfunkEndReason {
    /// Map a QUIC close. Host app codes: `APP_EXITED` → GameExited, `0` → HostEnded.
    /// Any other application code is unnamed and treated as `HostError` (visible) rather
    /// than a clean host end. Transport-level failures are `Lost`.
    fn from(e: &quinn::ConnectionError) -> Self {
        match e {
            quinn::ConnectionError::LocallyClosed => Self::Local,
            quinn::ConnectionError::ApplicationClosed(ac) => {
                match u32::try_from(u64::from(ac.error_code)) {
                    Ok(crate::quic::APP_EXITED_CLOSE_CODE) => Self::GameExited,
                    Ok(0) => Self::HostEnded,
                    _ => Self::HostError,
                }
            }
            // TimedOut, Reset, VersionMismatch, TransportError, CidsExhausted, peer transport close.
            _ => Self::Lost,
        }
    }
}

pub struct NativeClient {
    // Per-plane mutex so `NativeClient` is `Sync`. One-thread-per-plane (C ABI); the
    // lock is uncontended there. Two threads racing one plane serialize instead of UB.
    frames: Arc<FrameChannel>,
    audio: Mutex<Receiver<AudioPacket>>,
    rumble: Mutex<Receiver<RumbleUpdate>>,
    /// Policy engine in parallel with the raw `rumble` queue. Consume ONE of the two APIs
    /// ([`NativeClient::next_rumble_command`]).
    rumble_sched: Arc<rumble::RumbleShared>,
    hidout: Mutex<Receiver<HidOutput>>,
    /// DualSense haptics/speaker Opus. Empty unless [`quic::CLIENT_CAP_PAD_AUDIO`] met
    /// [`quic::HOST_CAP_PAD_AUDIO`].
    pad_audio: Mutex<Receiver<PadAudioFrame>>,
    /// Per-pad render caps (bit0 haptics, bit1 speaker). OR'd into arrival flags 8/9 toward
    /// a `HOST_CAP_PAD_AUDIO` host only.
    pad_audio_caps: Arc<[AtomicU8; crate::input::MAX_PADS]>,
    /// Pads translated into pointer and keys ([`NativeClient::set_pad_mouse`]).
    pad_mouse: Arc<pad_mouse::PadMouseShared>,
    hdr_meta: Mutex<Receiver<HdrMeta>>,
    /// Per-AU capture→send timings. Client always advertises [`quic::VIDEO_CAP_HOST_TIMING`];
    /// an older host never sends any.
    host_timing: Mutex<Receiver<crate::quic::HostTiming>>,
    /// Control-stream shapes. Empty unless [`quic::CLIENT_CAP_CURSOR`] met [`quic::HOST_CAP_CURSOR`].
    cursor_shape: Mutex<Receiver<crate::quic::CursorShape>>,
    /// Per-frame cursor state (`0xD0`). Same negotiation gate as shapes.
    cursor_state: Mutex<Receiver<crate::quic::CursorState>>,
    /// Wake-up plane for [`NativeClient::next_access_update`]. Truth is `access_grants` /
    /// `access_deadline_unix`; a dropped event loses news, never accuracy.
    access: Mutex<Receiver<crate::quic::AccessUpdate>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// Bounded ([`MIC_QUEUE`]): pump sheds oldest-first; a full queue drops the fresh frame.
    /// Standing backlog is worse than a dropout.
    mic_tx: tokio::sync::mpsc::Sender<(u32, u64, Vec<u8>)>,
    mic_stats: Arc<MicUplinkCounters>,
    /// Pre-encoded 0xCC bytes ([`RichInput`] and [`crate::quic::PenBatch`]). Worker forwards;
    /// a new 0xCC kind never touches the pump.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Bounded ([`CTRL_QUEUE`]). Sparse; full means the control task is wedged — treat as closed.
    ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    clip: Mutex<Receiver<ClipEventCore>>,
    /// Unbounded like `input_tx`; sparse, at most one paste's bytes each.
    clip_cmd_tx: tokio::sync::mpsc::UnboundedSender<ClipCommand>,
    /// Outbound fetch ids. Stay below [`crate::clipboard::INBOUND_REQ_FLAG`] or they collide
    /// with inbound serve `req_id`.
    next_xfer_id: AtomicU32,
    /// Wrapping [`crate::quic::PenBatch::seq`]; the host's reorder gate compares it.
    pen_seq: AtomicU16,
    pub host_caps: u8,
    pub host_caps2: u8,
    /// `0` when the host did not advertise a management port.
    pub mgmt_port: u16,
    /// Seeded from Welcome, latest [`crate::quic::AccessUpdate`] wins.
    access_grants: Arc<AtomicU32>,
    /// Client-wall unix seconds; `0` = permanent.
    access_deadline_unix: Arc<AtomicU64>,
    /// Mid-session [`crate::reject::RejectReason`] close code; `0` = none.
    end_reject_code: Arc<AtomicU32>,
    /// The sentence the host sent with that close, if any.
    end_reject_said: Arc<std::sync::OnceLock<String>>,
    probe: Arc<Mutex<ProbeState>>,
    shutdown: Arc<AtomicBool>,
    /// [`PunktfunkEndReason`] as `u8`, latched with `shutdown`.
    end_reason: Arc<AtomicU8>,
    /// [`NativeClient::disconnect_quit`] → [`crate::quic::QUIT_CLOSE_CODE`] (skip keep-alive
    /// linger). A plain drop leaves this false → close code 0.
    quit: Arc<AtomicBool>,
    /// Unrecoverable AUs. Watch for increases to request a keyframe: infinite GOP conceals
    /// reference-missing frames, so a decode-error trigger misses them.
    frames_dropped: Arc<AtomicU64>,
    /// Parity-repaired shards. HUD windows by diffing successive reads.
    fec_recovered: Arc<AtomicU64>,
    /// See [`unsustainable_pin_kbps`](Self::unsustainable_pin_kbps).
    unsustainable_pin_kbps: Arc<AtomicU32>,
    /// Shared loss-range detector for [`note_frame_index`](Self::note_frame_index): next
    /// expected `frame_index` plus RFI throttle. Avoids per-embedder wrapping arithmetic.
    rfi: Mutex<RfiRecovery>,
    /// Pump tid plus [`NativeClient::register_hot_thread`] ids. Android feeds ADPF. Empty
    /// without `gettid` (see [`current_hot_tid`]).
    hot_tids: Arc<Mutex<Vec<i32>>>,
    /// Live host−client offset (ns). Seeded at connect, refreshed every 60 s and on the
    /// pump's first no-op clock flush.
    clock_offset: Arc<AtomicI64>,
    /// `displayed + clock_offset − pts` (ns). `0` = nothing presented yet. Presenter writes;
    /// audio reads to land with the picture ([`crate::audio::AvSync`]). Lives next to
    /// `clock_offset` because neither plane owns the other.
    video_e2e_ns: Arc<AtomicU64>,
    /// Smoothed A/V offset (ms); positive = audio late. Audio writes, HUD reads.
    audio_av_offset_ms: Arc<AtomicI64>,
    /// Playback-ring depth (ms). Audio writes, HUD reads.
    audio_buffer_ms: Arc<AtomicU32>,
    /// Why the speakers are silent: [`AUDIO_MUTE_LOCAL`] | [`AUDIO_MUTE_HOST`]. The embedder
    /// owns its bit, the control task the host's; clearing one leaves the other standing.
    audio_mute: Arc<AtomicU8>,
    /// OS pad slots the host gave this session, one bit each. Slot `n` is player
    /// `n + 1`; `0` until a pad of ours has a device on the host.
    pad_slots: Arc<AtomicU16>,
    /// Latest launch verdict from the host; `None` until one arrives.
    launch_outcome: Arc<Mutex<Option<crate::quic::LaunchOutcome>>>,
    /// Smoothed QUIC round trip (µs), sampled by the worker. `0` until the first sample.
    rtt_us: Arc<AtomicU32>,
    /// The stats overlay window. Receipt and 0xCF timings land in it as they are pulled.
    hud: Arc<crate::hud::Stats>,
    /// Embedder decode-latency samples. Pump drains per window into ABR; see [`DecodeLatAcc`].
    decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Live encoder target (kbps), follows `BitrateChanged`. [`resolved_bitrate_kbps`] is the
    /// frozen session-start value. `0` = old host that never reported a rate.
    live_bitrate_kbps: Arc<AtomicU32>,
    /// Closed ABR windows waiting to be read ([`NativeClient::take_abr_windows`]).
    abr_windows: Arc<Mutex<std::collections::VecDeque<crate::abr::WindowRecord>>>,
    /// ABR armed (Automatic, not rate-pinned PyroWave). Skip per-frame decode measurement when
    /// false ([`wants_decode_latency`](Self::wants_decode_latency)).
    wants_decode: bool,
    worker: Option<std::thread::JoinHandle<()>>,
    mode: Arc<std::sync::Mutex<Mode>>,
    /// SHA-256 of the cert the host presented. A TOFU caller (`pin = None`) persists this.
    pub host_fingerprint: [u8; 32],
    /// Host-resolved compositor. `Auto` = older host. Gamescope capture has no cursor, so
    /// clients draw one locally by default.
    pub resolved_compositor: CompositorPref,
    /// Host-resolved virtual pad. `Auto` = older host (assume Xbox 360, no DualSense feedback).
    pub resolved_gamepad: GamepadPref,
    /// Hello ask, kept beside the host's answer. `resolved` matches a pad only when that pad
    /// declared this value; see [`pad_motion_reaches`](crate::config::pad_motion_reaches).
    pub requested_gamepad: GamepadPref,
    /// Host-configured encoder rate (kbps). Request clamped to host range, or host default if
    /// we asked `0`. `0` = older host that didn't report it.
    pub resolved_bitrate_kbps: u32,
    /// Bytes of AU per datagram — parse window for chunk-aligned AUs
    /// ([`crate::packet::USER_FLAG_CHUNK_ALIGNED`]).
    pub shard_payload: u16,
    /// Connect-time host−client offset (ns). Add to a local stamp to express it in capture
    /// clock. `0` = old host or synced clocks. Ongoing math should read
    /// [`clock_offset_now_ns`](Self::clock_offset_now_ns).
    pub clock_offset_ns: i64,
    /// Encode bit depth: `8`, or `10` for Main10/HDR. `8` for an older host.
    pub bit_depth: u8,
    /// Host colour signalling for decoder/presenter. [`ColorInfo::SDR_BT709`] for an older
    /// host. HDR mastering arrives via [`NativeClient::next_hdr_meta`].
    pub color: ColorInfo,
    /// HEVC `chroma_format_idc` ([`quic::CHROMA_IDC_420`] or [`quic::CHROMA_IDC_444`]). SPS
    /// is authoritative; this pre-sizes the decoder. `420` for an older host.
    pub chroma_format: u8,
    /// Host-resolved channels: `2` / `6` / `8`. Build the Opus decoder from this via
    /// [`crate::audio::layout_for`], never from the request. Omitted → `2`.
    pub audio_channels: u8,
    /// Selects the decoder: [`quic::AUDIO_CODEC_OPUS`] (`0xC9`) or [`quic::AUDIO_CODEC_PCM`]
    /// (`0xD3`). A 48 kHz/16-bit lossless session and a 48 kHz Opus session agree on every
    /// other resolved value. Fixed for the session — the output device is open at one format.
    pub audio_codec: u8,
    /// Host-resolved sample rate. `48_000` for Opus and older hosts; a hi-res session may
    /// land lower than asked. Open the output device from THIS, never from the request.
    pub audio_sample_rate_hz: u32,
    /// 16 or 24. Stride on the `0xD3` plane; `16` on Opus (samples reach the embedder as f32).
    pub audio_bits: u8,
    /// Microseconds of audio in one `0xD3` datagram; `0` on Opus (fixed 5 ms on `0xC9`).
    /// Negotiated from path MTU — at 96 kHz/24-bit the default ceiling only fits 2 ms.
    ///
    /// Nominal, not a duration. 44.1 kHz divides no rung: 5 ms at 44 100 Hz is 220 samples
    /// (4 988 662 ns). Size rings from this; time from
    /// [`crate::audio::pcm::frame_duration_ns`]. Advancing a clock by this invents 2.3 ms/s.
    pub audio_frame_us: u16,
    /// Surround coupling the host encodes, a [`crate::audio::AudioLayout`] wire id kept
    /// verbatim. Build the Opus decoder from `AudioLayout::from_wire(self.audio_layout)`, never
    /// from the request, and refuse audio on `None` rather than pair channels wrongly. `0` for
    /// an older host.
    pub audio_layout: u8,
    /// Host-resolved video codec. Build the decoder from THIS; do not assume HEVC.
    pub codec: u8,
}

impl NativeClient {
    /// Payload kbps of the lossless plane: `rate × depth × channels` (CBR, exact). `None` for
    /// Opus (VBR, host-chosen via [`crate::audio::plan_audio_budget`]) — a short window would
    /// read as jitter. Header and QUIC framing are omitted, as with every other quoted bitrate.
    pub fn audio_kbps(&self) -> Option<u32> {
        (self.audio_codec == crate::quic::AUDIO_CODEC_PCM).then(|| {
            crate::audio::pcm::bitrate_kbps(
                self.audio_sample_rate_hz,
                self.audio_bits,
                self.audio_channels,
            )
        })
    }
}

/// Pin the calling thread to user-interactive QoS.
///
/// Apple consumers drain planes on `.userInteractive` and block on the channels these
/// workers feed. Default-QoS producers invert priority. Android uses nice −8; no-op
/// elsewhere (no QoS scheduler).
#[cfg(target_vendor = "apple")]
fn pin_thread_user_interactive() {
    // SAFETY: sets only the current thread's QoS class — always valid to call.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
/// Nice −8 (URGENT_DISPLAY). Default 0 parks a bursty pump on a little core; a few ms of
/// delay overflows the socket recv buffer → wire loss the link never saw. Below decode's
/// −10 so the display path still wins. Best-effort.
#[cfg(target_os = "android")]
fn pin_thread_user_interactive() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls; a refusal is
    // reported via the return value (ignored — a missed boost, not an error on the data path).
    unsafe {
        let tid = libc::gettid();
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8);
    }
}
#[cfg(not(any(target_vendor = "apple", target_os = "android")))]
fn pin_thread_user_interactive() {}

/// Wall-clock now (ns, CLOCK_REALTIME) for latency math against host `pts_ns` after skew.
///
/// [`crate::audio::AvSync`] lives in an embedder crate and must use this basis: `Instant` or
/// a monotonic clock is wrong by boot time and still looks plausible.
pub fn now_realtime_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Calling thread's kernel id for ADPF-style hints. Linux/Android `gettid`; elsewhere `None`.
#[cfg(any(target_os = "android", target_os = "linux"))]
fn current_hot_tid() -> Option<i32> {
    // SAFETY: `gettid` reads the calling thread's kernel id — an always-safe syscall, no args.
    Some(unsafe { libc::gettid() })
}
#[cfg(not(any(target_os = "android", target_os = "linux")))]
fn current_hot_tid() -> Option<i32> {
    None
}

/// Record the calling tid (deduped). Missing `gettid` or a poisoned lock skips — a missed
/// hint, not a data-path error.
fn register_hot_tid(reg: &Mutex<Vec<i32>>) {
    if let Some(t) = current_hot_tid() {
        if let Ok(mut v) = reg.lock() {
            if !v.contains(&t) {
                v.push(t);
            }
        }
    }
}

/// Default [`NativeClient::connect`] `name`: `/etc/hostname`, then env, then OS hostname.
/// Lives here so the C ABI (`punktfunk_connect`) shares it.
///
/// Apple GUI processes have neither `COMPUTERNAME` nor `HOSTNAME` (`launchd` does not
/// export the shell variable), so without `gethostname` every Apple client knocks as
/// "This device". Pass a better name via [`crate::abi::punktfunk_connect_ex10`].
pub fn device_name() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(os_hostname)
        .unwrap_or_else(|| "This device".into())
}

/// OS hostname, or `None` if missing/useless. Strip `.local` (mDNS host label). Reject
/// `localhost` — it labels nothing.
#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `len` bytes into the caller's buffer; this one is a
    // stack array we own and pass its true length. A truncating write may omit the NUL, which
    // the `position` fallback below covers.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = std::str::from_utf8(&buf[..end]).ok()?.trim();
    let s = s.strip_suffix(".local").unwrap_or(s);
    (!s.is_empty() && !s.eq_ignore_ascii_case("localhost")).then(|| s.to_string())
}

/// Windows: `COMPUTERNAME` is always set, so the env step never falls through. Avoid winsock.
#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

/// Embedder `client_caps` plus the two bits core decides. Named so a test can pin the
/// bandwidth ask (1.5–4.6 Mbps).
///
/// [`quic::CLIENT_CAP_AUDIO_RED`] always: demux recovers into the same queue, ~1 %, host
/// still decides whether to spend it. [`quic::CLIENT_CAP_AUDIO_HIRES`] only when the caller
/// specified a format — it costs 1.5–4.6 Mbps ABR cannot reclaim, and advertising it without
/// opening the device plays nothing. OR'd in, never substituted, so 48 kHz/16-bit lossless
/// stays expressible by setting the bit in `client_caps` itself.
fn advertised_client_caps(client_caps: u8, audio_rate_hz: u32, audio_bits: u8) -> u8 {
    // Non-zero = caller specified a format, not "differs from default". 48 kHz/16-bit is
    // both the default and the cheapest lossless rung; a "differs" rule would make it
    // unreachable. [`NativeClient::connect`] passes 0/0; the wire encodes 48 000/16 as absent.
    let hires = audio_rate_hz != 0 || audio_bits != 0;
    client_caps
        | crate::quic::CLIENT_CAP_AUDIO_RED
        | if hires {
            crate::quic::CLIENT_CAP_AUDIO_HIRES
        } else {
            0
        }
}

impl NativeClient {
    /// Connect to a `punktfunk/1` host at (up to) `mode`. Blocks until handshake or `timeout`.
    ///
    /// `pin`: expected SHA-256 of the host cert. Mismatch → [`PunktfunkError::Crypto`].
    /// `None` = TOFU; read [`NativeClient::host_fingerprint`] afterwards.
    ///
    /// `identity`: persistent PEM + PKCS#8 ([`endpoint::generate_identity`]) for TLS client
    /// auth. `None` = anonymous (rejected by hosts that require pairing).
    ///
    /// Asks for legacy Opus 48 kHz / 16-bit so `Hello` stays byte-identical to the pre-hi-res
    /// wire. Lossless callers use [`connect_with_audio_format`](Self::connect_with_audio_format).
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        host: &str,
        port: u16,
        mode: Mode,
        compositor: CompositorPref,
        gamepad: GamepadPref,
        bitrate_kbps: u32,
        // quic::VIDEO_CAP_10BIT / VIDEO_CAP_HDR. Host upgrades only when the matching bit is
        // set. 0 = 8-bit BT.709, which every client understands.
        video_caps: u8,
        // 2 / 6 / 8; host clamps to capture and echoes in [`NativeClient::audio_channels`].
        audio_channels: u8,
        // Decode bitfield (H264 / HEVC / AV1) plus a single-bit preference (`0` = auto).
        // Host echoes the chosen codec in [`NativeClient::codec`].
        video_codecs: u8,
        preferred_codec: u8,
        // Panel volume for the virtual display's EDID. `None` = unknown/SDR (host EDID defaults).
        display_hdr: Option<HdrMeta>,
        // Set [`crate::quic::CLIENT_CAP_CURSOR`] only if this embedder renders the pointer:
        // the host then stops compositing it, so a non-renderer streams with no cursor. `0`
        // = composited.
        client_caps: u8,
        // AU prefixes as [`Frame`]s with `part = Some` while the rest is on the wire. Set
        // only if the decoder understands parts (MediaCodec BUFFER_FLAG_PARTIAL_FRAME).
        frame_parts: bool,
        launch: Option<String>,
        // [`crate::quic::Hello::name`]. `None` → fingerprint "device abcd1234". Usually
        // [`device_name`].
        name: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        Self::connect_with_audio_format(
            host,
            port,
            mode,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            // 0/0 = unspecified, so `Hello` stays pre-hi-res. Explicit 48 000/16 would mean
            // "cheapest lossless rung" under `advertised_client_caps`.
            0,
            0,
            crate::audio::AudioLayout::Legacy,
            crate::video_fit::VideoFit::Fit,
            video_codecs,
            preferred_codec,
            display_hdr,
            client_caps,
            frame_parts,
            launch,
            name,
            pin,
            identity,
            timeout,
            None,
        )
    }

    /// [`connect`](Self::connect) plus the audio format asked for: `audio_rate_hz`
    /// ([`crate::audio::pcm::rate_is_supported`]) and `audio_bits` (16 or 24).
    ///
    /// [`quic::CLIENT_CAP_AUDIO_HIRES`] is set when either argument is non-zero (`0` =
    /// unspecified, which [`connect`](Self::connect) passes). Not "differs from 48/16": that
    /// would make the cheapest lossless rung unreachable. Unlike [`quic::CLIENT_CAP_AUDIO_RED`]
    /// (always OR'd), hi-res costs 1.5–4.6 Mbps ABR cannot reclaim.
    ///
    /// 48 kHz/16-bit through this pair stays Opus (byte-identical to legacy). Ask 48 kHz/24-bit
    /// for lossless at the default rate, or set [`quic::CLIENT_CAP_AUDIO_HIRES`] in `client_caps`
    /// (OR'd, never substituted). The host may still answer Opus; open the device from
    /// [`audio_codec`](Self::audio_codec) / [`audio_sample_rate_hz`](Self::audio_sample_rate_hz) /
    /// [`audio_bits`](Self::audio_bits).
    #[allow(clippy::too_many_arguments)]
    pub fn connect_with_audio_format(
        host: &str,
        port: u16,
        mode: Mode,
        compositor: CompositorPref,
        gamepad: GamepadPref,
        bitrate_kbps: u32,
        video_caps: u8,
        audio_channels: u8,
        audio_rate_hz: u32,
        audio_bits: u8,
        // Surround coupling to ask for ([`crate::audio::AudioLayout`]). The host answers in
        // [`NativeClient::audio_layout`]; `Legacy` keeps the Hello byte-identical.
        audio_layout: crate::audio::AudioLayout,
        // How this client fills its view ([`crate::quic::Hello::video_fit`]); `Fit` keeps the
        // Hello byte-identical.
        video_fit: crate::video_fit::VideoFit,
        video_codecs: u8,
        preferred_codec: u8,
        display_hdr: Option<HdrMeta>,
        client_caps: u8,
        frame_parts: bool,
        launch: Option<String>,
        name: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
        // Abort while blocked. Request-access can park ~185 s; Cancel cannot honour that if
        // this call ignores the flag. Same give-up as budget expiry (quit + shutdown). Do not
        // alias onto `shutdown` — the pump means "this connection died" and a caller-set flag
        // would race the end reason. `None` = uncancelable.
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<NativeClient> {
        let frame_chan = Arc::new(FrameChannel::new());
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<RumbleUpdate>(RUMBLE_QUEUE);
        let rumble_sched = Arc::new(rumble::RumbleShared::new());
        let rumble_feed = rumble::RumbleFeed(rumble_sched.clone());
        let (hidout_tx, hidout_rx) = std::sync::mpsc::sync_channel::<HidOutput>(HIDOUT_QUEUE);
        let (pad_audio_tx, pad_audio_rx) =
            std::sync::mpsc::sync_channel::<PadAudioFrame>(PAD_AUDIO_QUEUE);
        let pad_audio_caps: Arc<[AtomicU8; crate::input::MAX_PADS]> =
            Arc::new(std::array::from_fn(|_| AtomicU8::new(0)));
        let pad_mouse = Arc::new(pad_mouse::PadMouseShared::default());
        let (hdr_meta_tx, hdr_meta_rx) = std::sync::mpsc::sync_channel::<HdrMeta>(HDR_META_QUEUE);
        let (host_timing_tx, host_timing_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::HostTiming>(HOST_TIMING_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel::<(u32, u64, Vec<u8>)>(MIC_QUEUE);
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(CTRL_QUEUE);
        let (clip_event_tx, clip_event_rx) =
            std::sync::mpsc::sync_channel::<ClipEventCore>(CLIP_EVENT_QUEUE);
        let (clip_cmd_tx, clip_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClipCommand>();
        let (cursor_shape_tx, cursor_shape_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::CursorShape>(CURSOR_SHAPE_QUEUE);
        let (cursor_state_tx, cursor_state_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::CursorState>(CURSOR_STATE_QUEUE);
        let (access_tx, access_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::AccessUpdate>(ACCESS_QUEUE);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let end_reason = Arc::new(AtomicU8::new(PunktfunkEndReason::None as u8));
        let quit = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));
        let probe = Arc::new(Mutex::new(ProbeState::default()));
        let frames_dropped = Arc::new(AtomicU64::new(0));
        let fec_recovered = Arc::new(AtomicU64::new(0));
        let unsustainable_pin_kbps = Arc::new(AtomicU32::new(0));
        let mic_stats = Arc::new(MicUplinkCounters::default());
        let hot_tids = Arc::new(Mutex::new(Vec::new()));
        let clock_offset = Arc::new(AtomicI64::new(0));
        let video_e2e_ns = Arc::new(AtomicU64::new(0));
        let audio_av_offset_ms = Arc::new(AtomicI64::new(0));
        let audio_buffer_ms = Arc::new(AtomicU32::new(0));
        let audio_mute = Arc::new(AtomicU8::new(0));
        let pad_slots = Arc::new(AtomicU16::new(0));
        let launch_outcome = Arc::new(Mutex::new(None));
        let rtt_us = Arc::new(AtomicU32::new(0));
        let decode_lat = Arc::new(Mutex::new(DecodeLatAcc::default()));
        let abr_windows = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        // Pump seeds from Welcome before ready_tx, then follows every ack.
        let live_bitrate = Arc::new(AtomicU32::new(0));
        // Same seeding: Welcome before ready_tx, then every AccessUpdate. GRANT_ALL /
        // permanent here is the pre-handshake placeholder.
        let access_grants = Arc::new(AtomicU32::new(crate::quic::GRANT_ALL));
        let access_deadline_unix = Arc::new(AtomicU64::new(0));
        let end_reject_code = Arc::new(AtomicU32::new(0));
        let end_reject_said: Arc<std::sync::OnceLock<String>> =
            Arc::new(std::sync::OnceLock::new());

        let host = host.to_string();
        let frame_chan_w = frame_chan.clone();
        let shutdown_w = shutdown.clone();
        let end_reason_w = end_reason.clone();
        let quit_w = quit.clone();
        let mode_slot_w = mode_slot.clone();
        let probe_w = probe.clone();
        let frames_dropped_w = frames_dropped.clone();
        let fec_recovered_w = fec_recovered.clone();
        let unsustainable_pin_kbps_w = unsustainable_pin_kbps.clone();
        let mic_stats_w = mic_stats.clone();
        let hot_tids_w = hot_tids.clone();
        let clock_offset_w = clock_offset.clone();
        let rtt_us_w = rtt_us.clone();
        let decode_lat_w = decode_lat.clone();
        let abr_windows_w = abr_windows.clone();
        let live_bitrate_w = live_bitrate.clone();
        let pad_audio_caps_w = pad_audio_caps.clone();
        let pad_mouse_w = pad_mouse.clone();
        let audio_mute_w = audio_mute.clone();
        let pad_slots_w = pad_slots.clone();
        let launch_outcome_w = launch_outcome.clone();
        let access_grants_w = access_grants.clone();
        let access_deadline_w = access_deadline_unix.clone();
        let end_reject_w = end_reject_code.clone();
        let end_reject_said_w = end_reject_said.clone();
        let ctrl_tx_pump = ctrl_tx.clone(); // pump sends adaptive-FEC LossReports
        let worker = std::thread::Builder::new()
            .name("punktfunk-client".into())
            .spawn(move || {
                pin_thread_user_interactive(); // runtime + handshake thread
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    // Workers + spawn_blocking pump match consumer QoS — no priority inversion.
                    .on_thread_start(pin_thread_user_interactive)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(PunktfunkError::Io(e)));
                        return;
                    }
                };
                rt.block_on(run_pump(WorkerArgs {
                    host,
                    port,
                    mode,
                    compositor,
                    gamepad,
                    bitrate_kbps,
                    video_caps,
                    audio_channels,
                    audio_rate_hz,
                    audio_bits,
                    audio_layout,
                    video_fit,
                    video_codecs,
                    preferred_codec,
                    display_hdr,
                    // RED is core-decided; HIRES is not. See `advertised_client_caps`.
                    client_caps: advertised_client_caps(client_caps, audio_rate_hz, audio_bits),
                    frame_parts,
                    launch,
                    name,
                    pin,
                    identity,
                    connect_timeout: timeout,
                    frames: frame_chan_w,
                    audio_tx,
                    rumble_tx,
                    rumble_feed,
                    hidout_tx,
                    pad_audio_tx,
                    pad_audio_caps: pad_audio_caps_w,
                    pad_mouse: pad_mouse_w,
                    hdr_meta_tx,
                    host_timing_tx,
                    cursor_shape_tx,
                    cursor_state_tx,
                    input_rx,
                    mic_rx,
                    rich_input_rx,
                    ctrl_rx,
                    ctrl_tx: ctrl_tx_pump,
                    clip_event_tx,
                    clip_cmd_rx,
                    ready_tx,
                    shutdown: shutdown_w,
                    end_reason: end_reason_w,
                    quit: quit_w,
                    mode_slot: mode_slot_w,
                    probe: probe_w,
                    frames_dropped: frames_dropped_w,
                    fec_recovered: fec_recovered_w,
                    unsustainable_pin_kbps: unsustainable_pin_kbps_w,
                    mic_stats: mic_stats_w,
                    hot_tids: hot_tids_w,
                    clock_offset: clock_offset_w,
                    rtt_us: rtt_us_w,
                    decode_lat: decode_lat_w,
                    abr_windows: abr_windows_w,
                    live_bitrate: live_bitrate_w,
                    audio_mute: audio_mute_w,
                    pad_slots: pad_slots_w,
                    launch_outcome: launch_outcome_w,
                    access_grants: access_grants_w,
                    access_deadline_unix: access_deadline_w,
                    access_tx,
                    end_reject_code: end_reject_w,
                    end_reject_said: end_reject_said_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        // Poll so `cancel` can abort; a parked request-access handshake has nothing to wake on.
        const READY_POLL: Duration = Duration::from_millis(50);
        let deadline = std::time::Instant::now() + timeout;
        let negotiated = loop {
            match ready_rx.recv_timeout(READY_POLL) {
                Ok(Ok(t)) => break t,
                Ok(Err(e)) => return Err(e),
                // Keep waiting unless budget spent or cancelled. Disconnected = worker died
                // without reporting; the give-up arm below covers it. Cancel and expiry share
                // that arm: both owe the host the same close.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline
                        && !cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) => {}
                Err(_) => {
                    // Failed connect must not linger if handshake lands late: QUIT, not close
                    // code 0, so the host tears down instead of holding a virtual display.
                    quit.store(true, Ordering::SeqCst);
                    shutdown.store(true, Ordering::SeqCst);
                    return Err(PunktfunkError::Timeout);
                }
            }
        };
        *mode_slot.lock().unwrap() = negotiated.mode;
        let hud = Arc::new(crate::hud::Stats::new(clock_offset.clone()));
        Ok(NativeClient {
            frames: frame_chan,
            audio: Mutex::new(audio_rx),
            rumble: Mutex::new(rumble_rx),
            rumble_sched,
            hidout: Mutex::new(hidout_rx),
            pad_audio: Mutex::new(pad_audio_rx),
            pad_audio_caps,
            pad_mouse,
            hdr_meta: Mutex::new(hdr_meta_rx),
            host_timing: Mutex::new(host_timing_rx),
            cursor_shape: Mutex::new(cursor_shape_rx),
            cursor_state: Mutex::new(cursor_state_rx),
            access: Mutex::new(access_rx),
            audio_mute,
            pad_slots,
            launch_outcome,
            access_grants,
            access_deadline_unix,
            end_reject_code,
            end_reject_said,
            input_tx,
            mic_tx,
            mic_stats,
            rich_input_tx,
            ctrl_tx,
            clip: Mutex::new(clip_event_rx),
            clip_cmd_tx,
            next_xfer_id: AtomicU32::new(1),
            pen_seq: AtomicU16::new(0),
            host_caps: negotiated.host_caps,
            host_caps2: negotiated.host_caps2,
            mgmt_port: negotiated.mgmt_port,
            probe,
            shutdown,
            end_reason,
            quit,
            worker: Some(worker),
            frames_dropped,
            fec_recovered,
            unsustainable_pin_kbps,
            rfi: Mutex::new(RfiRecovery::default()),
            hot_tids,
            clock_offset,
            video_e2e_ns,
            audio_av_offset_ms,
            audio_buffer_ms,
            rtt_us,
            hud,
            decode_lat,
            abr_windows,
            live_bitrate_kbps: live_bitrate,
            // Match the pump: Automatic, not rate-pinned PyroWave, AND host echoed a rate.
            // Dropping the last term over-advertises against an old host that reports no rate.
            wants_decode: bitrate_kbps == 0
                && negotiated.codec != crate::quic::CODEC_PYROWAVE
                && negotiated.bitrate_kbps > 0,
            mode: mode_slot,
            host_fingerprint: negotiated.host_fingerprint,
            resolved_compositor: negotiated.compositor,
            resolved_gamepad: negotiated.gamepad,
            requested_gamepad: gamepad,
            resolved_bitrate_kbps: negotiated.bitrate_kbps,
            shard_payload: negotiated.shard_payload,
            clock_offset_ns: negotiated.clock_offset_ns,
            bit_depth: negotiated.bit_depth,
            color: negotiated.color,
            chroma_format: negotiated.chroma_format,
            audio_channels: negotiated.audio_channels,
            audio_codec: negotiated.audio_codec,
            audio_sample_rate_hz: negotiated.audio_rate_hz,
            audio_bits: negotiated.audio_bits,
            audio_frame_us: negotiated.audio_frame_us,
            audio_layout: negotiated.audio_layout,
            codec: negotiated.codec,
        })
    }

    /// Handshake-only reachability of `host:port`. Does not use mDNS (routed/VPN hosts
    /// never advertise). Blocks up to `timeout`.
    ///
    /// Reachability alone: use it for a record with no pin to compare against. A caller
    /// holding one asks [`NativeClient::probe_identity`] — an address is not an identity.
    pub fn probe(host: &str, port: u16, timeout: Duration) -> bool {
        Self::probe_identity(host, port, timeout).is_some()
    }

    /// Who answers at `host:port`: the SHA-256 of the certificate the peer presents, or
    /// `None` when nothing completed a handshake within `timeout`.
    ///
    /// The handshake itself is unpinned, so this reports the address's occupant rather than
    /// verifying one — the caller compares. That comparison is what a presence pip owes: a
    /// stranger who inherits a sleeping host's DHCP lease answers at its address, and reading
    /// that as the host lights the pip AND, since wake is gated on `!online`, silences
    /// Wake-on-LAN for exactly the machine that needs it. Two OS installs of a dual-boot box
    /// share one lease the same way.
    pub fn probe_identity(host: &str, port: u16, timeout: Duration) -> Option<[u8; 32]> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let host = host.to_string();
        rt.block_on(async move {
            // Hostname (MagicDNS, `.local`), not always an IP literal — resolve, don't parse.
            let mut addrs = tokio::net::lookup_host((host.as_str(), port)).await.ok()?;
            let remote = addrs.next()?;
            // pin = None accepts any cert and records it. Failures are DNS / no route /
            // connect timeout.
            let (ep, observed) = endpoint::client_pinned_with_identity(None, None);
            let ep = ep.ok()?;
            let reachable = match ep.connect(remote, "punktfunk") {
                Ok(connecting) => {
                    matches!(tokio::time::timeout(timeout, connecting).await, Ok(Ok(_)))
                }
                Err(_) => false,
            };
            ep.close(0u32.into(), b"probe");
            let _ = tokio::time::timeout(Duration::from_millis(200), ep.wait_idle()).await;
            // The slot is written by the cert verifier, so it is only meaningful once the
            // handshake completed: a refused or timed-out connect leaves whatever it had.
            reachable.then(|| *observed.lock().unwrap()).flatten()
        })
    }

    /// Welcome mode, until an accepted [`NativeClient::request_mode`] switches it.
    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    /// Queue a live mode switch (no reconnect). Accepted: next frames open with an IDR and
    /// [`NativeClient::mode`] updates. Rejected: session unchanged.
    pub fn request_mode(&self, mode: Mode) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Mode(mode))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Who draws the pointer: `true` = client (host forwards shape/state, excludes it from
    /// video), `false` = host composites. Latest-wins; no-op without
    /// [`HOST_CAP_CURSOR`](crate::quic::HOST_CAP_CURSOR).
    pub fn set_cursor_render(&self, client_draws: bool) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::CursorRender(crate::quic::CursorRenderMode {
                client_draws,
            }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Fire-and-forget IDR. Throttle: decode stays wedged until it lands, so per-frame
    /// requests flood the control stream.
    pub fn request_keyframe(&self) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Keyframe)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Recover `[first_frame, last_frame]` by RFI instead of a full IDR. Capable hosts emit a
    /// P-frame tagged [`crate::packet::USER_FLAG_RECOVERY_ANCHOR`]; others force an IDR
    /// ([`request_keyframe`](Self::request_keyframe)). Prefer on loss; keyframe is the backstop
    /// when the recovery frame itself is lost. Fire-and-forget; throttle like keyframe.
    pub fn request_rfi(&self, first_frame: u32, last_frame: u32) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Rfi(RfiRequest {
                first_frame,
                last_frame,
            }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Feed each received AU's `frame_index` (receive order). A forward gap fires a throttled
    /// [`request_rfi`](Self::request_rfi) for `[first_missing, frame_index-1]`. Call every frame;
    /// [`frames_dropped`](Self::frames_dropped) + [`request_keyframe`](Self::request_keyframe)
    /// stays the backstop when the recovery frame is lost.
    ///
    /// Returns gap width (`0` if none), even when RFI was throttled, so a freeze can re-arm
    /// and pre-credit the later `frames_dropped` climb
    /// ([`crate::reanchor::ReanchorGate::arm_expecting_drops`]). Without the credit a fast
    /// LTR-RFI lift is re-frozen by the stale climb.
    pub fn note_frame_index(&self, frame_index: u32) -> u32 {
        // Update under the lock; fire the request after releasing it.
        let (gap, ask) = self
            .rfi
            .lock()
            .unwrap()
            .observe(frame_index, Instant::now());
        match ask {
            RecoveryAsk::Rfi(first, last) => {
                let _ = self.request_rfi(first, last);
            }
            // Wider than RFI_MAX_RANGE: RFI cannot repair it; resync on a keyframe.
            RecoveryAsk::Keyframe => {
                let _ = self.request_keyframe();
            }
            RecoveryAsk::None => {}
        }
        gap
    }

    /// Unrecoverable AUs (FEC failed). Poll and [`request_keyframe`](Self::request_keyframe)
    /// on increase: infinite GOP conceals reference-missing frames, so a decode-error trigger
    /// misses them. Monotonic; compare against the last observed value.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    /// The pinned bitrate (kbps) this client could not keep up with — it shed its receive
    /// backlog repeatedly and a pin leaves nothing else to give. `0` = not so far. Latches
    /// for the session; show it to the user once with the next move (Automatic, or lower).
    pub fn unsustainable_pin_kbps(&self) -> u32 {
        self.unsustainable_pin_kbps.load(Ordering::Relaxed)
    }

    /// Parity-repaired shards (loss that never became a dropped frame). Monotonic; HUD diffs
    /// successive reads against [`frames_dropped`](Self::frames_dropped).
    pub fn fec_recovered_shards(&self) -> u64 {
        self.fec_recovered.load(Ordering::Relaxed)
    }

    /// Mic uplink counts per stage. Monotonic; HUD diffs successive reads.
    pub fn mic_stats(&self) -> MicUplinkStats {
        MicUplinkStats {
            sent: self.mic_stats.sent.load(Ordering::Relaxed),
            dropped_full: self.mic_stats.dropped_full.load(Ordering::Relaxed),
            dropped_stale: self.mic_stats.dropped_stale.load(Ordering::Relaxed),
        }
    }

    /// QUIC session ended (`conn.closed()`, [`disconnect_quit`](Self::disconnect_quit), or drop).
    /// Once true, every `next_*` plane returns [`PunktfunkError::Closed`]. Poll-friendly
    /// counterpart to catching `Closed` in a plane loop.
    pub fn is_session_ended(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Why the session ended — see [`PunktfunkEndReason`].
    ///
    /// Refinement of [`is_session_ended`](Self::is_session_ended), never a substitute: stays
    /// [`PunktfunkEndReason::None`] until that is true. Latches through teardown.
    pub fn end_reason(&self) -> PunktfunkEndReason {
        PunktfunkEndReason::from_u8(self.end_reason.load(Ordering::SeqCst))
    }

    pub fn ended_because_game_exited(&self) -> bool {
        self.end_reason() == PunktfunkEndReason::GameExited
    }

    /// Mid-session [`crate::reject::RejectReason`], if any. Access expiry (`0x69`) would
    /// otherwise file as `HostError`. Latches with `end_reason`. Connect-time rejections
    /// are [`PunktfunkError::Rejected`] from [`connect`](Self::connect).
    pub fn end_reject(&self) -> Option<crate::reject::RejectReason> {
        crate::reject::RejectReason::from_close_code(self.end_reject_code.load(Ordering::SeqCst))
    }

    /// What the host said about that close, in its own words — already stripped of
    /// control characters and capped ([`worker::sanitize_reason`]).
    ///
    /// `None` whenever the host sent no sentence, which is most closes: render
    /// [`end_reject`](Self::end_reject)'s own wording then. A host names what only it
    /// can know, a mis-set capture monitor being the case this exists for.
    pub fn end_reject_said(&self) -> Option<&str> {
        self.end_reject_said.get().map(String::as_str)
    }

    /// Fold the calling thread into [`hot_thread_ids`](Self::hot_thread_ids) (decode/audio
    /// with the pump). Idempotent; no-op without `gettid`.
    pub fn register_hot_thread(&self) {
        register_hot_tid(&self.hot_tids);
    }

    /// Pump tid plus [`register_hot_thread`](Self::register_hot_thread) ids. Android ADPF.
    /// Empty without `gettid`. Call after the first frame so the pump has registered.
    pub fn hot_thread_ids(&self) -> Vec<i32> {
        self.hot_tids.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Live host−client offset (ns). Re-syncs every 60 s and on a suspected wall-clock step.
    /// Prefer over connect-time [`clock_offset_ns`](Self::clock_offset_ns): NTP/drift silently
    /// corrupts capture-clock math. `0` = old host / synced clocks.
    pub fn clock_offset_now_ns(&self) -> i64 {
        self.clock_offset.load(Ordering::Relaxed)
    }

    /// Live offset for plane threads that outlive `&self`. Load Relaxed each use; never cache
    /// across frames. Holding this does not keep the session alive (unlike `Arc<NativeClient>`).
    pub fn clock_offset_shared(&self) -> Arc<AtomicI64> {
        self.clock_offset.clone()
    }

    /// Video e2e latency cell (ns, `0` = nothing presented). Presenter writes; audio reads.
    pub fn video_e2e_shared(&self) -> Arc<AtomicU64> {
        self.video_e2e_ns.clone()
    }

    /// Smoothed A/V offset cell (ms, positive = audio late). Audio writes; HUD reads.
    pub fn audio_av_offset_shared(&self) -> Arc<AtomicI64> {
        self.audio_av_offset_ms.clone()
    }

    /// Last measured A/V offset (ms). Positive = audio late. `0` before evidence or when off.
    pub fn audio_av_offset_ms(&self) -> i64 {
        self.audio_av_offset_ms.load(Ordering::Relaxed)
    }

    pub fn audio_buffer_ms_shared(&self) -> Arc<AtomicU32> {
        self.audio_buffer_ms.clone()
    }

    /// The stats overlay window. A client notes its decode and display stamps here.
    pub fn hud(&self) -> &crate::hud::Stats {
        &self.hud
    }

    /// The window as an owned handle, for a thread that must not keep the connector alive.
    pub fn hud_shared(&self) -> Arc<crate::hud::Stats> {
        self.hud.clone()
    }

    /// Smoothed QUIC round trip, µs. `0` until the worker's first sample.
    pub fn rtt_us(&self) -> u32 {
        self.rtt_us.load(Ordering::Relaxed)
    }

    fn hud_counters(&self) -> crate::hud::Counters {
        let mic = self.mic_stats();
        crate::hud::Counters {
            frames_dropped: self.frames_dropped(),
            fec_recovered: self.fec_recovered_shards(),
            mic_sent: mic.sent,
            mic_dropped: mic.dropped_full + mic.dropped_stale,
            audio_buffer_ms: self.audio_buffer_ms(),
            av_offset_ms: self
                .audio_av_offset_ms()
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            rtt_us: self.rtt_us(),
            target_kbps: self.current_bitrate_kbps(),
            pad_slots: self.pad_slots(),
        }
    }

    /// Sample for the overlay only while one is shown. On from connect.
    pub fn set_hud_enabled(&self, on: bool) {
        self.hud.set_enabled(on, &self.hud_counters());
    }

    /// Close the overlay window with everything the connector knows filled in: mode, codec,
    /// colour, audio format, counters. The caller adds the decoder, display HDR and extras.
    pub fn hud_snapshot(&self) -> crate::hud::StatsSnapshot {
        let mut s = self.hud.drain(&self.hud_counters());
        let m = self.mode();
        (s.width, s.height, s.refresh_hz) = (m.width, m.height, m.refresh_hz);
        s.codec = crate::hud::codec_label(self.codec).into();
        s.bit_depth = self.bit_depth;
        if matches!(
            self.color.transfer,
            crate::quic::ColorInfo::TRC_PQ | crate::quic::ColorInfo::TRC_HLG
        ) {
            s.hdr = crate::hud::Hdr::Hdr;
        }
        s.chroma_444 = self.chroma_format == crate::quic::CHROMA_IDC_444;
        s.auto_rate = self.wants_decode;
        s.audio_lossless = self.audio_codec == crate::quic::AUDIO_CODEC_PCM;
        s.audio_rate_hz = self.audio_sample_rate_hz;
        s.audio_bits = self.audio_bits;
        s.audio_channels = self.audio_channels;
        s
    }

    pub fn audio_buffer_ms(&self) -> u32 {
        self.audio_buffer_ms.load(Ordering::Relaxed)
    }

    /// Decode-stage latency (µs): AU leaving [`next_frame`](Self::next_frame) to decoder output.
    /// Measure from handoff, not the codec-queue call (includes input backpressure); exclude
    /// vsync wait. Feeds Automatic ABR so rate caps at the decoder, not the link. Call every
    /// frame; ignored when Automatic is off; pump drains each window so the acc stays bounded.
    pub fn report_decode_us(&self, us: u32) {
        let mut acc = self.decode_lat.lock().unwrap();
        acc.sum_us += us as u64;
        acc.count += 1;
    }

    /// Whether [`report_decode_us`](Self::report_decode_us) is used (Automatic, non-PyroWave).
    /// Constant for the session.
    pub fn wants_decode_latency(&self) -> bool {
        self.wants_decode
    }

    /// Live encoder target (kbps), follows `BitrateChanged`. [`resolved_bitrate_kbps`] is the
    /// frozen session-start value. `0` = old host that never reported one.
    /// The ABR windows that closed since the last call, oldest first.
    ///
    /// The controller's own record of what it judged and asked for, not a
    /// re-derivation. The queue holds [`ABR_TRAJECTORY_WINDOWS`] and sheds the
    /// oldest, so an embedder that never calls this costs a few kilobytes.
    pub fn take_abr_windows(&self) -> Vec<crate::abr::WindowRecord> {
        self.abr_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub fn current_bitrate_kbps(&self) -> u32 {
        self.live_bitrate_kbps.load(Ordering::Relaxed)
    }

    /// Display-latch grid for host capture phase-lock (~1 Hz). `next_latch_host_ns` must
    /// already be host clock (`T_host = T_client +` [`clock_offset_now_ns`](Self::clock_offset_now_ns)).
    /// Fire-and-forget; a full queue drops (next report supersedes).
    pub fn report_phase(
        &self,
        next_latch_host_ns: u64,
        latch_period_ns: u32,
        uncertainty_ns: u32,
        arrival_lead_ns: u32,
        coherence_milli: u16,
    ) {
        let _ = self
            .ctrl_tx
            .try_send(CtrlRequest::Phase(crate::quic::PhaseReport {
                next_latch_host_ns,
                latch_period_ns,
                uncertainty_ns,
                arrival_lead_ns,
                coherence_milli,
            }));
    }

    /// Burst filler at `target_kbps` for `duration_ms`, pausing video. Non-blocking; poll
    /// [`NativeClient::probe_result`] until `done`. Resets any prior measurement. Host clamps
    /// ≤ 10 Gbps, ≤ 5 s.
    pub fn request_probe(&self, target_kbps: u32, duration_ms: u32) -> Result<()> {
        *self.probe.lock().unwrap() = ProbeState {
            active: true,
            duration_ms,
            ..Default::default()
        };
        let sent = self
            .ctrl_tx
            .try_send(CtrlRequest::Probe(ProbeRequest {
                target_kbps,
                duration_ms,
            }))
            .map_err(|_| PunktfunkError::Closed);
        if sent.is_err() {
            // Send failed: nothing will answer. Leaving `active` would suppress the pump's
            // report tick for the rest of the session.
            self.probe.lock().unwrap().active = false;
        }
        sent
    }

    /// Whether a burst is in flight — an embedder speed test or the startup capacity probe. Loss
    /// inside it is the burst's own doing on a link it exceeds, so a "connection issues" notice
    /// gated on this stays quiet for it.
    pub fn probe_active(&self) -> bool {
        self.probe.lock().unwrap().active
    }

    /// Speed-test measurement: partial until `done`, then the host's end-of-burst report.
    pub fn probe_result(&self) -> ProbeOutcome {
        let p = self.probe.lock().unwrap();
        // Live (rx_now − base) while bursting; frozen once the host report lands.
        let (delivered_packets, delivered_bytes) = if p.done {
            (p.delivered_packets, p.delivered_bytes)
        } else {
            let base_p = p.base_packets.unwrap_or(p.rx_packets_now);
            let base_b = p.base_bytes.unwrap_or(p.rx_bytes_now);
            (
                p.rx_packets_now.saturating_sub(base_p),
                p.rx_bytes_now.saturating_sub(base_b),
            )
        };
        // Client-measured receive interval, live while bursting, else host send-window (host
        // window alone overstates the link).
        let window_ms = p.throughput_window_ms(delivered_packets);
        let throughput_kbps = if window_ms > 0 {
            (delivered_bytes.saturating_mul(8) / window_ms as u64) as u32
        } else {
            0
        };
        // Packet-level loss: degrades past the FEC budget instead of cliffing to 100% when AUs stop.
        let loss_pct = if p.host_wire_packets > 0 {
            (p.host_wire_packets as i64 - delivered_packets as i64).max(0) as f64
                / p.host_wire_packets as f64
                * 100.0
        } else {
            0.0
        } as f32;
        // Send-buffer refusals. Saturating: a hostile wire sum must not overflow-panic debug.
        let offered_wire = p.host_wire_packets.saturating_add(p.host_send_dropped);
        let host_drop_pct = if offered_wire > 0 {
            p.host_send_dropped as f64 / offered_wire as f64 * 100.0
        } else {
            0.0
        } as f32;
        ProbeOutcome {
            done: p.done,
            recv_bytes: delivered_bytes,
            recv_packets: delivered_packets as u32,
            host_bytes: p.host_goodput_bytes,
            host_packets: p.host_au,
            elapsed_ms: window_ms,
            throughput_kbps,
            loss_pct,
            host_drop_pct,
            wire_packets_sent: p.host_wire_packets,
            send_dropped: p.host_send_dropped,
        }
    }

    /// Next FEC-recovered AU. [`PunktfunkError::NoFrame`] on timeout, `Closed` once ended.
    /// One thread per plane; `&self` is for sharing across planes, not two consumers of one.
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        match self.frames.pop(timeout) {
            FramePop::Frame(f) => {
                let completes_au = f.part.as_ref().is_none_or(|p| p.last);
                self.hud
                    .note_received(f.pts_ns, f.received_ns, f.data.len(), completes_au);
                Ok(f)
            }
            FramePop::Timeout => Err(PunktfunkError::NoFrame),
            FramePop::Closed => Err(PunktfunkError::Closed),
        }
    }

    /// Next audio packet. Drain on a dedicated thread — packets arrive every 5 ms.
    pub fn next_audio(&self, timeout: Duration) -> Result<AudioPacket> {
        match self.audio.lock().unwrap().recv_timeout(timeout) {
            Ok(p) => Ok(p),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Mute this client's own speakers. Nothing leaves for the host: it keeps encoding, and a
    /// session joined to the same sink keeps hearing the game. Packets keep arriving and keep
    /// decoding — the embedder zeroes only what it queues for the device — so the decoder never
    /// loses its state and unmute lands in step. Leaves [`AUDIO_MUTE_HOST`] alone.
    pub fn set_audio_muted(&self, muted: bool) {
        set_mute_bit(&self.audio_mute, AUDIO_MUTE_LOCAL, muted);
    }

    /// Why this session is silent: [`AUDIO_MUTE_LOCAL`], [`AUDIO_MUTE_HOST`], both, or `0`.
    /// The overlay names the reason from this; [`audio_mute_label`] is the shared wording.
    pub fn audio_mute(&self) -> u8 {
        self.audio_mute.load(Ordering::Relaxed)
    }

    /// Either reason silences the speakers. Zero the decoded frame on this; show
    /// [`audio_mute`](Self::audio_mute) to say whose mute it is.
    pub fn audio_muted(&self) -> bool {
        self.audio_mute() != 0
    }

    /// OS pad slots the host gave this session, one bit each: bit `n` = player
    /// `n + 1`. `0` before a pad of ours has a device, or on a host too old to
    /// send [`crate::quic::PadSlots`]. [`crate::hud::player_label`] is the wording.
    pub fn pad_slots(&self) -> u16 {
        self.pad_slots.load(Ordering::Relaxed)
    }

    /// What became of this session's library launch, latest verdict first. `None` before the
    /// host sends one, on a session that launched nothing, and on a host too old to say.
    /// [`crate::quic::LaunchOutcome::notice`] is the line to show.
    pub fn launch_outcome(&self) -> Option<crate::quic::LaunchOutcome> {
        self.launch_outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `(pad, low, high)`; TTL of a v2 envelope is dropped. Use
    /// [`NativeClient::next_rumble_ttl`] to honor it. `(0, 0)` = stop.
    pub fn next_rumble(&self, timeout: Duration) -> Result<(u16, u16, u16)> {
        self.next_rumble_ttl(timeout).map(|(p, l, h, _)| (p, l, h))
    }

    /// `(pad, low, high, ttl_ms)`. `Some(ms)` = v2 lease; `None` = v1, use the renderer's
    /// staleness heuristic. Reorder gate is applied in demux; stale envelopes never surface.
    pub fn next_rumble_ttl(&self, timeout: Duration) -> Result<RumbleUpdate> {
        match self.rumble.lock().unwrap().recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Policy-engine command: level on every wire update, explicit zero at expiry/staleness/
    /// close, plus quirk keepalives ([`NativeClient::set_rumble_quirks`]). No TTL to own.
    /// All-zero = stop; non-zero = run, `backstop_ms` for APIs that take a duration.
    /// `Closed` only after every close-drain stop was delivered.
    ///
    /// Four levels (two handle + two impulse triggers). Render triggers only on a pad that
    /// has them; do not fold them into a handle ([`RumbleCommand`]). Use this OR
    /// `next_rumble`/`next_rumble_ttl` for the connection, never both.
    pub fn next_rumble_command(&self, timeout: Duration) -> Result<RumbleCommand> {
        match self.rumble_sched.next_command(timeout) {
            Ok(Some(c)) => Ok(c),
            Ok(None) => Err(PunktfunkError::NoFrame),
            Err(rumble::Closed) => Err(PunktfunkError::Closed),
        }
    }

    /// Actuator quirks for wire pad `pad` (at attach). Default = well-behaved; only decaying
    /// actuators need a keepalive.
    pub fn set_rumble_quirks(&self, pad: u16, quirks: ActuatorQuirks) {
        self.rumble_sched.set_quirks(pad, quirks);
    }

    /// DualSense HID-output (lightbar / LEDs / adaptive trigger). DualSense host backend only.
    pub fn next_hidout(&self, timeout: Duration) -> Result<HidOutput> {
        match self.hidout.lock().unwrap().recv_timeout(timeout) {
            Ok(h) => Ok(h),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pad-audio Opus (haptics 5 ms / speaker 10 ms). Shared queue; fan out by `pad`/`kind`.
    /// `None` on timeout and on end ([`is_session_ended`](Self::is_session_ended) distinguishes).
    /// Empty unless [`quic::CLIENT_CAP_PAD_AUDIO`] met [`quic::HOST_CAP_PAD_AUDIO`] and
    /// [`set_pad_audio_caps`](Self::set_pad_audio_caps) declared the pad.
    pub fn next_pad_audio(&self, timeout: Duration) -> Option<PadAudioFrame> {
        self.pad_audio.lock().unwrap().recv_timeout(timeout).ok()
    }

    /// Pad-audio render caps: bit0 haptics, bit1 speaker. Call at attach, before arrival —
    /// worker ORs bits 8/9 toward a [`quic::HOST_CAP_PAD_AUDIO`] host only. Never calling
    /// this leaves the wire unchanged. Latest-wins; unknown bits masked.
    pub fn set_pad_audio_caps(&self, pad: u8, audio_caps: u8) {
        if let Some(slot) = self.pad_audio_caps.get(pad as usize) {
            slot.store(audio_caps & 0x03, Ordering::Relaxed);
        }
    }

    /// ST.2086 mastering + CLL. Host sends at start and on mastering/keyframe changes. HDR
    /// (`color.is_hdr()`, PQ) only; drain on its own thread and apply the latest.
    pub fn next_hdr_meta(&self, timeout: Duration) -> Result<HdrMeta> {
        match self.hdr_meta.lock().unwrap().recv_timeout(timeout) {
            Ok(m) => Ok(m),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// RGBA cursor bitmap + hotspot, on pointer-bitmap change. Cache by `serial`;
    /// [`NativeClient::next_cursor_state`] references it. Empty unless
    /// [`crate::quic::CLIENT_CAP_CURSOR`] was advertised against a capable host.
    pub fn next_cursor_shape(&self, timeout: Duration) -> Result<crate::quic::CursorShape> {
        match self.cursor_shape.lock().unwrap().recv_timeout(timeout) {
            Ok(s) => Ok(s),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Per-frame cursor state (`0xD0`): position, visibility, relative-mode hint. Latest-wins
    /// — drain and apply only the newest. Same gate as [`NativeClient::next_cursor_shape`].
    pub fn next_cursor_state(&self, timeout: Duration) -> Result<crate::quic::CursorState> {
        match self.cursor_state.lock().unwrap().recv_timeout(timeout) {
            Ok(s) => Ok(s),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Per-AU capture→sent (`pts_ns`). HUD split: `network = (received + clock_offset − pts)
    /// − host_us`. Older host never sends any — keep combined `host+network`. Drain
    /// non-blockingly alongside frame samples.
    pub fn next_host_timing(&self, timeout: Duration) -> Result<crate::quic::HostTiming> {
        match self.host_timing.lock().unwrap().recv_timeout(timeout) {
            Ok(t) => {
                self.hud.note_host_timing(&t);
                Ok(t)
            }
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        self.input_tx.send(*ev).map_err(|_| PunktfunkError::Closed)
    }

    /// Welcome [`crate::quic::HOST_CAP_GAMEPAD_STATE`] / [`crate::quic::HOST_CAP_CLIPBOARD`].
    pub fn host_caps(&self) -> u8 {
        self.host_caps
    }

    /// [`crate::quic::HOST_CAP2_TOUCH`] and kin. `0` from an older host.
    pub fn host_caps2(&self) -> u8 {
        self.host_caps2
    }

    /// Host management-API port from Welcome. `0` if unadvertised — keep the caller's default
    /// (do not assume 47990). Arrives over the already-authenticated connection, so VPN/IP
    /// hosts need no mDNS.
    pub fn mgmt_port(&self) -> u16 {
        self.mgmt_port
    }

    /// Live grants ([`crate::quic::GRANT_GAMEPAD`] family). Welcome seed, latest
    /// [`crate::quic::AccessUpdate`] wins. Old host → [`crate::quic::GRANT_ALL`]. Courtesy:
    /// the host enforces. Load per use; never cache across
    /// [`next_access_update`](Self::next_access_update).
    pub fn access_grants(&self) -> u32 {
        self.access_grants.load(Ordering::Relaxed)
    }

    /// Access expiry as client-wall unix seconds. `None` = permanent (old host). Anchored
    /// from relative wire seconds so skew cannot move a countdown; re-anchored on update.
    pub fn access_deadline_unix(&self) -> Option<u64> {
        match self.access_deadline_unix.load(Ordering::Relaxed) {
            0 => None,
            d => Some(d),
        }
    }

    /// Mid-session [`crate::quic::AccessUpdate`] (console edit, T−5/T−1 expiry). Wake-up
    /// only: truth is already in [`access_grants`](Self::access_grants) /
    /// [`access_deadline_unix`](Self::access_deadline_unix).
    pub fn next_access_update(&self, timeout: Duration) -> Result<crate::quic::AccessUpdate> {
        match self.access.lock().unwrap().recv_timeout(timeout) {
            Ok(u) => Ok(u),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Opt-in clipboard. Nothing is announced until `enabled = true`. `flags` carries
    /// [`crate::quic::CLIP_FLAG_FILES`]. Host replies with a `State` ([`NativeClient::next_clip`]).
    pub fn clip_control(&self, enabled: bool, flags: u8) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::ClipControl(ClipControl { enabled, flags }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Lazy format-list offer. `seq` newest-wins; `kinds` ≤ [`crate::quic::CLIP_MAX_KINDS`].
    /// Bytes cross only if the host later fetches.
    pub fn clip_offer(&self, seq: u32, kinds: Vec<ClipKind>) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::ClipOffer(ClipOffer { seq, kinds }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Pull one format of host offer `seq`. [`crate::quic::CLIP_FILE_INDEX_NONE`] for non-file.
    /// Returns the `xfer_id` echoed on `Data` / `Error` / `Cancelled`.
    pub fn clip_fetch(&self, seq: u32, mime: String, file_index: u32) -> Result<u32> {
        let xfer_id = self.next_xfer_id.fetch_add(1, Ordering::Relaxed);
        // Low id space: inbound serve ids carry the high bit. Wrap defensively.
        let xfer_id = xfer_id & !crate::clipboard::INBOUND_REQ_FLAG;
        self.clip_cmd_tx
            .send(ClipCommand::Fetch {
                xfer_id,
                seq,
                file_index,
                mime,
            })
            .map_err(|_| PunktfunkError::Closed)?;
        Ok(xfer_id)
    }

    /// Answer a `FetchRequest`. Repeat to stream; `last = true` completes. `clip_cancel` aborts.
    pub fn clip_serve(&self, req_id: u32, bytes: Vec<u8>, last: bool) -> Result<()> {
        self.clip_cmd_tx
            .send(ClipCommand::Serve {
                req_id,
                bytes,
                last,
            })
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Cancel outbound fetch (`xfer_id`) or inbound serve (`req_id`).
    pub fn clip_cancel(&self, id: u32) -> Result<()> {
        self.clip_cmd_tx
            .send(ClipCommand::Cancel { id })
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Clipboard events (offer, state, fetch-request, data, cancel, error). Drain on its own
    /// thread onto the OS pasteboard.
    pub fn next_clip(&self, timeout: Duration) -> Result<ClipEventCore> {
        match self.clip.lock().unwrap().recv_timeout(timeout) {
            Ok(e) => Ok(e),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Opus mic uplink (0xCB). `seq`/`pts_ns` are caller diagnostics. Best-effort; no retransmit.
    pub fn send_mic(&self, seq: u32, pts_ns: u64, opus: Vec<u8>) -> Result<()> {
        use tokio::sync::mpsc::error::TrySendError;
        match self.mic_tx.try_send((seq, pts_ns, opus)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                // Worker outran the pump's oldest-first shed. Drop (best-effort); counter visible.
                self.mic_stats.dropped_full.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("mic uplink queue full — dropping frame");
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err(PunktfunkError::Closed),
        }
    }

    /// Switch the pads in `mask` (bit = wire pad index) to controller mouse: their buttons and
    /// sticks drive the host pointer and a few keys while the host pad sits neutral. The rest
    /// forward as usual. Session-scoped; a removed pad drops its bit. Needs
    /// [`GRANT_POINTER`](crate::quic::GRANT_POINTER), and losing it clears the mask.
    pub fn set_pad_mouse(&self, mask: u16) -> Result<()> {
        if mask != 0 && self.access_grants() & crate::quic::GRANT_POINTER == 0 {
            return Err(PunktfunkError::Unsupported(
                "host did not grant pointer input",
            ));
        }
        self.pad_mouse.request(mask);
        Ok(())
    }

    /// Replace the controller-mouse layout — plain buttons, chords, and the pointer, scroll,
    /// deadzone and long-press tunables — from a JSON document. `None` restores the shipped
    /// table. A pad already in controller mouse keeps the layout it entered with.
    pub fn set_pad_mouse_layout(&self, doc: Option<&str>) -> Result<()> {
        let layout = match doc {
            Some(json) => pad_mouse::Layout::parse(json)?,
            None => pad_mouse::Layout::default(),
        };
        self.pad_mouse.set_layout(layout);
        Ok(())
    }

    /// Pads the embedder switched to controller mouse and that are still connected.
    pub fn pad_mouse(&self) -> u16 {
        self.pad_mouse.active(self.access_grants())
    }

    /// Wire pad indices the host holds right now: declared or driven, not yet removed.
    pub fn live_pads(&self) -> u16 {
        self.pad_mouse.live()
    }

    /// DualSense touchpad/motion (0xCC). Best-effort. No-op unless the host runs DualSense.
    /// Dropped for a controller-mouse pad, so its gyro cannot aim the neutral host pad.
    pub fn send_rich_input(&self, rich: RichInput) -> Result<()> {
        if self.pad_mouse() & 1u16.checked_shl(u32::from(rich.pad())).unwrap_or(0) != 0 {
            return Ok(());
        }
        self.rich_input_tx
            .send(rich.encode())
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Stylus batch (`0xCC/0x05`). State-full, oldest-first, ≤ [`crate::quic::PEN_BATCH_MAX`]
    /// — split longer runs so wrapping `seq` stays ordered. Lost batches self-heal (host diffs
    /// full state, [`crate::quic::PenTracker`]).
    ///
    /// Heartbeat: while in range or touching, repeat the last sample every ~100 ms even when
    /// still — capture is silent for a stationary pen, and the host force-releases after
    /// [`crate::quic::PEN_TOUCH_TIMEOUT_MS`]. Without [`crate::quic::HOST_CAP_PEN`] this
    /// returns `Unsupported` so embedders keep pen-as-touch instead of spraying 240 Hz unread.
    pub fn send_pen(&self, samples: &[crate::quic::PenSample]) -> Result<()> {
        if self.host_caps & crate::quic::HOST_CAP_PEN == 0 {
            return Err(PunktfunkError::Unsupported(
                "host did not advertise HOST_CAP_PEN",
            ));
        }
        if samples.is_empty() || samples.len() > crate::quic::PEN_BATCH_MAX {
            return Err(PunktfunkError::InvalidArg(
                "pen batch must hold 1..=PEN_BATCH_MAX samples",
            ));
        }
        let seq = self.pen_seq.fetch_add(1, Ordering::Relaxed);
        self.rich_input_tx
            .send(crate::quic::PenBatch::new(seq, samples).encode())
            .map_err(|_| PunktfunkError::Closed)
    }

    /// User stop: close with [`crate::quic::QUIT_CLOSE_CODE`] so the host skips keep-alive
    /// linger. A plain drop closes with code 0 and the host waits for reconnect.
    pub fn disconnect_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// `PUNKTFUNK_CLIENT_PEAK_NITS=<nits>` synthesizes [`Hello::display_hdr`](crate::quic::Hello::display_hdr)
/// at that peak (BT.2020, D65, 0.005-nit floor) so EDID tone-map can be pinned. `None` if
/// unset/unparsable/zero.
pub fn display_hdr_env_override() -> Option<HdrMeta> {
    let nits: u32 = std::env::var("PUNKTFUNK_CLIENT_PEAK_NITS")
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&n| n > 0)?;
    tracing::info!(
        nits,
        "PUNKTFUNK_CLIENT_PEAK_NITS: overriding the advertised display volume"
    );
    Some(HdrMeta {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]], // BT.2020 G, B, R
        white_point: [15635, 16450],                                      // D65
        max_display_mastering_luminance: nits.saturating_mul(10_000),
        min_display_mastering_luminance: 50, // 0.005 nits
        max_cll: 0,
        max_fall: 0,
    })
}

#[cfg(test)]
mod host_port_tests {
    use super::join_host_port;

    #[test]
    fn brackets_bare_ipv6_only() {
        assert_eq!(join_host_port("192.168.1.9", 4770), "192.168.1.9:4770");
        assert_eq!(join_host_port("myhost", 4770), "myhost:4770");
        assert_eq!(join_host_port("fd00::1", 4770), "[fd00::1]:4770");
        assert_eq!(join_host_port("[fd00::1]", 4770), "[fd00::1]:4770");
        assert!(join_host_port("fd00::1", 4770)
            .parse::<std::net::SocketAddr>()
            .is_ok());
    }
}

#[cfg(test)]
mod client_caps_tests {
    use super::advertised_client_caps;
    use crate::audio::pcm::{BITS_16, BITS_24};
    use crate::audio::SAMPLE_RATE_HZ;
    use crate::quic::{CLIENT_CAP_AUDIO_HIRES, CLIENT_CAP_AUDIO_RED, CLIENT_CAP_CURSOR};

    /// RED is unconditional; HIRES only when a format is specified. A miss still works, it
    /// just spends 1.5–4.6 Mbps nobody asked for.
    #[test]
    fn hires_is_advertised_only_when_the_caller_specified_a_format() {
        // 0/0 = unspecified: RED on, HIRES off, embedder bits untouched. What `connect` passes.
        let legacy = advertised_client_caps(CLIENT_CAP_CURSOR, 0, 0);
        assert_eq!(legacy & CLIENT_CAP_AUDIO_RED, CLIENT_CAP_AUDIO_RED);
        assert_eq!(legacy & CLIENT_CAP_AUDIO_HIRES, 0);
        assert_eq!(legacy & CLIENT_CAP_CURSOR, CLIENT_CAP_CURSOR);
        assert_eq!(advertised_client_caps(0, 0, 0), CLIENT_CAP_AUDIO_RED);

        // 48 kHz/16-bit is both default and cheapest lossless; explicit must still set HIRES.
        assert_eq!(
            advertised_client_caps(0, SAMPLE_RATE_HZ, BITS_16) & CLIENT_CAP_AUDIO_HIRES,
            CLIENT_CAP_AUDIO_HIRES,
            "explicit 48 kHz/16-bit is a lossless request, not a legacy one"
        );

        // Either half non-zero is still a request.
        for (rate, bits) in [
            (SAMPLE_RATE_HZ, BITS_24),
            (96_000, BITS_16),
            (96_000, BITS_24),
            (0, BITS_24),
            (96_000, 0),
        ] {
            let caps = advertised_client_caps(0, rate, bits);
            assert_eq!(
                caps & CLIENT_CAP_AUDIO_HIRES,
                CLIENT_CAP_AUDIO_HIRES,
                "{rate} Hz / {bits}-bit must ask for the lossless plane"
            );
            assert_eq!(caps & CLIENT_CAP_AUDIO_RED, CLIENT_CAP_AUDIO_RED);
        }

        // Escape: 48/16 looks like legacy, so the caller sets HIRES itself and is not overridden.
        let explicit = advertised_client_caps(CLIENT_CAP_AUDIO_HIRES, SAMPLE_RATE_HZ, BITS_16);
        assert_eq!(explicit & CLIENT_CAP_AUDIO_HIRES, CLIENT_CAP_AUDIO_HIRES);
    }
}

#[cfg(test)]
mod mute_tests {
    use super::*;

    /// The player's mute and the operator's are separate bits: each unmutes on its own, and
    /// while either stands the overlay says which one it is.
    #[test]
    fn a_local_mute_and_a_host_mute_never_stand_in_for_each_other() {
        let m = AtomicU8::new(0);
        assert_eq!(audio_mute_label(m.load(Ordering::Relaxed)), None);

        set_mute_bit(&m, AUDIO_MUTE_LOCAL, true);
        assert_eq!(
            audio_mute_label(m.load(Ordering::Relaxed)),
            Some("Muted on this device")
        );

        set_mute_bit(&m, AUDIO_MUTE_HOST, true);
        assert_eq!(
            audio_mute_label(m.load(Ordering::Relaxed)),
            Some("Muted by the host and on this device")
        );

        // Unmuting locally must not claim the player can hear again.
        set_mute_bit(&m, AUDIO_MUTE_LOCAL, false);
        assert_eq!(
            audio_mute_label(m.load(Ordering::Relaxed)),
            Some("Muted by the host")
        );

        set_mute_bit(&m, AUDIO_MUTE_HOST, false);
        assert_eq!(audio_mute_label(m.load(Ordering::Relaxed)), None);
    }
}
