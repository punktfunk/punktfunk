//! Embeddable `punktfunk/2` client connector, behind the `quic` feature.
//!
//! [`NativeClient::connect`] runs QUIC handshake ([`crate::quic`]), the data plane
//! ([`crate::session::Session`] on a native thread), and input datagrams. The surface is
//! pull reassembled access units, push input. Platform clients link via the C ABI
//! (`punktfunk_connect` in `punktfunk-ffi`); `punktfunk-probe` is the Rust-native consumer.
//!
//! One worker owns a tokio runtime (QUIC control plane only) plus a blocking data-plane
//! pump. Frames cross to the embedder on a bounded channel. Methods are safe from any
//! single embedder thread.

// Carve-out: thread ids and QoS pins, each with a `// SAFETY:` proof.
// Host code never runs this module.
#![allow(unsafe_code)]

use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::{PunktfunkError, Result};
use crate::input::{InputEvent, InputKind, PadMouseMode};
use crate::quic::{
    endpoint, ClipControl, ClipKind, ClipOffer, ColorInfo, HdrMeta, HidOutput, PadAudioFrame,
    ProbeRequest, RichInput,
};
use crate::session::Frame;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod control;
pub(crate) mod frame_channel;
pub mod health;
pub mod mgmt;
mod pad_mouse;
mod pad_touch;
mod pairing;
mod planes;
mod probe;
mod pump;
pub(crate) mod recovery;
mod resume;
/// cbindgen:ignore
pub mod sc1_ble;
/// cbindgen:ignore
pub mod sc2;
mod screens;
use crate::rumble;
mod worker;

pub use self::planes::AudioPacket;
pub use self::probe::ProbeOutcome;
pub use crate::reanchor::{FLUSH_COOLDOWN, NO_VIDEO_RETRY};

pub use crate::audio::{
    audio_mute_label, audio_mute_notice, mic_mute_label, mic_mute_notice, AUDIO_MUTE_HOST,
    AUDIO_MUTE_LOCAL, LOCAL_MUTE_NOTICE,
};

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
use self::planes::{PlaneRx, RawRumble};
use self::probe::ProbeState;
use self::pump::run_pump;
pub use self::recovery::{Arrival, Decoded, Shown};
use self::recovery::{
    FrameOrder, RecentRfis, RecoveryAsk, RecoveryDriver, RecoveryEvent, ShortFrames,
};
pub use self::sc2::{Sc2Gate, SC2_GATE_CHORDS, SC2_GATE_MASKED, SC2_GATE_SYSTEM_LOCAL};
use self::worker::{ClientShared, WorkerArgs};

/// What this client calls itself in the host's `handshake complete` line: build plus the shell
/// and path that dialled. Process-wide because it describes the embedder, not one session; set it
/// before the dial. Empty (the default) sends no label.
static CLIENT_LABEL: Mutex<String> = Mutex::new(String::new());

/// Set the label [`ClientHello::client_label`](crate::quic::v2::hello::ClientHello::client_label)
/// carries. Bounded and stripped on the way in, so the wire never has to trust the caller.
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

pub use crate::discovery::join_host_port;

/// The address to dial: an IP literal as written, else the name's first IPv4 answer (MagicDNS,
/// `.local`). The client endpoint binds IPv4, so a v6 answer is the fallback only.
async fn dial_addr(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    if let Ok(addr) = join_host_port(host, port).parse() {
        return Ok(addr);
    }
    let mut first = None;
    for addr in tokio::net::lookup_host((host, port)).await? {
        if addr.is_ipv4() {
            return Ok(addr);
        }
        first.get_or_insert(addr);
    }
    first.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{host} has no address"),
        )
        .into()
    })
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

    /// How long to wait before redial `attempt` (0-based) of a session that ended this way;
    /// `None` once the schedule is spent, and for every ending but [`Self::Lost`]: a host
    /// that closed the session meant it. Every shell redials on this one schedule.
    pub fn redial_delay(self, attempt: u32) -> Option<Duration> {
        const SCHEDULE_S: [u64; 5] = [0, 1, 2, 4, 8];
        if self != Self::Lost {
            return None;
        }
        SCHEDULE_S
            .get(attempt as usize)
            .map(|&s| Duration::from_secs(s))
    }
}

#[cfg(feature = "quic")]
impl From<&quinn::ConnectionError> for PunktfunkEndReason {
    /// Map a QUIC close. Host app codes: `APP_EXITED` → GameExited, `PATH_SHRANK` → Lost
    /// (a redial heals it), `0` → HostEnded. Any other application code is unnamed and
    /// treated as `HostError` (visible) rather than a clean host end. Transport-level
    /// failures are `Lost`.
    fn from(e: &quinn::ConnectionError) -> Self {
        match e {
            quinn::ConnectionError::LocallyClosed => Self::Local,
            quinn::ConnectionError::ApplicationClosed(ac) => {
                match u32::try_from(u64::from(ac.error_code)) {
                    Ok(crate::quic::APP_EXITED_CLOSE_CODE) => Self::GameExited,
                    Ok(crate::quic::PATH_SHRANK_CLOSE_CODE) => Self::Lost,
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
    /// Cells the worker writes and this handle reads, or the other way round.
    shared: Arc<ClientShared>,
    /// The host-filled planes, one receiver each.
    planes: PlaneRx,
    /// Newest entry [`NativeClient::latest_hdr_meta`] drained.
    hdr_meta_last: Mutex<Option<HdrMeta>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// The connect, and the player's last input in ms after it
    /// ([`InputEvent::is_activity`]). The connect counts as input: the player just asked.
    born: Instant,
    last_input_ms: AtomicU64,
    /// Bounded ([`MIC_QUEUE`]): pump sheds oldest-first; a full queue drops the fresh frame.
    /// Standing backlog is worse than a dropout.
    mic_tx: tokio::sync::mpsc::Sender<(u32, u64, Vec<u8>)>,
    /// Pre-encoded 0xCC bytes ([`RichInput`] and [`crate::quic::PenBatch`]). Worker forwards;
    /// a new 0xCC kind never touches the pump.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Touchpad contacts of controller-mouse pads, for the input task's pointer.
    pad_touch_tx: tokio::sync::mpsc::UnboundedSender<pad_touch::Contact>,
    /// Per wire pad: what of a Steam Controller 2's raw reports reaches the host.
    sc2: Mutex<[sc2::Filter; sc2::PADS]>,
    /// Bounded ([`CTRL_QUEUE`]). Sparse; full means the control task is wedged — treat as closed.
    ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Unbounded like `input_tx`; sparse, at most one paste's bytes each.
    clip_cmd_tx: tokio::sync::mpsc::UnboundedSender<ClipCommand>,
    /// Outbound fetch ids. Stay below [`crate::clipboard::INBOUND_REQ_FLAG`] or they collide
    /// with inbound serve `req_id`.
    next_xfer_id: AtomicU32,
    /// Wrapping [`crate::quic::PenBatch::seq`]; the host's reorder gate compares it.
    pen_seq: AtomicU16,
    /// The host's offer. The pub fields below are copies of it, read by embedders.
    welcome: crate::quic::Welcome,
    pub host_caps: u8,
    pub host_caps2: u8,
    /// `0` when the host did not advertise a management port.
    pub mgmt_port: u16,
    /// The profile the host resolved this session to; `None` from a host without profiles.
    profile: Option<String>,
    /// `displayed + clock_offset − pts` (ns). `0` = nothing presented yet. Presenter writes;
    /// audio reads to land with the picture ([`crate::audio::AvSync`]). Lives next to
    /// `clock_offset` because neither plane owns the other.
    video_e2e_ns: Arc<AtomicU64>,
    /// Smoothed A/V offset (ms); positive = audio late. Audio writes, HUD reads.
    audio_av_offset_ms: Arc<AtomicI64>,
    /// Playback-ring depth (ms). Audio writes, HUD reads.
    audio_buffer_ms: Arc<AtomicU32>,
    /// The stats overlay window. Receipt and 0xCF timings land in it as they are pulled.
    hud: Arc<crate::hud::Stats>,
    /// ABR armed (Automatic). Skip per-frame decode measurement when false
    /// ([`wants_decode_latency`](Self::wants_decode_latency)).
    wants_decode: bool,
    /// Hello asked for 4:4:4, so the overlay can name a refusal.
    asked_444: bool,
    worker: Option<std::thread::JoinHandle<()>>,
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
    /// Encode bit depth at connect: `8`, or `10` for Main10/HDR. `8` for an older host.
    /// [`video_format`](Self::video_format) follows it mid-session.
    pub bit_depth: u8,
    /// Host colour signalling at connect. [`ColorInfo::SDR_BT709`] for an older host. HDR
    /// mastering arrives via [`NativeClient::next_hdr_meta`]; [`video_format`](Self::video_format)
    /// follows the colour mid-session.
    pub color: ColorInfo,
    /// HEVC `chroma_format_idc` at connect ([`quic::CHROMA_IDC_420`] or
    /// [`quic::CHROMA_IDC_444`]). SPS is authoritative; this pre-sizes the decoder. `420` for an
    /// older host. [`video_format`](Self::video_format) follows it mid-session.
    pub chroma_format: u8,
    /// Host-resolved channels: `2` / `6` / `8`. Build the Opus decoder from this via
    /// [`crate::audio::layout_for`], never from the request. Omitted → `2`.
    pub audio_channels: u8,
    /// Selects the decoder: [`quic::AUDIO_CODEC_OPUS`] (`0xC9`) or [`quic::AUDIO_CODEC_PCM`]
    /// (`0xD3`). A 48 kHz/16-bit lossless session and a 48 kHz Opus session agree on every
    /// other resolved value. [`quic::AUDIO_CODEC_AC3`] / [`quic::AUDIO_CODEC_EAC3`] (`0xD4`,
    /// only on a [`ConnectParams::audio_bitstreams`] ask) go to a passthrough sink undecoded.
    /// Fixed for the session — the output device is open at one format.
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
    /// Host-resolved video codec at connect. Build the decoder from THIS; do not assume HEVC.
    /// Under [`ConnectParams::rebuilds_decoder`] it can move: [`video_format`](Self::video_format).
    pub codec: u8,
}

impl NativeClient {
    /// Payload kbps of a constant-rate plane: lossless is `rate × depth × channels` (exact),
    /// bitstream its fixed [`bitrate_kbps`](crate::audio::bitstream::bitrate_kbps). `None` for
    /// Opus (VBR, host-chosen via [`crate::audio::plan_audio_budget`]) — a short window would
    /// read as jitter. Header and QUIC framing are omitted, as with every other quoted bitrate.
    pub fn audio_kbps(&self) -> Option<u32> {
        if crate::audio::bitstream::is_bitstream(self.audio_codec) {
            return Some(crate::audio::bitstream::bitrate_kbps(
                self.audio_codec,
                self.audio_channels,
            ));
        }
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
pub(crate) fn pin_thread_user_interactive() {
    // SAFETY: sets only the current thread's QoS class — always valid to call.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
/// Nice −8 (URGENT_DISPLAY). Default 0 parks a bursty pump on a little core; a few ms of
/// delay overflows the socket recv buffer → wire loss the link never saw. Below decode's
/// −10 so the display path still wins. Best-effort.
#[cfg(target_os = "android")]
pub(crate) fn pin_thread_user_interactive() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls; a refusal is
    // reported via the return value (ignored — a missed boost, not an error on the data path).
    unsafe {
        let tid = libc::gettid();
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8);
    }
}
/// Desktop has no QoS class of its own; the embedder installs one with
/// [`set_thread_boost`] (nice via rtkit, MMCSS on Windows).
#[cfg(not(any(target_vendor = "apple", target_os = "android")))]
pub(crate) fn pin_thread_user_interactive() {
    if let Some(boost) = THREAD_BOOST.get() {
        boost();
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "android")))]
static THREAD_BOOST: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Install the call every hot client thread (UDP pump, runtime workers) makes at start
/// to raise its own priority. First caller wins; later calls are ignored.
#[cfg(not(any(target_vendor = "apple", target_os = "android")))]
pub fn set_thread_boost(boost: fn()) {
    let _ = THREAD_BOOST.set(boost);
}

/// Wall-clock now (ns, CLOCK_REALTIME) for latency math against host `pts_ns` after skew.
///
/// [`crate::audio::AvSync`] lives in an embedder crate and must use this basis: `Instant` or
/// a monotonic clock is wrong by boot time and still looks plausible.
pub fn now_realtime_ns() -> i128 {
    crate::quic::wall_clock_ns() as i128
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

/// Send an RFI and log it at info, the most Android keeps. `missing_shards` is what
/// `first_frame` lacked past its parity when its tail arrived or a later frame overtook
/// it; absent when none of it arrived.
fn send_rfi(shared: &ClientShared, first_frame: u32, last_frame: u32) -> Result<()> {
    shared.ask_rfi(first_frame, last_frame);
    let short = shared.short_frames.lock().unwrap().get(first_frame);
    tracing::info!(
        first = first_frame,
        last = last_frame,
        missing_shards = short.map(|s| s.0),
        recovery_shards = short.map(|s| s.1),
        "reference-frame invalidation requested"
    );
    Ok(())
}

/// Fire a recovery ask. Call with the `recovery` lock released: the driver already dropped
/// the range a keyframe repairs.
fn send_recovery(shared: &ClientShared, ask: RecoveryAsk) {
    match ask {
        RecoveryAsk::Rfi(first, last) => {
            let _ = send_rfi(shared, first, last);
        }
        RecoveryAsk::Keyframe => shared.ask_keyframe(),
        RecoveryAsk::StarvedKeyframe => shared.ask_starved_keyframe(),
        RecoveryAsk::None => {}
    }
}

/// Default [`NativeClient::connect`] `name`: `/etc/hostname`, then env, then OS hostname.
/// Lives here so the C ABI (`punktfunk_connect`) shares it.
///
/// Apple GUI processes have neither `COMPUTERNAME` nor `HOSTNAME` (`launchd` does not
/// export the shell variable), so without `gethostname` every Apple client knocks as
/// "This device". Pass a better name via `punktfunk_connect_ex10`.
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
    // unreachable. [`ConnectParams::new`] leaves 0/0; the wire encodes 48 000/16 as absent.
    let hires = audio_rate_hz != 0 || audio_bits != 0;
    client_caps
        | crate::quic::CLIENT_CAP_AUDIO_RED
        | if hires {
            crate::quic::CLIENT_CAP_AUDIO_HIRES
        } else {
            0
        }
}

/// One dial's ask: the `Hello` fields plus how to reach, trust and wait for the host.
///
/// [`ConnectParams::new`] fills what a plain dial sends: host-decided compositor, pad,
/// bitrate and PyroWave quality, 8-bit SDR, stereo Opus, HEVC only, no client caps, no
/// launch, anonymous TOFU. Set the rest by field name.
pub struct ConnectParams {
    /// IP literal or resolvable hostname.
    pub host: String,
    pub port: u16,
    /// Asked-for size; the host answers in [`NativeClient::mode`].
    pub mode: Mode,
    pub compositor: CompositorPref,
    pub gamepad: GamepadPref,
    /// Encoder rate in kbps; `0` = host default, and the only value that arms ABR.
    pub bitrate_kbps: u32,
    /// PyroWave quality in hundredths of a bit per pixel
    /// ([`crate::quic::EXT_TAG_PYROWAVE_QUALITY`]); `0` leaves the host's own.
    pub pyrowave_bpp_x100: u16,
    /// H.264/HEVC slices per picture the decoder wants; `0` leaves the host's. The host reads
    /// it only under [`crate::quic::VIDEO_CAP_MULTI_SLICE`].
    pub slices: u8,
    /// [`crate::quic::VIDEO_CAP_10BIT`] / [`crate::quic::VIDEO_CAP_HDR`]; the host upgrades only
    /// on a set bit. `0` = 8-bit BT.709.
    pub video_caps: u8,
    /// 2 / 6 / 8; the host clamps and answers in [`NativeClient::audio_channels`].
    pub audio_channels: u8,
    /// Audio format ask ([`crate::audio::pcm::rate_is_supported`]); `0` = unspecified. Either
    /// half non-zero asks for lossless ([`advertised_client_caps`]), 48 kHz/16-bit included.
    pub audio_rate_hz: u32,
    /// 16 or 24; see `audio_rate_hz`.
    pub audio_bits: u8,
    /// Surround coupling to ask for; `Legacy` keeps the `Hello` byte-identical.
    pub audio_layout: crate::audio::AudioLayout,
    /// How this client fills its view ([`crate::quic::Hello::video_fit`]).
    pub video_fit: crate::video_fit::VideoFit,
    /// The sinks this embedder plays through unchanged
    /// ([`crate::quic::Hello::audio_bitstreams`]); `0` asks for none.
    pub audio_bitstreams: u8,
    /// Decode bitfield (H264 / HEVC / AV1 / PyroWave); `0` = HEVC only.
    pub video_codecs: u8,
    /// One codec bit to prefer; `0` = host's choice. The host answers in [`NativeClient::codec`].
    pub preferred_codec: u8,
    /// Panel volume for the virtual display's EDID; `None` = unknown/SDR.
    pub display_hdr: Option<HdrMeta>,
    /// Set [`crate::quic::CLIENT_CAP_CURSOR`] only when this embedder draws the pointer: the
    /// host then stops compositing it.
    pub client_caps: u8,
    /// AU prefixes as [`Frame`]s with `part = Some`. Only for a decoder that takes parts.
    pub frame_parts: bool,
    /// This embedder rebuilds its decoder on a frame whose [`Frame::new_format`] is `Some`.
    /// Only then may the host change codec, depth, colour or chroma mid-stream
    /// ([`crate::quic::v2::registry::FEATURE_FORMAT_EPOCH`]).
    pub rebuilds_decoder: bool,
    /// This embedder reopens its audio decoder and player on a packet whose
    /// [`AudioPacket::new_format`] is `Some`. Only then may the host change the audio format
    /// mid-stream ([`crate::quic::v2::registry::FEATURE_AUDIO_EPOCH`]).
    pub reopens_audio: bool,
    /// Store-qualified library id to launch (`steam:570`).
    pub launch: Option<String>,
    /// [`crate::quic::Hello::name`], usually [`device_name`]. `None` knocks as "device abcd1234".
    pub name: Option<String>,
    /// Expected SHA-256 of the host cert; a mismatch is [`PunktfunkError::Crypto`]. `None` =
    /// TOFU: read [`NativeClient::host_fingerprint`] afterwards.
    pub pin: Option<[u8; 32]>,
    /// PEM cert + PKCS#8 key ([`endpoint::generate_identity`]); `None` = anonymous.
    pub identity: Option<(String, String)>,
    /// Settings preset this dial names; the host shows it and hands it to hooks, the stream is
    /// unchanged. `None` names none.
    pub preset: Option<crate::quic::SessionPreset>,
    /// A diagnostic session ([`crate::quic::EXT_DELIVERY_PROBE_ONLY`]): the host serves probes
    /// from the bound data plane and never builds a pipeline.
    pub probe_only: bool,
    /// The profile to play as: a host profile id. `None` lets the host choose
    /// ([`NativeClient::profile`] says which it did).
    pub profile: Option<String>,
    /// Every screen this embedder shows, screen 0 first with `mode`'s size; empty for one.
    /// Screens past the first arrive on [`NativeClient::next_screen_frame`].
    pub screens: Vec<crate::quic::v2::msg::Screen>,
    /// This embedder can show more than one picture, so screens are offered even with `screens`
    /// empty and [`NativeClient::request_screens`] can ask for them later.
    pub shows_screens: bool,
    /// This embedder's display shows full-range BT.709 video as studio range, so the host is
    /// asked for studio SDR ([`crate::quic::v2::registry::FEATURE_STUDIO_RANGE`]). Build the
    /// decoder from [`NativeClient::video_format`]'s colour, which says what the host sent.
    pub studio_range: bool,
    /// [`Self::studio_range`] for HDR10: this display shows full-range HDR as studio range
    /// ([`crate::quic::v2::registry::FEATURE_STUDIO_RANGE_HDR`]).
    pub studio_range_hdr: bool,
    /// Handshake budget. The dial re-dials inside it, so a waking host is not a failure.
    pub timeout: Duration,
    /// Abort while blocked: a request-access knock parks ~185 s. Never alias the session's
    /// `shutdown` onto it; that races the end reason. `None` = uncancelable.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Called once, on the connecting thread, when the host answers that its operator is
    /// deciding about this device. From then the dial waits for the host's verdict, up to
    /// [`APPROVAL_WAIT`], not `timeout`; `cancel` still ends it. `None`: `timeout` stands.
    pub on_pending: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Longest a dial waits on an operator once the host said it is asking. The host answers
/// sooner: approval, a refusal, or its own timeout.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(10 * 60);

impl ConnectParams {
    /// A plain dial to `host:port` at (up to) `mode`, giving up after `timeout`.
    pub fn new(host: impl Into<String>, port: u16, mode: Mode, timeout: Duration) -> Self {
        ConnectParams {
            host: host.into(),
            port,
            mode,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            pyrowave_bpp_x100: 0,
            slices: 0,
            video_caps: 0,
            audio_channels: 2,
            audio_rate_hz: 0,
            audio_bits: 0,
            audio_layout: crate::audio::AudioLayout::Legacy,
            video_fit: crate::video_fit::VideoFit::Fit,
            audio_bitstreams: 0,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            frame_parts: false,
            rebuilds_decoder: false,
            reopens_audio: false,
            launch: None,
            name: None,
            pin: None,
            identity: None,
            preset: None,
            probe_only: false,
            profile: None,
            screens: Vec::new(),
            shows_screens: false,
            studio_range: false,
            studio_range_hdr: false,
            timeout,
            cancel: None,
            on_pending: None,
        }
    }
}

impl NativeClient {
    /// Dial a `punktfunk/2` host and block until the handshake lands, `timeout` passes or
    /// `cancel` is set. A host that turns the dial away is [`PunktfunkError::Rejected`].
    ///
    /// The host may answer below the ask: open the audio device from
    /// [`audio_codec`](Self::audio_codec) / [`audio_sample_rate_hz`](Self::audio_sample_rate_hz) /
    /// [`audio_bits`](Self::audio_bits) and build the decoder from [`codec`](Self::codec), never
    /// from `params`.
    pub fn connect(mut params: ConnectParams) -> Result<NativeClient> {
        let (planes_tx, planes) = planes::channels();
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel::<(u32, u64, Vec<u8>)>(MIC_QUEUE);
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (pad_touch_tx, pad_touch_rx) =
            tokio::sync::mpsc::unbounded_channel::<pad_touch::Contact>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(CTRL_QUEUE);
        let (clip_cmd_tx, clip_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClipCommand>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shared = Arc::new(ClientShared::new(params.mode));
        shared
            .probe_only
            .store(params.probe_only, Ordering::Relaxed);

        let cancel = params.cancel.take();
        let on_pending = params.on_pending.take();
        let (timeout, bitrate_kbps, requested_gamepad) =
            (params.timeout, params.bitrate_kbps, params.gamepad);
        let (preferred_codec, offered_codecs) = (params.preferred_codec, params.video_codecs);
        let asked_444 = params.video_caps & crate::quic::VIDEO_CAP_444 != 0;
        // RED is core-decided; HIRES is not. See `advertised_client_caps`.
        params.client_caps =
            advertised_client_caps(params.client_caps, params.audio_rate_hz, params.audio_bits);
        let shared_w = shared.clone();
        let ctrl_tx_pump = ctrl_tx.clone(); // pump sends its report windows
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
                    params,
                    shared: shared_w,
                    planes: planes_tx,
                    input_rx,
                    mic_rx,
                    rich_input_rx,
                    pad_touch_rx,
                    ctrl_rx,
                    ctrl_tx: ctrl_tx_pump,
                    clip_cmd_rx,
                    ready_tx,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        // Poll so `cancel` can abort; a parked request-access handshake has nothing to wake on.
        const READY_POLL: Duration = Duration::from_millis(50);
        let mut deadline = std::time::Instant::now() + timeout;
        let mut told_pending = false;
        let negotiated = loop {
            if !told_pending && shared.pending.load(Ordering::Relaxed) {
                told_pending = true;
                if let Some(hook) = &on_pending {
                    hook();
                    deadline = deadline.max(std::time::Instant::now() + APPROVAL_WAIT);
                }
            }
            match ready_rx.recv_timeout(READY_POLL) {
                Ok(Ok(t)) => break t,
                Ok(Err(e)) => {
                    // Stops what the handshake already started.
                    shared.shutdown.store(true, Ordering::SeqCst);
                    return Err(e);
                }
                // Keep waiting unless budget spent or cancelled. Disconnected = worker died
                // without reporting; the give-up arm below covers it. Cancel and expiry share
                // that arm: both owe the host the same close.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline
                        && !cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) => {}
                Err(_) => {
                    // Failed connect must not linger if handshake lands late: QUIT, not close
                    // code 0, so the host tears down instead of holding a virtual display.
                    shared.quit.store(true, Ordering::SeqCst);
                    shared.shutdown.store(true, Ordering::SeqCst);
                    return Err(PunktfunkError::Timeout);
                }
            }
        };
        let welcome = negotiated.welcome;
        *shared.mode.lock().unwrap() = welcome.mode;
        shared
            .recovery
            .lock()
            .unwrap()
            .set_all_intra(welcome.codec == crate::quic::CODEC_PYROWAVE);
        let hud = Arc::new(crate::hud::Stats::new(shared.clock_offset.clone()));
        Ok(NativeClient {
            shared,
            planes,
            hdr_meta_last: Mutex::new(None),
            input_tx,
            born: Instant::now(),
            last_input_ms: AtomicU64::new(0),
            mic_tx,
            rich_input_tx,
            pad_touch_tx,
            sc2: Mutex::default(),
            ctrl_tx,
            clip_cmd_tx,
            next_xfer_id: AtomicU32::new(1),
            pen_seq: AtomicU16::new(0),
            welcome,
            host_caps: welcome.host_caps,
            host_caps2: welcome.host_caps2,
            mgmt_port: welcome.mgmt_port,
            profile: negotiated.profile,
            worker: Some(worker),
            video_e2e_ns: Arc::new(AtomicU64::new(0)),
            audio_av_offset_ms: Arc::new(AtomicI64::new(0)),
            audio_buffer_ms: Arc::new(AtomicU32::new(0)),
            hud,
            // Match the pump: Automatic AND host echoed a rate. Dropping the last term
            // over-advertises against an old host that reports no rate.
            wants_decode: crate::quic::session_bitrate_kbps(
                bitrate_kbps,
                preferred_codec,
                offered_codecs,
                welcome.codec,
            ) == 0
                && welcome.bitrate_kbps > 0,
            asked_444,
            host_fingerprint: negotiated.host_fingerprint,
            resolved_compositor: welcome.compositor,
            resolved_gamepad: welcome.gamepad,
            requested_gamepad,
            resolved_bitrate_kbps: welcome.bitrate_kbps,
            shard_payload: welcome.shard_payload,
            clock_offset_ns: negotiated.clock_offset_ns,
            bit_depth: welcome.bit_depth,
            color: welcome.color,
            chroma_format: welcome.chroma_format,
            audio_channels: welcome.audio_channels,
            audio_codec: welcome.audio_codec,
            audio_sample_rate_hz: welcome.audio_rate_hz,
            audio_bits: welcome.audio_bits,
            audio_frame_us: welcome.audio_frame_us,
            audio_layout: welcome.audio_layout,
            codec: welcome.codec,
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
            let remote = dial_addr(&host, port).await.ok()?;
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
        *self.shared.mode.lock().unwrap()
    }

    /// Queue a live mode switch (no reconnect). Accepted: next frames open with an IDR and
    /// [`NativeClient::mode`] updates. Rejected: session unchanged.
    pub fn request_mode(&self, mode: Mode) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Mode(mode))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// The format the newest frame from [`next_frame`](Self::next_frame) decodes as: the
    /// Welcome's until a frame's [`Frame::new_format`] moves it.
    pub fn video_format(&self) -> crate::quic::VideoFormat {
        *self.shared.video_format.lock().unwrap()
    }

    /// The decoder can't take these `CODEC_*` bits: ask the host to stream on in another codec,
    /// from a new epoch, without a reconnect. The answer is a frame whose [`Frame::new_format`]
    /// names the new codec, or a [`codec_refused`](Self::codec_refused).
    /// [`PunktfunkError::Unsupported`] when the session has no format epochs.
    pub fn request_codec_change(&self, exclude: u8) -> Result<()> {
        if !self.shared.anchor.lock().unwrap().formats {
            return Err(PunktfunkError::Unsupported("format epochs"));
        }
        self.ctrl_tx
            .try_send(CtrlRequest::Codec(exclude))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// The desktop audio format the newest packet from [`next_audio`](Self::next_audio) is in:
    /// the Welcome's until a packet's [`AudioPacket::new_format`] moves it.
    pub fn audio_format(&self) -> crate::audio::plane::PlaneFormat {
        self.shared
            .audio_out
            .lock()
            .unwrap()
            .unwrap_or_else(|| crate::audio::plane::PlaneFormat::of(self))
    }

    /// `CODEC_*` bits of the codec ask the host refused, `0` while none was. The stream is
    /// unchanged: reconnect without those codecs.
    pub fn codec_refused(&self) -> u8 {
        self.shared.codec_refused.load(Ordering::Relaxed)
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

    /// The decoder needs an IDR: a rebuild, a demotion, a damaged AU. Arms the freeze if it
    /// is down and asks through the one 100 ms window, so no caller floods the control
    /// stream. Drops the lost range an RFI still owes; the IDR repairs it.
    pub fn request_keyframe(&self) -> Result<()> {
        self.recovery_step(RecoveryEvent::NeedKeyframe, false);
        Ok(())
    }

    /// The device's network path changed: an interface came up, went down or took over the
    /// default route. Re-picks the socket's source address and sends the host a copy of the
    /// last feedback, so it follows a roam within a round trip instead of a keep-alive.
    pub fn path_changed(&self) {
        if let Some(reconnect) = self.shared.reconnect.get() {
            reconnect();
        }
        let fb = self.shared.feedback.lock().unwrap().again();
        self.shared.send_feedback(&fb);
    }

    /// Recover `[first_frame, last_frame]` by RFI instead of a full IDR. Capable hosts emit a
    /// P-frame tagged [`crate::packet::USER_FLAG_RECOVERY_ANCHOR`]; others force an IDR
    /// ([`request_keyframe`](Self::request_keyframe)). Prefer on loss; keyframe is the backstop
    /// when the recovery frame itself is lost. Fire-and-forget; throttle like keyframe.
    ///
    /// Every RFI a client sends is logged at info with what its first frame lacked.
    pub fn request_rfi(&self, first_frame: u32, last_frame: u32) -> Result<()> {
        send_rfi(&self.shared, first_frame, last_frame)
    }

    /// For a lane that keeps no freeze (an all-intra decoder, a recorder): feed each received
    /// AU's `frame_index` in receive order. A forward gap fires a throttled
    /// [`request_rfi`](Self::request_rfi) for the lost range, less frames the pump asked for
    /// at their tail, and returns its width; `0` in order and for a straggler. It never arms
    /// the freeze, so the pump's tick asks for nothing on this lane. A decoder that holds a
    /// picture reports through [`recovery_on_frame`](Self::recovery_on_frame) instead.
    pub fn note_frame_index(&self, frame_index: u32) -> u32 {
        // Update under the lock; fire the request after releasing it.
        let (order, ask) = self
            .shared
            .recovery
            .lock()
            .unwrap()
            .observe(frame_index, Instant::now());
        send_recovery(&self.shared, ask);
        match order {
            FrameOrder::Gap(gap) => gap,
            FrameOrder::InOrder | FrameOrder::Straggler => 0,
        }
    }

    /// One AU before decode, in receive order. `false`: drop it — a straggler a reference
    /// decoder rewinds on, or a partial a newer frame beat. A gap arms the freeze and asks
    /// for the lost range from here. `decode_order` is the decoder's count so far, the
    /// watermark a recovery SEI is paired against.
    pub fn recovery_on_frame(&self, frame: &Frame, decode_order: u64) -> bool {
        self.recovery_on_au(frame.frame_index, frame.complete, decode_order)
            .admit
    }

    /// [`recovery_on_frame`](Self::recovery_on_frame) by index, with the gap ahead of the AU
    /// for the receiver rule. Report a sliced AU once, on its first part: parts repeat the
    /// index.
    pub fn recovery_on_au(&self, index: u32, complete: bool, decode_order: u64) -> Arrival {
        let arrived = RecoveryEvent::Arrived {
            index,
            complete,
            decode_order,
        };
        let step = self.recovery_step(arrived, true);
        Arrival {
            admit: step.admit,
            gap: step.gap,
        }
    }

    /// One decoded picture: whether to show it, and whether the lift came from an
    /// intra-refresh heal the decoder's reference ledger cannot see.
    pub fn recovery_on_decoded(&self, decoded: Decoded) -> Shown {
        let step = self.recovery_step(RecoveryEvent::Decoded(decoded), true);
        Shown {
            hold: step.hold,
            forgive_chain: step.forgive_chain,
        }
    }

    /// One AU produced no picture: a wedged decoder with no reassembler drop. Three in a
    /// row arm the freeze and ask for an IDR.
    pub fn recovery_on_no_output(&self) {
        self.recovery_step(RecoveryEvent::NoOutput, true);
    }

    /// A loss the index gap cannot see (a parked AU overflowed, a wedge): arm and ask.
    pub fn recovery_lost(&self) {
        self.recovery_step(RecoveryEvent::Lost, true);
    }

    /// The decoder owes output and nothing was lost: arm and ask as
    /// [`recovery_lost`](Self::recovery_lost) does. This ask and the freeze's re-asks after
    /// it are counted apart from recovery's: a lower rate cannot feed a starved decoder.
    pub fn recovery_starved(&self) {
        self.recovery_step(RecoveryEvent::Starved, true);
    }

    /// The decoder raised an error it recovers from: its next pictures may be concealment.
    /// Arms the freeze without asking; the backstop asks if no re-anchor comes.
    pub fn recovery_on_codec_error(&self) {
        self.recovery_step(RecoveryEvent::CodecError, true);
    }

    /// The decoder is gone or parked (a rebuild, a lost surface, a background drain): drop
    /// the freeze. Nothing polls or asks until the next report, which starts a fresh one at
    /// that moment's drop count.
    pub fn recovery_reset(&self) {
        self.shared.recovery.lock().unwrap().reset();
    }

    /// The freeze's arm count, `0` with none up. A move since the last look is a loss: a
    /// cadence loop re-anchors on it.
    pub fn recovery_arms(&self) -> u64 {
        self.shared.recovery.lock().unwrap().arms()
    }

    /// Fold one event under the lock and send what it asks for. An embedder's `report`
    /// makes the driver's gate live from the first one; the pump's tick then asks for it.
    fn recovery_step(&self, ev: RecoveryEvent, report: bool) -> recovery::RecoveryStep {
        let step = {
            let mut r = self.shared.recovery.lock().unwrap();
            if report {
                r.engage(self.shared.frames_dropped.load(Ordering::Relaxed));
            }
            r.step(ev, Instant::now())
        };
        send_recovery(&self.shared, step.ask);
        step
    }

    /// Unrecoverable AUs (FEC failed). Poll and [`request_keyframe`](Self::request_keyframe)
    /// on increase: infinite GOP conceals reference-missing frames, so a decode-error trigger
    /// misses them. Monotonic; compare against the last observed value.
    pub fn frames_dropped(&self) -> u64 {
        self.shared.frames_dropped.load(Ordering::Relaxed)
    }

    /// Whole AUs skipped because the decoder was still busy when newer ones arrived
    /// (all-intra streams only). Monotonic; compare against the last observed value.
    pub fn frames_behind(&self) -> u64 {
        self.shared.frames.skipped_ever()
    }

    /// The pinned bitrate (kbps) this client could not keep up with — it shed its receive
    /// backlog repeatedly and a pin leaves nothing else to give. `0` = not so far. Latches
    /// for the session; show it to the user once with the next move (Automatic, or lower).
    pub fn unsustainable_pin_kbps(&self) -> u32 {
        self.shared.unsustainable_pin_kbps.load(Ordering::Relaxed)
    }

    /// Parity-repaired shards (loss that never became a dropped frame). Monotonic; HUD diffs
    /// successive reads against [`frames_dropped`](Self::frames_dropped).
    pub fn fec_recovered_shards(&self) -> u64 {
        self.shared.fec_recovered.load(Ordering::Relaxed)
    }

    /// Mic uplink counts per stage. Monotonic; HUD diffs successive reads.
    pub fn mic_stats(&self) -> MicUplinkStats {
        MicUplinkStats {
            sent: self.shared.mic_stats.sent.load(Ordering::Relaxed),
            dropped_full: self.shared.mic_stats.dropped_full.load(Ordering::Relaxed),
            dropped_stale: self.shared.mic_stats.dropped_stale.load(Ordering::Relaxed),
        }
    }

    /// QUIC session ended (`conn.closed()`, [`disconnect_quit`](Self::disconnect_quit), or drop).
    /// Once true, every `next_*` plane returns [`PunktfunkError::Closed`]. Poll-friendly
    /// counterpart to catching `Closed` in a plane loop.
    pub fn is_session_ended(&self) -> bool {
        self.shared.shutdown.load(Ordering::SeqCst)
    }

    /// Why the session ended — see [`PunktfunkEndReason`].
    ///
    /// Refinement of [`is_session_ended`](Self::is_session_ended), never a substitute: stays
    /// [`PunktfunkEndReason::None`] until that is true. Latches through teardown.
    pub fn end_reason(&self) -> PunktfunkEndReason {
        PunktfunkEndReason::from_u8(self.shared.end_reason.load(Ordering::SeqCst))
    }

    pub fn ended_because_game_exited(&self) -> bool {
        self.end_reason() == PunktfunkEndReason::GameExited
    }

    /// Mid-session [`crate::reject::RejectReason`], if any. Access expiry (`0x69`) would
    /// otherwise file as `HostError`. Latches with `end_reason`. Connect-time rejections
    /// are [`PunktfunkError::Rejected`] from [`connect`](Self::connect).
    pub fn end_reject(&self) -> Option<crate::reject::RejectReason> {
        crate::reject::RejectReason::from_close_code(
            self.shared.end_reject_code.load(Ordering::SeqCst),
        )
    }

    /// What the host said about that close, in its own words — already stripped of
    /// control characters and capped ([`worker::sanitize_reason`]).
    ///
    /// `None` whenever the host sent no sentence, which is most closes: render
    /// [`end_reject`](Self::end_reject)'s own wording then. A host names what only it
    /// can know, a mis-set capture monitor being the case this exists for.
    pub fn end_reject_said(&self) -> Option<&str> {
        self.shared.end_reject_said.get().map(String::as_str)
    }

    /// [`crate::reject::wake_redial_delay`] for how this session ended: its typed close, and
    /// whether a frame reached the embedder first. Ask once
    /// [`is_session_ended`](Self::is_session_ended).
    pub fn wake_redial_delay(
        &self,
        auto_wake: bool,
        since_first: Option<Duration>,
    ) -> Option<Duration> {
        let streamed = self.shared.streamed.load(Ordering::Relaxed);
        crate::reject::wake_redial_delay(auto_wake, self.end_reject(), streamed, since_first)
    }

    /// Fold the calling thread into [`hot_thread_ids`](Self::hot_thread_ids) (decode/audio
    /// with the pump). Idempotent; no-op without `gettid`.
    pub fn register_hot_thread(&self) {
        register_hot_tid(&self.shared.hot_tids);
    }

    /// Pump tid plus [`register_hot_thread`](Self::register_hot_thread) ids. Android ADPF.
    /// Empty without `gettid`. Call after the first frame so the pump has registered.
    /// Exited threads are pruned: ADPF refuses a whole session over one dead tid.
    pub fn hot_thread_ids(&self) -> Vec<i32> {
        let Ok(mut v) = self.shared.hot_tids.lock() else {
            return Vec::new();
        };
        v.retain(|t| std::path::Path::new(&format!("/proc/self/task/{t}")).exists());
        v.clone()
    }

    /// Live host−client offset (ns). Re-syncs every 60 s and on a suspected wall-clock step.
    /// Prefer over connect-time [`clock_offset_ns`](Self::clock_offset_ns): NTP/drift silently
    /// corrupts capture-clock math. `0` = old host / synced clocks.
    pub fn clock_offset_now_ns(&self) -> i64 {
        self.shared.clock_offset.load(Ordering::Relaxed)
    }

    /// Live offset for plane threads that outlive `&self`. Load Relaxed each use; never cache
    /// across frames. Holding this does not keep the session alive (unlike `Arc<NativeClient>`).
    pub fn clock_offset_shared(&self) -> Arc<AtomicI64> {
        self.shared.clock_offset.clone()
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
        self.shared.rtt_us.load(Ordering::Relaxed)
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
            rate_cut: self.shared.rate_cut.load(Ordering::Relaxed),
            rfis_last_min: self
                .shared
                .recent_rfis
                .lock()
                .unwrap()
                .count(Instant::now()),
            pad_slots: self.pad_slots(),
        }
    }

    /// Sample for the overlay only while one is shown. On from connect.
    pub fn set_hud_enabled(&self, on: bool) {
        self.hud.set_enabled(on, &self.hud_counters());
    }

    /// Close the overlay window with everything the connector knows filled in: mode, codec,
    /// colour, chroma asked and granted, audio format, counters, the link line and the
    /// weak-network warning. The caller adds the decoder, display HDR and its own extras.
    pub fn hud_snapshot(&self) -> crate::hud::StatsSnapshot {
        // Whole AUs the all-intra drain dropped before the decoder: it fell behind.
        let behind = self.shared.frames.take_skipped();
        if behind > 0 {
            self.hud
                .note_skipped(0, behind.min(u64::from(u32::MAX)) as u32);
        }
        let mut s = self.hud.drain(&self.hud_counters());
        let m = self.mode();
        (s.width, s.height, s.refresh_hz) = (m.width, m.height, m.refresh_hz);
        let f = self.video_format();
        s.codec = crate::hud::codec_label(f.codec).into();
        s.bit_depth = f.bit_depth;
        if f.color.is_hdr() {
            s.hdr = crate::hud::Hdr::Hdr;
        }
        s.chroma_444 = f.chroma_format == crate::quic::CHROMA_IDC_444;
        s.asked_444 = self.asked_444;
        s.auto_rate = self.wants_decode;
        let a = self.audio_format();
        s.audio_lossless = a.is_pcm();
        s.audio_rate_hz = a.rate_hz;
        s.audio_bits = a.bits;
        s.audio_channels = a.channels;
        s.extras.push(crate::hud::Extra {
            role: crate::hud::Role::Muted,
            ..crate::hud::Extra::detail(self.link_line())
        });
        let floor = self.shared.quality_floor_kbps.load(Ordering::Relaxed);
        if floor > 0 {
            s.extras.push(crate::hud::Extra {
                text: crate::hud::quality_floor_line(floor),
                tier: crate::hud::StatsVerbosity::Compact,
                advanced_only: false,
                role: crate::hud::Role::Warn,
            });
        }
        s
    }

    /// The link every frame is paced at and why, as the overlay shows it.
    pub fn link_line(&self) -> String {
        let host = self.shared.host_link.lock().unwrap().facts();
        // The buffer is named while the socket drops packets and it is under what a 4K
        // frame needs.
        let small_buffer_kb = Some(self.recv_buffer_kb())
            .filter(|&kb| kb > 0 && kb < 8 * 1024 && self.shared.draining.load(Ordering::Relaxed));
        let shape = if self.shared.wake_shape.load(Ordering::Relaxed) {
            crate::abr::Shape::Wake
        } else {
            crate::abr::Shape::Auto
        };
        crate::hud::link_line(
            host,
            *self.shared.client_link.lock().unwrap(),
            *self.shared.link.lock().unwrap(),
            shape,
            small_buffer_kb,
        )
    }

    pub fn audio_buffer_ms(&self) -> u32 {
        self.audio_buffer_ms.load(Ordering::Relaxed)
    }

    /// Decode-stage latency (µs): AU leaving [`next_frame`](Self::next_frame) to decoder output.
    /// Measure from handoff, not the codec-queue call (includes input backpressure); exclude
    /// vsync wait. Feeds Automatic ABR so rate caps at the decoder, not the link. Call every
    /// frame; ignored when Automatic is off; pump drains each window so the acc stays bounded.
    pub fn report_decode_us(&self, us: u32) {
        let mut acc = self.shared.decode_lat.lock().unwrap();
        acc.sum_us += us as u64;
        acc.count += 1;
    }

    /// Assemble video AUs in `memory` from the next frame on: a decoder that reads the AU where
    /// it lies lends its own ([`crate::au`]). Frames already assembled keep their memory.
    pub fn set_au_memory(&self, memory: Arc<dyn crate::au::AuMemory>) {
        *self.shared.au_memory.lock().unwrap() = Some(memory);
    }

    /// Whether [`report_decode_us`](Self::report_decode_us) is used (Automatic). Constant for
    /// the session.
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
        self.shared
            .abr_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// What the bring-up ramp measured, or `None` while it is still running,
    /// was declined, or never ran.
    pub fn abr_ramp(&self) -> Option<crate::abr::RampRecord> {
        self.shared
            .abr_ramp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn current_bitrate_kbps(&self) -> u32 {
        self.shared.live_bitrate_kbps.load(Ordering::Relaxed)
    }

    /// Burst filler at `target_kbps` for `duration_ms` beside the video. Non-blocking; poll
    /// [`NativeClient::probe_result`] until `done`. Resets any prior measurement. Host clamps
    /// ≤ 10 Gbps, ≤ 5 s.
    pub fn request_probe(&self, target_kbps: u32, duration_ms: u32) -> Result<()> {
        self.send_probe(
            duration_ms,
            CtrlRequest::Probe(ProbeRequest {
                target_kbps,
                duration_ms,
            }),
        )
    }

    /// A shaped burst ([`crate::quic::ProbeShaped`]): frame-sized bursts at line rate, or
    /// paced groups, at the same average rate. Same polling as [`Self::request_probe`].
    pub fn request_probe_shaped(&self, shape: crate::quic::ProbeShaped) -> Result<()> {
        self.send_probe(shape.duration_ms, CtrlRequest::ProbeShaped(shape))
    }

    fn send_probe(&self, duration_ms: u32, req: CtrlRequest) -> Result<()> {
        *self.shared.probe.lock().unwrap() = ProbeState {
            active: true,
            duration_ms,
            ..Default::default()
        };
        let sent = self
            .ctrl_tx
            .try_send(req)
            .map_err(|_| PunktfunkError::Closed);
        if sent.is_err() {
            // Send failed: nothing will answer. Leaving `active` would suppress the pump's
            // report tick for the rest of the session.
            self.shared.probe.lock().unwrap().active = false;
        }
        sent
    }

    /// What the host said about its end of the path in its `ServerHello`; zeros before the
    /// handshake, and where its OS did not say.
    pub fn host_link(&self) -> crate::quic::HostLink {
        *self.shared.host_link.lock().unwrap()
    }

    /// A diagnostic session: the dial asked for probes only, so no video ever comes.
    pub fn probe_only(&self) -> bool {
        self.shared.probe_only()
    }

    /// Packets the OS dropped at this session's receive buffer so far; `None` where the
    /// platform keeps no per-socket figure ([`crate::transport::sockstat`]).
    pub fn socket_drops(&self) -> Option<u64> {
        let sock = self.shared.data_sock.lock().unwrap();
        sock.as_ref()
            .and_then(crate::transport::sockstat::socket_drops)
    }

    /// The receive buffer the OS granted the data socket, KiB; `0` before the dial lands.
    pub fn recv_buffer_kb(&self) -> u32 {
        let sock = self.shared.data_sock.lock().unwrap();
        sock.as_ref()
            .map_or(0, crate::transport::sockstat::recv_buffer_kb)
    }

    /// The address this session's data leaves from, once the dial has landed.
    pub fn local_ip(&self) -> Option<std::net::IpAddr> {
        let sock = self.shared.data_sock.lock().unwrap();
        sock.as_ref()
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.ip())
            .filter(|ip| !ip.is_unspecified())
            .or(*self.shared.local_ip.lock().unwrap())
    }

    /// Whether a burst is in flight — an embedder speed test or the startup capacity probe. Loss
    /// inside it is the burst's own doing on a link it exceeds, so a "connection issues" notice
    /// gated on this stays quiet for it.
    pub fn probe_active(&self) -> bool {
        self.shared.probe.lock().unwrap().active
    }

    /// Speed-test measurement: partial until `done`, then the host's end-of-burst report.
    pub fn probe_result(&self) -> ProbeOutcome {
        let p = self.shared.probe.lock().unwrap();
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
            gap_p50_us: crate::stats::probe_gap_percentile(&p.gap_buckets, 0.5),
            gap_p99_us: crate::stats::probe_gap_percentile(&p.gap_buckets, 0.99),
            reorders: p.reorders,
        }
    }

    /// Next FEC-recovered AU. [`PunktfunkError::NoFrame`] on timeout, `Closed` once ended.
    /// One thread per plane; `&self` is for sharing across planes, not two consumers of one.
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        match self.shared.frames.pop(timeout) {
            FramePop::Frame(mut f) => {
                if !self.shared.streamed.load(Ordering::Relaxed) {
                    self.shared.streamed.store(true, Ordering::Relaxed);
                }
                pump::anchor::mark(&self.shared, &mut f);
                let completes_au = f.part.as_ref().is_none_or(|p| p.last);
                self.hud
                    .note_received(f.pts_ns, f.received_ns, f.data.len(), completes_au);
                self.shared
                    .decoder_took(f.frame_index, f.flags, completes_au && f.complete);
                Ok(f)
            }
            FramePop::Timeout => Err(PunktfunkError::NoFrame),
            FramePop::Closed => Err(PunktfunkError::Closed),
        }
    }

    /// Return the consumer from [`Self::next_frame`] with `NoFrame` now, or at its next
    /// call: a hardware decode it submitted is complete. Any thread may call this.
    pub fn kick_frame_wait(&self) {
        self.shared.frames.kick();
    }

    /// The screens the host built, screen 0 included, and a generation that moves with each
    /// change; generation 0 before the host said. A screen it could not build is absent.
    pub fn screen_layout(&self) -> (u32, Vec<crate::quic::v2::msg::Screen>) {
        self.shared.screens.layout()
    }

    /// The host took screens: [`Self::request_screens`] reaches it. Without, the session shows
    /// one picture whatever is asked.
    pub fn screens_on(&self) -> bool {
        self.shared.screens.is_on()
    }

    /// Next whole AU of screen `screen` (1 or more), the first of each format with
    /// [`Frame::new_format`] set. `NoFrame` on timeout, also before the host opened that
    /// screen; `Closed` once the session ended or the host dropped the screen.
    pub fn next_screen_frame(&self, screen: u8, timeout: Duration) -> Result<Frame> {
        let lane = (screen > 0)
            .then(|| {
                self.shared
                    .screens
                    .lane(crate::packet::screen_stream(screen))
            })
            .flatten();
        let Some(lane) = lane else {
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return Err(PunktfunkError::Closed);
            }
            std::thread::sleep(timeout.min(Duration::from_millis(20)));
            return Err(PunktfunkError::NoFrame);
        };
        match lane.frames.pop(timeout) {
            FramePop::Frame(f) => Ok(f),
            FramePop::Timeout => Err(PunktfunkError::NoFrame),
            FramePop::Closed => Err(PunktfunkError::Closed),
        }
    }

    /// Screen `screen`'s decoder cannot go on, or is new: its frames wait for a keyframe, which
    /// the pump asks the host for, and that frame names its format.
    pub fn request_screen_keyframe(&self, screen: u8) {
        if screen > 0 {
            self.shared
                .screens
                .want_key(crate::packet::screen_stream(screen));
        }
    }

    /// Show these screens from now on, screen 0 first: the host adds, drops and resizes the
    /// others and answers with a new [`Self::screen_layout`]. Screen 0's size still moves by
    /// [`Self::request_mode`]. A host or dial without screens ignores it.
    pub fn request_screens(&self, screens: Vec<crate::quic::v2::msg::Screen>) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Screens(screens))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Next audio packet. Drain on a dedicated thread — packets arrive every 5 ms.
    pub fn next_audio(&self, timeout: Duration) -> Result<AudioPacket> {
        let pkt = pull(&self.planes.audio, timeout)?;
        if let Some(format) = pkt.new_format {
            *self.shared.audio_out.lock().unwrap() = Some(format);
        }
        Ok(pkt)
    }

    /// Mute this client's own speakers. Nothing leaves for the host: it keeps encoding, and a
    /// session joined to the same sink keeps hearing the game. Packets keep arriving and keep
    /// decoding — the embedder zeroes only what it queues for the device — so the decoder never
    /// loses its state and unmute lands in step. Leaves [`AUDIO_MUTE_HOST`] alone.
    pub fn set_audio_muted(&self, muted: bool) {
        set_mute_bit(&self.shared.audio_mute, AUDIO_MUTE_LOCAL, muted);
    }

    /// Why this session is silent: [`AUDIO_MUTE_LOCAL`], [`AUDIO_MUTE_HOST`], both, or `0`.
    /// The overlay names the reason from this; [`audio_mute_label`] is the shared wording.
    pub fn audio_mute(&self) -> u8 {
        self.shared.audio_mute.load(Ordering::Relaxed)
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
        self.shared.pad_slots.load(Ordering::Relaxed)
    }

    /// What became of this session's library launch, latest verdict first. `None` before the
    /// host sends one, on a session that launched nothing, and on a host too old to say.
    /// [`crate::quic::LaunchOutcome::notice`] is the line to show.
    pub fn launch_outcome(&self) -> Option<crate::quic::LaunchOutcome> {
        self.shared
            .launch_outcome
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
    pub fn next_rumble_ttl(&self, timeout: Duration) -> Result<RawRumble> {
        pull(&self.planes.rumble, timeout)
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
        match self.planes.rumble_sched.next_command(timeout) {
            Ok(Some(c)) => Ok(c),
            Ok(None) => Err(PunktfunkError::NoFrame),
            Err(rumble::Closed) => Err(PunktfunkError::Closed),
        }
    }

    /// Run `wake` on the demux thread as each rumble update lands, for a poller that blocks
    /// on something other than [`Self::next_rumble_command`] and then drains it with a zero
    /// timeout. A scheduled follow-up (a ramp step, a backstop) still waits for that poll.
    pub fn set_rumble_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.planes.rumble_sched.set_wake(wake);
    }

    /// Actuator quirks for wire pad `pad` (at attach). Default = well-behaved; only decaying
    /// actuators need a keepalive.
    pub fn set_rumble_quirks(&self, pad: u16, quirks: ActuatorQuirks) {
        self.planes.rumble_sched.set_quirks(pad, quirks);
    }

    /// DualSense HID-output (lightbar / LEDs / adaptive trigger). DualSense host backend only.
    pub fn next_hidout(&self, timeout: Duration) -> Result<HidOutput> {
        pull(&self.planes.hidout, timeout)
    }

    /// Pad-audio Opus (haptics 5 ms / speaker 10 ms). Shared queue; fan out by `pad`/`kind`.
    /// `None` on timeout and on end ([`is_session_ended`](Self::is_session_ended) distinguishes).
    /// Empty unless [`quic::CLIENT_CAP_PAD_AUDIO`] met [`quic::HOST_CAP_PAD_AUDIO`] and
    /// [`set_pad_audio_caps`](Self::set_pad_audio_caps) declared the pad.
    pub fn next_pad_audio(&self, timeout: Duration) -> Option<PadAudioFrame> {
        pull(&self.planes.pad_audio, timeout).ok()
    }

    /// Pad-audio render caps: bit0 haptics, bit1 speaker. Call at attach, before arrival —
    /// worker ORs bits 8/9 toward a [`quic::HOST_CAP_PAD_AUDIO`] host only. Never calling
    /// this leaves the wire unchanged. Latest-wins; unknown bits masked.
    pub fn set_pad_audio_caps(&self, pad: u8, audio_caps: u8) {
        if let Some(slot) = self.shared.pad_audio_caps.get(pad as usize) {
            slot.store(audio_caps & 0x03, Ordering::Relaxed);
        }
    }

    /// ST.2086 mastering + CLL. Host sends at start and on mastering/keyframe changes. HDR
    /// (`color.is_hdr()`, PQ) only; drain on its own thread and apply the latest.
    pub fn next_hdr_meta(&self, timeout: Duration) -> Result<HdrMeta> {
        pull(&self.planes.hdr_meta, timeout)
    }

    /// The newest [`HdrMeta`] so far: drains the queue, blocking up to `wait` only while none
    /// has ever arrived. Kept across calls, so a decoder rebuilt mid-session starts from the
    /// current grade. Use this OR [`next_hdr_meta`](Self::next_hdr_meta), never both.
    pub fn latest_hdr_meta(&self, wait: Duration) -> Option<HdrMeta> {
        let rx = self.planes.hdr_meta.lock().unwrap();
        latest_of(&rx, &mut self.hdr_meta_last.lock().unwrap(), wait)
    }

    /// RGBA cursor bitmap + hotspot, on pointer-bitmap change. Cache by `serial`;
    /// [`NativeClient::next_cursor_state`] references it. Empty unless
    /// [`crate::quic::CLIENT_CAP_CURSOR`] was advertised against a capable host.
    pub fn next_cursor_shape(&self, timeout: Duration) -> Result<crate::quic::CursorShape> {
        self.planes
            .cursor_shape
            .recv_timeout(timeout)
            .map_err(plane_err)
    }

    /// Per-frame cursor state (`0xD0`): position, visibility, relative-mode hint. Latest-wins
    /// — drain and apply only the newest. Same gate as [`NativeClient::next_cursor_shape`].
    pub fn next_cursor_state(&self, timeout: Duration) -> Result<crate::quic::CursorState> {
        pull(&self.planes.cursor_state, timeout)
    }

    /// Per-AU capture→sent (`pts_ns`). HUD split: `network = (received + clock_offset − pts)
    /// − host_us`. Older host never sends any — keep combined `host+network`. Drain
    /// non-blockingly alongside frame samples.
    pub fn next_host_timing(&self, timeout: Duration) -> Result<crate::quic::HostTiming> {
        let t = pull(&self.planes.host_timing, timeout)?;
        self.hud.note_host_timing(&t);
        Ok(t)
    }

    /// How long since the player last did something, for [`holds_screen_awake`]. Every
    /// platform's input leaves through this client's senders, so they are the one clock for
    /// it: an event that [`InputEvent::is_activity`], a pad surface touched, a raw pad report
    /// with a button held, a pen batch. Motion never counts: a pad on the sofa still reports.
    pub fn input_idle(&self) -> Duration {
        let last = Duration::from_millis(self.last_input_ms.load(Ordering::Relaxed));
        self.born.elapsed().saturating_sub(last)
    }

    /// Restart [`Self::input_idle`]'s clock.
    fn stamp_input(&self) {
        let now = self.born.elapsed().as_millis() as u64;
        self.last_input_ms.store(now, Ordering::Relaxed);
    }

    /// Queue one event. The input task drops a class the live grants refuse; the host
    /// enforces the same mask. A pad's arrival or removal restarts its SC2 filter. Player
    /// input stamps [`Self::input_idle`]'s clock.
    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        if ev.is_activity() {
            self.stamp_input();
        }
        if matches!(
            ev.kind,
            InputKind::GamepadArrival | InputKind::GamepadRemove
        ) {
            if let Some(f) = self
                .sc2
                .lock()
                .unwrap()
                .get_mut(usize::from(ev.flags as u8))
            {
                f.reset();
            }
        }
        self.input_tx.send(*ev).map_err(|_| PunktfunkError::Closed)
    }

    /// Tell the host who the Steam Controller on `id.pad` is, before its arrival: the host's
    /// virtual pad then answers Steam with this serial and these replies
    /// ([`sc2::identity_requests`] lists what to read, [`sc2::classic_identity_requests`] for
    /// the 2015 pad). An older host skips the frame.
    pub fn send_pad_identity(&self, id: crate::quic::PadIdentity) -> Result<()> {
        if id.serial.len() > crate::quic::PAD_IDENTITY_SERIAL_MAX
            || id.replies.len() > crate::quic::PAD_IDENTITY_REPLIES_MAX
            || crate::quic::unpack_identity_replies(&id.replies).is_none()
        {
            return Err(PunktfunkError::InvalidArg("pad identity out of bounds"));
        }
        self.ctrl_tx
            .try_send(CtrlRequest::PadIdentity(id))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// How a Steam Controller's raw reports on wire pad `pad` are gated, either generation: the
    /// client's overlay, its system-button policy and its ring chord. Latest wins; an index
    /// past 15 is ignored.
    pub fn set_sc2_gate(&self, pad: u8, gate: Sc2Gate) {
        if let Some(f) = self.sc2.lock().unwrap().get_mut(usize::from(pad)) {
            f.gate = gate;
        }
    }

    /// The host's session offer as it arrived. Connect-time values: [`mode`](Self::mode),
    /// [`access_grants`](Self::access_grants) and the live bitrate move on during the session.
    pub fn welcome(&self) -> &crate::quic::Welcome {
        &self.welcome
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

    /// The profile the host resolved this session to: the one asked for, or the host's choice
    /// when none was. `None` from a host without profiles.
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// Live grants ([`crate::quic::GRANT_GAMEPAD`] family). Welcome seed, latest
    /// [`crate::quic::AccessUpdate`] wins. Old host → [`crate::quic::GRANT_ALL`]. Courtesy:
    /// the host enforces. Load per use; never cache across
    /// [`next_access_update`](Self::next_access_update).
    pub fn access_grants(&self) -> u32 {
        self.shared.access_grants.load(Ordering::Relaxed)
    }

    /// Access expiry as client-wall unix seconds. `None` = permanent (old host). Anchored
    /// from relative wire seconds so skew cannot move a countdown; re-anchored on update.
    pub fn access_deadline_unix(&self) -> Option<u64> {
        match self.shared.access_deadline_unix.load(Ordering::Relaxed) {
            0 => None,
            d => Some(d),
        }
    }

    /// Seconds until access expires, as the C and JNI exports hand it to Swift and Kotlin.
    /// `0` = permanent. While a deadline is set it never reads `0`: it clamps to 1 past expiry
    /// until the host's typed close lands, so an ending session never reads as "forever".
    pub fn access_expires_in_secs(&self) -> u32 {
        expires_in_secs(
            self.access_deadline_unix(),
            crate::quic::wall_clock_ns() / 1_000_000_000,
        )
    }

    /// Mid-session [`crate::quic::AccessUpdate`] (console edit, T−5/T−1 expiry). Wake-up
    /// only: truth is already in [`access_grants`](Self::access_grants) /
    /// [`access_deadline_unix`](Self::access_deadline_unix).
    pub fn next_access_update(&self, timeout: Duration) -> Result<crate::quic::AccessUpdate> {
        pull(&self.planes.access, timeout)
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
        pull(&self.planes.clip, timeout)
    }

    /// Opus mic uplink (0xCB). `seq`/`pts_ns` are caller diagnostics. Best-effort; no retransmit.
    pub fn send_mic(&self, seq: u32, pts_ns: u64, opus: Vec<u8>) -> Result<()> {
        use tokio::sync::mpsc::error::TrySendError;
        match self.mic_tx.try_send((seq, pts_ns, opus)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                // Worker outran the pump's oldest-first shed. Drop (best-effort); counter visible.
                self.shared
                    .mic_stats
                    .dropped_full
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!("mic uplink queue full — dropping frame");
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err(PunktfunkError::Closed),
        }
    }

    /// Switch the pads in `mask` (bit = wire pad index) to full controller mouse: their buttons,
    /// sticks and touchpads drive the host pointer and a few keys while the host pad sits
    /// neutral. Every other full-mouse pad goes back to passthrough. Session-scoped; a removed
    /// pad drops its bit. Needs [`GRANT_POINTER`](crate::quic::GRANT_POINTER), and losing it
    /// clears every mode.
    pub fn set_pad_mouse(&self, mask: u16) -> Result<()> {
        if mask != 0 && self.access_grants() & crate::quic::GRANT_POINTER == 0 {
            return Err(PunktfunkError::Unsupported(
                "host did not grant pointer input",
            ));
        }
        self.shared.pad_mouse.request(mask);
        Ok(())
    }

    /// Change scroll direction for this session at the shared outbound seam.
    pub fn set_invert_scroll(&self, invert: bool) {
        self.shared.scroll_invert.store(invert, Ordering::Relaxed);
    }

    pub fn invert_scroll(&self) -> bool {
        self.shared.scroll_invert.load(Ordering::Relaxed)
    }

    /// The controller-mouse mode every pad in `target` shares; a mixed set reads as off.
    pub fn pad_mouse_mode(&self, target: u16) -> PadMouseMode {
        self.shared.pad_mouse.mode(target, self.access_grants())
    }

    /// Step the pads in `target` to the next mode (off, touchpad, full) and return it. In
    /// touchpad mode the pads stay in the game and only their touchpads drive the pointer.
    /// Needs [`GRANT_POINTER`](crate::quic::GRANT_POINTER).
    pub fn cycle_pad_mouse(&self, target: u16) -> Result<PadMouseMode> {
        if self.access_grants() & crate::quic::GRANT_POINTER == 0 {
            return Err(PunktfunkError::Unsupported(
                "host did not grant pointer input",
            ));
        }
        if target == 0 {
            return Ok(PadMouseMode::Off);
        }
        let next = self.pad_mouse_mode(target).next();
        self.shared.pad_mouse.set_mode(target, next);
        Ok(next)
    }

    /// Pads in full controller mouse that are still connected.
    pub fn pad_mouse(&self) -> u16 {
        self.shared.pad_mouse.active(self.access_grants())
    }

    /// Wire pad indices the host holds right now: declared or driven, not yet removed.
    pub fn live_pads(&self) -> u16 {
        self.shared.pad_mouse.live()
    }

    /// Rich input (0xCC): touchpads, motion, a Steam Controller 2's raw reports. Best-effort.
    /// A raw report passes its pad's [`Sc2Gate`] first. Under controller mouse a pad's touchpads
    /// move the pointer instead, and a full-mouse pad keeps its gyro too, so it cannot aim the
    /// neutral host pad. A touch or a held button stamps [`Self::input_idle`]'s clock.
    pub fn send_rich_input(&self, mut rich: RichInput) -> Result<()> {
        if let RichInput::HidReport { pad, len, data } = &mut rich {
            let n = usize::from(*len).min(data.len());
            let mut filters = self.sc2.lock().unwrap();
            if let Some(f) = filters.get_mut(usize::from(*pad)) {
                if !f.apply(&mut data[..n]) {
                    return Ok(());
                }
            }
        }
        if match &rich {
            RichInput::Touchpad { active, .. } => *active,
            RichInput::TouchpadEx { touch, .. } => *touch,
            RichInput::HidReport { len, data, .. } => {
                sc2::holds_button(&data[..usize::from(*len).min(data.len())])
            }
            _ => false,
        } {
            self.stamp_input();
        }
        let bit = 1u16.checked_shl(u32::from(rich.pad())).unwrap_or(0);
        let grants = self.access_grants();
        let full = self.shared.pad_mouse.active(grants) & bit != 0;
        if full || self.shared.pad_mouse.touchpad_active(grants) & bit != 0 {
            let (contacts, forward) = pad_touch::route(&mut rich, full);
            for c in contacts {
                self.pad_touch_tx
                    .send(c)
                    .map_err(|_| PunktfunkError::Closed)?;
            }
            if !forward {
                return Ok(());
            }
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
        self.stamp_input();
        let seq = self.pen_seq.fetch_add(1, Ordering::Relaxed);
        self.rich_input_tx
            .send(crate::quic::PenBatch::new(seq, samples).encode())
            .map_err(|_| PunktfunkError::Closed)
    }

    /// User stop: close with [`crate::quic::QUIT_CLOSE_CODE`] so the host skips keep-alive
    /// linger. A plain drop closes with code 0 and the host waits for reconnect.
    pub fn disconnect_quit(&self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        self.shared.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
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

/// How long a stream counts as played after the last input the player made. With "Keep
/// screen awake" off, the client holds the screen for this long, then lets the device's own
/// idle timer run. 300 s outlasts most cutscenes.
pub const KEEP_AWAKE_INPUT: Duration = Duration::from_secs(300);

/// Whether a stream holds this device's screen awake, `input_idle` since the player's last
/// input ([`NativeClient::input_idle`]). Always, with the setting on. Off, only inside
/// [`KEEP_AWAKE_INPUT`]: an OS does not count controller input as activity, so a plain
/// "never" would blank a pad-only session mid-game.
pub fn holds_screen_awake(keep_awake: bool, input_idle: Duration) -> bool {
    keep_awake || input_idle < KEEP_AWAKE_INPUT
}

fn expires_in_secs(deadline_unix: Option<u64>, now_unix: u64) -> u32 {
    deadline_unix.map_or(0, |d| {
        u32::try_from(d.saturating_sub(now_unix))
            .unwrap_or(u32::MAX)
            .max(1)
    })
}

#[cfg(all(test, feature = "quic"))]
mod end_reason_tests {
    use super::PunktfunkEndReason;

    fn closed_with(code: u32) -> PunktfunkEndReason {
        let e = quinn::ConnectionError::ApplicationClosed(quinn::ApplicationClose {
            error_code: quinn::VarInt::from_u32(code),
            reason: Default::default(),
        });
        PunktfunkEndReason::from(&e)
    }

    /// A path too small for the stream redials like a dropped connection.
    #[test]
    fn a_shrunk_path_reads_as_lost() {
        assert_eq!(
            closed_with(crate::quic::PATH_SHRANK_CLOSE_CODE),
            PunktfunkEndReason::Lost
        );
        assert_eq!(
            closed_with(crate::quic::APP_EXITED_CLOSE_CODE),
            PunktfunkEndReason::GameExited
        );
        assert_eq!(closed_with(0x6B), PunktfunkEndReason::HostError);
    }
}

#[cfg(test)]
mod redial_tests {
    use super::PunktfunkEndReason;

    /// Only a lost connection redials, five times, with the waits growing.
    #[test]
    fn only_a_lost_session_redials() {
        let lost = PunktfunkEndReason::Lost;
        let waits: Vec<u64> = (0..)
            .map_while(|a| lost.redial_delay(a))
            .map(|d| d.as_secs())
            .collect();
        assert_eq!(waits, [0, 1, 2, 4, 8]);
        for other in [
            PunktfunkEndReason::None,
            PunktfunkEndReason::Local,
            PunktfunkEndReason::GameExited,
            PunktfunkEndReason::HostEnded,
            PunktfunkEndReason::HostError,
        ] {
            assert_eq!(other.redial_delay(0), None);
        }
    }
}

#[cfg(test)]
mod expires_in_tests {
    use super::expires_in_secs;

    #[test]
    fn zero_only_means_permanent() {
        assert_eq!(expires_in_secs(None, 100), 0);
        assert_eq!(expires_in_secs(Some(160), 100), 60);
        assert_eq!(expires_in_secs(Some(100), 100), 1);
        assert_eq!(expires_in_secs(Some(50), 100), 1);
        assert_eq!(expires_in_secs(Some(u64::MAX), 0), u32::MAX);
    }
}

/// A plane's timeout is [`PunktfunkError::NoFrame`]; a dropped worker is `Closed`.
fn plane_err(e: RecvTimeoutError) -> PunktfunkError {
    match e {
        RecvTimeoutError::Timeout => PunktfunkError::NoFrame,
        RecvTimeoutError::Disconnected => PunktfunkError::Closed,
    }
}

/// One blocking pull from a plane queue, waiting up to `timeout`.
fn pull<T>(rx: &Mutex<Receiver<T>>, timeout: Duration) -> Result<T> {
    rx.lock().unwrap().recv_timeout(timeout).map_err(plane_err)
}

/// Drain `rx` into `last`, blocking up to `wait` only while `last` is still empty.
fn latest_of<T: Copy>(rx: &Receiver<T>, last: &mut Option<T>, wait: Duration) -> Option<T> {
    if last.is_none() {
        *last = rx.recv_timeout(wait).ok();
    }
    while let Ok(m) = rx.try_recv() {
        *last = Some(m);
    }
    *last
}

#[cfg(test)]
mod latest_of_tests {
    use super::latest_of;
    use std::time::{Duration, Instant};

    #[test]
    fn keeps_the_newest_and_stops_waiting_once_one_arrived() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u8>(8);
        let mut last = None;
        assert_eq!(latest_of(&rx, &mut last, Duration::ZERO), None);
        for v in 1..=3 {
            tx.send(v).unwrap();
        }
        assert_eq!(latest_of(&rx, &mut last, Duration::ZERO), Some(3));
        let t = Instant::now();
        assert_eq!(latest_of(&rx, &mut last, Duration::from_secs(5)), Some(3));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}

#[cfg(test)]
mod host_port_tests {
    use super::{dial_addr, join_host_port};

    #[test]
    fn a_name_dials_its_ipv4_answer_and_a_literal_dials_as_written() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let at = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
            assert_eq!(
                dial_addr("localhost", 9777).await.unwrap(),
                at("127.0.0.1:9777")
            );
            assert_eq!(
                dial_addr("fd00::1", 9777).await.unwrap(),
                at("[fd00::1]:9777")
            );
            assert_eq!(
                dial_addr("192.168.1.9", 9777).await.unwrap(),
                at("192.168.1.9:9777")
            );
        });
    }

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

    /// A mute and an unmute both say so, and both stop: the microphone has no standing badge.
    #[test]
    fn the_microphone_names_each_switch_and_then_stops() {
        let old = LOCAL_MUTE_NOTICE + Duration::from_secs(1);
        assert_eq!(mic_mute_notice(true, Duration::ZERO), Some(true));
        assert_eq!(mic_mute_notice(false, Duration::ZERO), Some(false));
        assert_eq!(mic_mute_notice(true, old), None);
        assert_eq!(mic_mute_notice(false, old), None);
        assert_eq!(mic_mute_label(true), "Microphone muted");
        assert_eq!(mic_mute_label(false), "Microphone live");
    }

    /// The badge's lifetime, not its wording: the player's own mute says itself once and
    /// gets out of the picture; the operator's stands for as long as it does.
    #[test]
    fn only_the_players_own_mute_stops_naming_itself() {
        let old = LOCAL_MUTE_NOTICE + Duration::from_secs(1);
        assert_eq!(
            audio_mute_notice(AUDIO_MUTE_LOCAL, Duration::ZERO),
            Some("Muted on this device")
        );
        assert_eq!(audio_mute_notice(AUDIO_MUTE_LOCAL, old), None);

        // Anything the host muted keeps the badge, however long it has stood.
        assert_eq!(
            audio_mute_notice(AUDIO_MUTE_HOST, old),
            Some("Muted by the host")
        );
        assert_eq!(
            audio_mute_notice(AUDIO_MUTE_HOST | AUDIO_MUTE_LOCAL, old),
            Some("Muted by the host and on this device")
        );
        assert_eq!(audio_mute_notice(0, Duration::ZERO), None);
    }
}

#[cfg(test)]
mod keep_awake_tests {
    use super::{holds_screen_awake, KEEP_AWAKE_INPUT};
    use std::time::Duration;

    /// Keep-awake on holds the screen whatever the player does. Off, a stream nobody has
    /// touched for [`KEEP_AWAKE_INPUT`] lets the screen go; one being played keeps it.
    #[test]
    fn keep_awake_off_still_holds_the_screen_for_a_played_stream() {
        let long = KEEP_AWAKE_INPUT + Duration::from_secs(1);
        assert!(holds_screen_awake(true, Duration::ZERO));
        assert!(holds_screen_awake(true, long));
        assert!(holds_screen_awake(false, Duration::ZERO));
        assert!(holds_screen_awake(
            false,
            KEEP_AWAKE_INPUT - Duration::from_secs(1)
        ));
        assert!(!holds_screen_awake(false, long));
    }
}
