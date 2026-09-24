//! The in-stream stats overlay: one window, one snapshot, one formatter for every client.
//!
//! [`Stats`] collects a window of per-frame stamps. The connector
//! ([`crate::client::NativeClient`]) feeds receipt, host timing and its own counters; a client
//! adds decode and display stamps. [`Stats::drain`] closes the window into a
//! [`StatsSnapshot`], and [`format`] renders a snapshot in one of two vocabularies:
//!
//! - **Standard**: Moonlight's slice as means. Frame rates, host, decode and display time, loss
//!   and round trip. No end-to-end figure, because Moonlight has none to compare it with.
//! - **Advanced**: the capture→glass headline as p50/p95 and every stage that tiles it.
//!
//! Platforms draw the returned lines and nothing else. `docs-site/content/docs/(guide)/(streaming)/stats.md`
//! explains every number.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Overlay depth. Each tier shows everything the tier below it shows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum StatsVerbosity {
    Off,
    /// One glanceable line.
    Compact,
    /// Stream facts plus the headline figures.
    Normal,
    /// Everything, down to each stage.
    Detailed,
}

impl StatsVerbosity {
    pub const ALL: [StatsVerbosity; 4] = [
        StatsVerbosity::Off,
        StatsVerbosity::Compact,
        StatsVerbosity::Normal,
        StatsVerbosity::Detailed,
    ];

    pub fn next(self) -> StatsVerbosity {
        match self {
            StatsVerbosity::Off => StatsVerbosity::Compact,
            StatsVerbosity::Compact => StatsVerbosity::Normal,
            StatsVerbosity::Normal => StatsVerbosity::Detailed,
            StatsVerbosity::Detailed => StatsVerbosity::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StatsVerbosity::Off => "Off",
            StatsVerbosity::Compact => "Compact",
            StatsVerbosity::Normal => "Normal",
            StatsVerbosity::Detailed => "Detailed",
        }
    }

    /// Position in [`Self::ALL`]. The C ABI, JNI and wasm exports pass tiers this way.
    pub fn index(self) -> u32 {
        self as u32
    }

    /// Inverse of [`Self::index`]. An unknown index reads as Normal.
    pub fn from_index(i: u32) -> StatsVerbosity {
        match i {
            0 => StatsVerbosity::Off,
            1 => StatsVerbosity::Compact,
            3 => StatsVerbosity::Detailed,
            _ => StatsVerbosity::Normal,
        }
    }
}

/// One stage over a window, µs. `n == 0` means unmeasured, never zero latency.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Summary {
    pub n: u32,
    pub mean_us: u32,
    pub min_us: u32,
    pub max_us: u32,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
}

impl Summary {
    /// Sorts `samples` in place. Rank `len * p / 100`, the rule every client already used.
    pub fn of(samples: &mut [u32]) -> Summary {
        let n = samples.len();
        if n == 0 {
            return Summary::default();
        }
        samples.sort_unstable();
        let at = |p: usize| samples[(n * p / 100).min(n - 1)];
        let sum: u64 = samples.iter().map(|&s| u64::from(s)).sum();
        Summary {
            n: n.min(u32::MAX as usize) as u32,
            mean_us: (sum / n as u64) as u32,
            min_us: samples[0],
            max_us: samples[n - 1],
            p50_us: samples[n / 2],
            p95_us: at(95),
            p99_us: at(99),
        }
    }

    pub fn is_measured(&self) -> bool {
        self.n > 0
    }
}

/// How the stream's dynamic range reaches the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum Hdr {
    #[default]
    Sdr,
    /// HDR stream on an HDR swapchain.
    Hdr,
    /// HDR stream tone-mapped onto SDR.
    ToneMapped,
    /// HDR stream shown on SDR with no tone-map.
    Untonemapped,
}

impl Hdr {
    fn tag(self) -> Option<&'static str> {
        match self {
            Hdr::Sdr => None,
            Hdr::Hdr => Some("HDR"),
            Hdr::ToneMapped => Some("HDR→SDR"),
            Hdr::Untonemapped => Some("HDR→SDR (raw)"),
        }
    }
}

/// Why Automatic bitrate last cut the rate. Cleared once the rate climbs again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum RateCut {
    /// Lost packets, dropped frames or a flushed queue.
    Loss = 1,
    /// The decoder kept asking for keyframes.
    Repairs = 2,
    /// Frames took longer to decode.
    Decoder = 3,
    /// The host took longer to encode.
    Encoder = 4,
    /// Packets arrived later: a queue is building on the path.
    Delay = 5,
}

impl RateCut {
    /// `0` is no cut, the cell's resting value.
    pub fn from_code(code: u8) -> Option<RateCut> {
        Some(match code {
            1 => RateCut::Loss,
            2 => RateCut::Repairs,
            3 => RateCut::Decoder,
            4 => RateCut::Encoder,
            5 => RateCut::Delay,
            _ => return None,
        })
    }

    /// What a window's [`Reason`](crate::abr::Reason) tells a player. Several
    /// signals share a word: lost frames, a flush and shard loss are all
    /// "packet loss" to the person watching. `None` = no cut in it.
    pub fn of_reason(reason: crate::abr::Reason) -> Option<RateCut> {
        use crate::abr::Reason;
        Some(match reason {
            Reason::LostFrame | Reason::Flush | Reason::Loss => RateCut::Loss,
            Reason::KeyframeAsks => RateCut::Repairs,
            Reason::Decode => RateCut::Decoder,
            Reason::Encode => RateCut::Encoder,
            Reason::Owd => RateCut::Delay,
            Reason::Clean | Reason::Quiet | Reason::Blip => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            RateCut::Loss => "packet loss",
            RateCut::Repairs => "decode repairs",
            RateCut::Decoder => "slow decoding",
            RateCut::Encoder => "slow host encoding",
            RateCut::Delay => "network delay",
        }
    }
}

/// Where the Advanced headline interval stops. Set by what the window measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endpoint {
    Received,
    Decoded,
    Displayed,
    OnGlass,
}

impl Endpoint {
    pub fn label(self) -> &'static str {
        match self {
            Endpoint::Received => "received",
            Endpoint::Decoded => "decoded",
            Endpoint::Displayed => "displayed",
            Endpoint::OnGlass => "on-glass",
        }
    }
}

/// How a platform paints a line. The wire code is the discriminant.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum Role {
    /// Stream facts and headline figures.
    #[default]
    Primary = 0,
    /// Breakdowns under a headline.
    Detail = 1,
    /// Context that is not a cost: an excluded floor, a readback.
    Muted = 2,
    /// Something is wrong: loss, a clock that lies, a capped panel.
    Warn = 3,
}

impl Role {
    pub fn from_code(c: u8) -> Role {
        match c {
            1 => Role::Detail,
            2 => Role::Muted,
            3 => Role::Warn,
            _ => Role::Primary,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HudLine {
    pub role: Role,
    pub text: String,
}

/// A line only one platform can measure. `tier` is the lowest tier that shows it.
#[derive(Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Extra {
    pub text: String,
    pub tier: StatsVerbosity,
    /// Shown only with Advanced statistics on.
    pub advanced_only: bool,
    pub role: Role,
}

impl Extra {
    /// An Advanced, Detailed-tier line: the common case for a platform diagnostic.
    pub fn detail(text: impl Into<String>) -> Extra {
        Extra {
            text: text.into(),
            tier: StatsVerbosity::Detailed,
            advanced_only: true,
            role: Role::Muted,
        }
    }
}

/// Session-cumulative counters and live gauges, read when a window closes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub frames_dropped: u64,
    pub fec_recovered: u64,
    pub mic_sent: u64,
    pub mic_dropped: u64,
    pub audio_buffer_ms: u32,
    pub av_offset_ms: i32,
    /// Smoothed QUIC round trip, µs. `0` = unknown.
    pub rtt_us: u32,
    pub target_kbps: u32,
    /// [`RateCut`] code; `0` = the rate has not been cut since it last climbed.
    pub rate_cut: u8,
    /// RFIs this client sent in the last 60 s. A gauge, not windowed.
    pub rfis_last_min: u32,
    /// OS pad slots this session holds, one bit each ([`crate::quic::PadSlots`]).
    pub pad_slots: u16,
}

/// One closed window. Latencies are raw; [`format`] applies the OS-floor policy.
#[derive(Clone, PartialEq, Debug, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct StatsSnapshot {
    pub window_ms: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// `H.264`, `HEVC`, `AV1` or `PyroWave`; empty when unknown.
    pub codec: String,
    pub bit_depth: u8,
    pub hdr: Hdr,
    pub chroma_444: bool,
    /// The session asked for 4:4:4. With `chroma_444` false the host declined.
    pub asked_444: bool,
    /// Decode path or decoder name. Empty until the first frame decodes.
    pub decoder: String,
    pub preset: Option<String>,
    /// AUs received, and their payload bytes (goodput: no FEC, no headers).
    pub received: u32,
    pub bytes: u64,
    /// `None` on a platform that cannot see decodes or presents.
    pub decoded: Option<u32>,
    pub presented: Option<u32>,
    pub target_kbps: u32,
    /// The bitrate controller moves `target_kbps`.
    pub auto_rate: bool,
    /// Why the controller last lowered `target_kbps`, until it climbs again.
    pub rate_cut: Option<RateCut>,
    /// Loss repairs (RFIs) this client asked for in the last 60 s.
    pub rfis_last_min: u32,
    /// Capture → displayed, and capture → decoded (the headline when nothing reached glass).
    pub e2e: Summary,
    pub e2e_decoded: Summary,
    /// Capture → received, then its 0xCF split into the host's share and the rest.
    pub host_net: Summary,
    pub host: Summary,
    pub net: Summary,
    pub decode: Summary,
    pub display: Summary,
    /// `display` split: decoded → present submit, submit → displayed.
    pub pace: Summary,
    pub latch: Summary,
    /// The OS present pipeline depth no client can pace under.
    pub os_floor: Summary,
    pub host_queue: Summary,
    pub host_encode: Summary,
    pub host_xfer: Summary,
    pub host_pace: Summary,
    /// The displayed stamp is a true on-glass instant.
    pub on_glass: bool,
    /// `decode` sits inside `display` (the async Vulkan rung) and must not tile with it.
    pub decode_overlaps_display: bool,
    /// Subtract `os_floor` from `e2e` and `display` before showing them.
    pub shave_os_floor: bool,
    /// No clock offset was measured; cross-machine figures assume one clock.
    pub same_host_clock: bool,
    /// Capture-anchored samples that came out ≤ 0: the clock offset is wrong.
    pub skew_trimmed: u32,
    pub lost: u32,
    /// `None` on a platform that does not count its own pacing drops.
    pub skipped: Option<u32>,
    pub skipped_overflow: u32,
    pub fec: u32,
    pub mic_sent: u32,
    pub mic_dropped: u32,
    pub rtt_us: Option<u32>,
    pub audio_buffer_ms: u32,
    /// Positive = audio behind the picture.
    pub av_offset_ms: i32,
    pub audio_lossless: bool,
    pub audio_rate_hz: u32,
    pub audio_bits: u8,
    pub audio_channels: u8,
    /// OS pad slots this session holds, one bit each. Bit `n` = player `n + 1`;
    /// `0` = no pad, which is most sessions.
    pub pad_slots: u16,
    pub extras: Vec<Extra>,
}

impl StatsSnapshot {
    fn secs(&self) -> f64 {
        f64::from(self.window_ms.max(1)) / 1000.0
    }

    pub fn received_fps(&self) -> f64 {
        f64::from(self.received) / self.secs()
    }

    pub fn decoded_fps(&self) -> Option<f64> {
        self.decoded.map(|n| f64::from(n) / self.secs())
    }

    pub fn presented_fps(&self) -> Option<f64> {
        self.presented.map(|n| f64::from(n) / self.secs())
    }

    pub fn mbps(&self) -> f64 {
        self.bytes as f64 * 8.0 / 1e6 / self.secs()
    }

    /// `lost / (received + lost)`, percent.
    pub fn lost_pct(&self) -> f64 {
        pct(self.lost, self.received.saturating_add(self.lost))
    }

    /// [`lost_pct`](Self::lost_pct), or `None` when the window held under
    /// [`THIN_WINDOW_FRAMES`] frames and a share would mislead.
    fn lost_share(&self) -> Option<f64> {
        (self.received.saturating_add(self.lost) >= THIN_WINDOW_FRAMES).then(|| self.lost_pct())
    }

    /// `lost x.x%`, or `lost N` in a thin window.
    fn lost_label(&self) -> String {
        match self.lost_share() {
            Some(p) => format!("lost {p:.1}%"),
            None => format!("lost {}", self.lost),
        }
    }

    /// Where the Advanced headline stops: the furthest point this window measured.
    pub fn endpoint(&self) -> Option<Endpoint> {
        if self.e2e.is_measured() {
            Some(if self.on_glass {
                Endpoint::OnGlass
            } else {
                Endpoint::Displayed
            })
        } else if self.e2e_decoded.is_measured() {
            Some(Endpoint::Decoded)
        } else if self.host_net.is_measured() {
            Some(Endpoint::Received)
        } else {
            None
        }
    }

    fn floor_us(&self, pick: fn(&Summary) -> u32) -> u32 {
        if self.shave_os_floor && self.os_floor.is_measured() {
            pick(&self.os_floor)
        } else {
            0
        }
    }
}

/// Below this many frames in a window, loss shows as a count. A still screen on a
/// Windows host sends almost none, so one lost frame would read as tens of percent.
const THIN_WINDOW_FRAMES: u32 = 30;

fn pct(part: u32, whole: u32) -> f64 {
    if whole == 0 {
        0.0
    } else {
        f64::from(part) * 100.0 / f64::from(whole)
    }
}

/// How the overlay names a pad-slot mask; `None` when this session holds no pad. One
/// place, so no client invents its own numbering for which player it is.
///
/// Slot `n` is player `n + 1`: a local co-op game reads the OS slot order, and slot 0
/// is its P1.
pub fn player_label(slots: u16) -> Option<String> {
    let players: Vec<String> = (0..crate::input::MAX_PADS)
        .filter(|n| slots & (1 << n) != 0)
        .map(|n| (n + 1).to_string())
        .collect();
    match players.len() {
        0 => None,
        1 => Some(format!("player {}", players[0])),
        _ => Some(format!("players {}", players.join(" · "))),
    }
}

/// The codec as the overlay names it. Empty for a wire id this build does not know.
pub fn codec_label(codec: u8) -> &'static str {
    match codec {
        crate::quic::CODEC_H264 => "H.264",
        crate::quic::CODEC_HEVC => "HEVC",
        crate::quic::CODEC_AV1 => "AV1",
        crate::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "",
    }
}

/// Samples beyond this are stale stamps, not latency.
const SANE_NS: i128 = 10_000_000_000;
/// Frames waiting for their 0xCF host timing. 256 ≈ 2 s at 120 Hz.
const PENDING_CAP: usize = 256;

#[derive(Default)]
struct Cursors {
    dropped: u64,
    fec: u64,
    mic_sent: u64,
    mic_dropped: u64,
}

impl Cursors {
    fn seed(c: &Counters) -> Cursors {
        Cursors {
            dropped: c.frames_dropped,
            fec: c.fec_recovered,
            mic_sent: c.mic_sent,
            mic_dropped: c.mic_dropped,
        }
    }
}

fn delta(total: u64, cursor: &mut u64) -> u32 {
    let d = total.saturating_sub(*cursor);
    *cursor = total;
    d.min(u64::from(u32::MAX)) as u32
}

struct Window {
    start: Instant,
    received: u32,
    bytes: u64,
    decoded: u32,
    presented: u32,
    e2e: Vec<u32>,
    e2e_decoded: Vec<u32>,
    host_net: Vec<u32>,
    host: Vec<u32>,
    net: Vec<u32>,
    decode: Vec<u32>,
    display: Vec<u32>,
    pace: Vec<u32>,
    latch: Vec<u32>,
    os_floor: Vec<u32>,
    host_queue: Vec<u32>,
    host_encode: Vec<u32>,
    host_xfer: Vec<u32>,
    host_pace: Vec<u32>,
    overlaps: bool,
    trimmed: u32,
    skipped: u32,
    skipped_overflow: u32,
    // Sticky for the session: a platform that counts once counts from then on.
    counts_decoded: bool,
    counts_presented: bool,
    counts_skipped: bool,
    // Survive a window boundary: a timing lands a frame or two after its AU.
    pending: VecDeque<(u64, u32)>,
    cursors: Cursors,
}

impl Window {
    fn new(now: Instant) -> Window {
        Window {
            start: now,
            received: 0,
            bytes: 0,
            decoded: 0,
            presented: 0,
            e2e: Vec::new(),
            e2e_decoded: Vec::new(),
            host_net: Vec::new(),
            host: Vec::new(),
            net: Vec::new(),
            decode: Vec::new(),
            display: Vec::new(),
            pace: Vec::new(),
            latch: Vec::new(),
            os_floor: Vec::new(),
            host_queue: Vec::new(),
            host_encode: Vec::new(),
            host_xfer: Vec::new(),
            host_pace: Vec::new(),
            overlaps: false,
            trimmed: 0,
            skipped: 0,
            skipped_overflow: 0,
            counts_decoded: false,
            counts_presented: false,
            counts_skipped: false,
            pending: VecDeque::with_capacity(PENDING_CAP),
            cursors: Cursors::default(),
        }
    }

    fn restart(&mut self, now: Instant) {
        self.start = now;
        self.received = 0;
        self.bytes = 0;
        self.decoded = 0;
        self.presented = 0;
        for v in [
            &mut self.e2e,
            &mut self.e2e_decoded,
            &mut self.host_net,
            &mut self.host,
            &mut self.net,
            &mut self.decode,
            &mut self.display,
            &mut self.pace,
            &mut self.latch,
            &mut self.os_floor,
            &mut self.host_queue,
            &mut self.host_encode,
            &mut self.host_xfer,
            &mut self.host_pace,
        ] {
            v.clear();
        }
        self.overlaps = false;
        self.trimmed = 0;
        self.skipped = 0;
        self.skipped_overflow = 0;
    }
}

/// `stamp + offset − pts`, µs, when it is a latency at all. `Err(true)` = impossible (≤ 0).
fn capture_anchored(stamp_ns: u64, offset_ns: i64, pts_ns: u64) -> Result<u32, bool> {
    let v = i128::from(stamp_ns) + i128::from(offset_ns) - i128::from(pts_ns);
    if v <= 0 {
        Err(true)
    } else if v >= SANE_NS {
        Err(false)
    } else {
        Ok((v / 1000) as u32)
    }
}

/// `later − earlier` on one clock, µs. A backwards step reads 0.
fn local(later_ns: u64, earlier_ns: u64) -> Option<u32> {
    let d = later_ns.saturating_sub(earlier_ns);
    (i128::from(d) < SANE_NS).then_some((d / 1000) as u32)
}

/// A window shared by every thread that sees a stamp. Cheap to note into: one relaxed load
/// while disabled, one uncontended lock while enabled.
pub struct Stats {
    enabled: AtomicBool,
    /// Host − client, ns. `0` = never measured.
    clock_offset: Arc<AtomicI64>,
    window: Mutex<Window>,
}

impl Stats {
    pub fn new(clock_offset: Arc<AtomicI64>) -> Stats {
        Stats {
            enabled: AtomicBool::new(true),
            clock_offset,
            window: Mutex::new(Window::new(Instant::now())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn enabled(&self) -> bool {
        self.live()
    }

    /// Turning sampling on opens a fresh window and re-seeds the counter cursors from `c`, so
    /// the first snapshot covers only time the overlay was up.
    pub fn set_enabled(&self, on: bool, c: &Counters) {
        let was = self.enabled.swap(on, Ordering::Relaxed);
        if on && !was {
            let mut w = self.lock();
            w.restart(Instant::now());
            w.cursors = Cursors::seed(c);
        }
    }

    /// One received AU, or one piece of one. Only the piece that `completes_au` counts a frame
    /// and stamps receipt.
    pub fn note_received(&self, pts_ns: u64, received_ns: u64, bytes: usize, completes_au: bool) {
        if !self.live() {
            return;
        }
        let offset = self.clock_offset.load(Ordering::Relaxed);
        let mut w = self.lock();
        w.bytes += bytes as u64;
        if !completes_au {
            return;
        }
        w.received = w.received.saturating_add(1);
        if received_ns == 0 {
            return;
        }
        match capture_anchored(received_ns, offset, pts_ns) {
            Ok(us) => {
                w.host_net.push(us);
                if w.pending.len() >= PENDING_CAP {
                    w.pending.pop_front();
                }
                w.pending.push_back((pts_ns, us));
            }
            Err(true) => w.trimmed = w.trimmed.saturating_add(1),
            Err(false) => {}
        }
    }

    /// The host's own share of one AU (0xCF), matched to its receipt by `pts_ns`.
    pub fn note_host_timing(&self, t: &crate::quic::HostTiming) {
        if !self.live() {
            return;
        }
        let mut w = self.lock();
        let Some(i) = w.pending.iter().rposition(|(p, _)| *p == t.pts_ns) else {
            return;
        };
        let Some((_, host_net)) = w.pending.remove(i) else {
            return;
        };
        w.host.push(t.host_us);
        w.net.push(host_net.saturating_sub(t.host_us));
        if let Some(s) = t.stages {
            w.host_queue.push(s.queue_us);
            w.host_encode.push(s.encode_us);
            w.host_pace.push(s.pace_us);
            // The residual (seal, FEC, channel wait) is what makes the four stages tile host_us.
            let named = s
                .queue_us
                .saturating_add(s.encode_us)
                .saturating_add(s.pace_us);
            w.host_xfer.push(t.host_us.saturating_sub(named));
        }
    }

    /// One frame out of the decoder. Counts it and stamps capture → decoded.
    pub fn note_decoded(&self, pts_ns: u64, decoded_ns: u64) {
        if !self.live() {
            return;
        }
        let offset = self.clock_offset.load(Ordering::Relaxed);
        let mut w = self.lock();
        w.counts_decoded = true;
        w.decoded = w.decoded.saturating_add(1);
        if let Ok(us) = capture_anchored(decoded_ns, offset, pts_ns) {
            w.e2e_decoded.push(us);
        }
    }

    /// One received → decoded duration. `overlaps_display`: the rung returns at submission,
    /// so this was fence-waited and the time also sits inside `display`.
    pub fn note_decode_us(&self, us: u64, overlaps_display: bool) {
        if !self.live() {
            return;
        }
        let mut w = self.lock();
        w.overlaps |= overlaps_display;
        w.decode.push(us.min(u64::from(u32::MAX)) as u32);
    }

    /// One frame reached the screen. `submitted_ns` is `0` when the platform cannot tell a
    /// present submit from the displayed instant.
    pub fn note_displayed(
        &self,
        pts_ns: u64,
        decoded_ns: u64,
        submitted_ns: u64,
        displayed_ns: u64,
    ) {
        if !self.live() {
            return;
        }
        let offset = self.clock_offset.load(Ordering::Relaxed);
        let mut w = self.lock();
        w.counts_presented = true;
        w.presented = w.presented.saturating_add(1);
        if let Ok(us) = capture_anchored(displayed_ns, offset, pts_ns) {
            w.e2e.push(us);
        }
        if decoded_ns == 0 {
            return;
        }
        if let Some(us) = local(displayed_ns, decoded_ns) {
            w.display.push(us);
        }
        if submitted_ns != 0 {
            if let (Some(p), Some(l)) = (
                local(submitted_ns, decoded_ns),
                local(displayed_ns, submitted_ns),
            ) {
                w.pace.push(p);
                w.latch.push(l);
            }
        }
    }

    /// One sample of the OS present pipeline depth.
    pub fn note_os_floor_us(&self, us: u64) {
        if !self.live() {
            return;
        }
        self.lock()
            .os_floor
            .push(us.min(u64::from(u32::MAX)) as u32);
    }

    /// Frames the client chose not to show: newer ones won (`pacing`), or the decoder fell
    /// behind and whole AUs were dropped before feeding (`overflow`).
    pub fn note_skipped(&self, pacing: u32, overflow: u32) {
        if !self.live() {
            return;
        }
        let mut w = self.lock();
        w.counts_skipped = true;
        w.skipped = w.skipped.saturating_add(pacing).saturating_add(overflow);
        w.skipped_overflow = w.skipped_overflow.saturating_add(overflow);
    }

    pub fn drain(&self, c: &Counters) -> StatsSnapshot {
        self.drain_at(c, Instant::now())
    }

    /// Close the window at `now`. Facts the window cannot see (mode, codec, decoder) stay at
    /// their defaults for the caller to fill.
    pub fn drain_at(&self, c: &Counters, now: Instant) -> StatsSnapshot {
        let offset = self.clock_offset.load(Ordering::Relaxed);
        let mut guard = self.lock();
        let w = &mut *guard;
        let elapsed = now
            .saturating_duration_since(w.start)
            .max(Duration::from_millis(1));
        let snap = StatsSnapshot {
            window_ms: elapsed.as_millis().min(u128::from(u32::MAX)) as u32,
            received: w.received,
            bytes: w.bytes,
            decoded: w.counts_decoded.then_some(w.decoded),
            presented: w.counts_presented.then_some(w.presented),
            target_kbps: c.target_kbps,
            rate_cut: RateCut::from_code(c.rate_cut),
            rfis_last_min: c.rfis_last_min,
            e2e: Summary::of(&mut w.e2e),
            e2e_decoded: Summary::of(&mut w.e2e_decoded),
            host_net: Summary::of(&mut w.host_net),
            host: Summary::of(&mut w.host),
            net: Summary::of(&mut w.net),
            decode: Summary::of(&mut w.decode),
            display: Summary::of(&mut w.display),
            pace: Summary::of(&mut w.pace),
            latch: Summary::of(&mut w.latch),
            os_floor: Summary::of(&mut w.os_floor),
            host_queue: Summary::of(&mut w.host_queue),
            host_encode: Summary::of(&mut w.host_encode),
            host_xfer: Summary::of(&mut w.host_xfer),
            host_pace: Summary::of(&mut w.host_pace),
            decode_overlaps_display: w.overlaps,
            same_host_clock: offset == 0,
            skew_trimmed: w.trimmed,
            lost: delta(c.frames_dropped, &mut w.cursors.dropped),
            skipped: w.counts_skipped.then_some(w.skipped),
            skipped_overflow: w.skipped_overflow,
            fec: delta(c.fec_recovered, &mut w.cursors.fec),
            mic_sent: delta(c.mic_sent, &mut w.cursors.mic_sent),
            mic_dropped: delta(c.mic_dropped, &mut w.cursors.mic_dropped),
            rtt_us: (c.rtt_us > 0).then_some(c.rtt_us),
            audio_buffer_ms: c.audio_buffer_ms,
            av_offset_ms: c.av_offset_ms,
            pad_slots: c.pad_slots,
            ..StatsSnapshot::default()
        };
        w.restart(now);
        snap
    }
}

/// The overlay for `s` at `tier`, in the Advanced vocabulary when `advanced`.
///
/// The player line leads, in both vocabularies and at every tier: which controller
/// this session is is a session fact, not a diagnostic, and a co-op guest reads it
/// before anything else. Silent unless the session holds a pad.
pub fn format(s: &StatsSnapshot, tier: StatsVerbosity, advanced: bool) -> Vec<HudLine> {
    if tier == StatsVerbosity::Off {
        return Vec::new();
    }
    let mut lines = if advanced {
        advanced_lines(s, tier)
    } else {
        standard_lines(s, tier)
    };
    if let Some(player) = player_label(s.pad_slots) {
        lines.insert(0, text(Role::Primary, player));
    }
    lines.extend(
        s.extras
            .iter()
            .filter(|e| tier >= e.tier && (advanced || !e.advanced_only))
            .map(|e| HudLine {
                role: e.role,
                text: e.text.clone(),
            }),
    );
    lines
}

/// Lines as one string: `sep` between them.
pub fn join(lines: &[HudLine], sep: &str) -> String {
    lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join(sep)
}

/// `<role code>\t<text>\n` per line: the form that crosses the C ABI, JNI and wasm.
pub fn encode_lines(lines: &[HudLine]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push(char::from(b'0' + l.role as u8));
        out.push('\t');
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

fn ms(us: u32) -> String {
    format!("{:.1}", f64::from(us) / 1000.0)
}

fn fps(v: f64) -> String {
    format!("{v:.0} fps")
}

fn mbps(v: f64) -> String {
    format!("{v:.1} Mb/s")
}

fn line(role: Role, fields: Vec<String>) -> HudLine {
    HudLine {
        role,
        text: fields.join(" · "),
    }
}

fn text(role: Role, text: String) -> HudLine {
    HudLine { role, text }
}

fn mode(s: &StatsSnapshot) -> String {
    format!("{}×{}@{}", s.width, s.height, s.refresh_hz)
}

/// Why Automatic is running below where it was, while that still holds, and how often
/// loss needed a repair in the last minute: a climbing count is a link not recovering.
fn rate_cut(s: &StatsSnapshot) -> Option<String> {
    s.rate_cut.filter(|_| s.auto_rate).map(|c| {
        format!(
            "bitrate lowered: {} · {} loss repairs/min",
            c.label(),
            s.rfis_last_min
        )
    })
}

fn target(s: &StatsSnapshot) -> Option<String> {
    (s.target_kbps > 0).then(|| {
        let mb = f64::from(s.target_kbps) / 1000.0;
        if s.auto_rate {
            format!("target {mb:.0} Mb/s (auto)")
        } else {
            format!("target {mb:.0} Mb/s")
        }
    })
}

/// Codec and depth, decoder, HDR, chroma: what decodes what.
fn media_fields(s: &StatsSnapshot, out: &mut Vec<String>) {
    let mut codec = s.codec.clone();
    if s.bit_depth > 0 {
        if !codec.is_empty() {
            codec.push(' ');
        }
        codec.push_str(&format!("{}-bit", s.bit_depth));
    }
    if !codec.is_empty() {
        out.push(codec);
    }
    if !s.decoder.is_empty() {
        out.push(s.decoder.clone());
    }
    if let Some(t) = s.hdr.tag() {
        out.push(t.into());
    }
    match (s.asked_444, s.chroma_444) {
        (_, true) => out.push("4:4:4".into()),
        (true, false) => out.push("4:4:4→4:2:0".into()),
        _ => {}
    }
}

/// 44.1 kHz family in tenths, from integer parts so no locale can print a comma.
fn khz(rate_hz: u32) -> String {
    let tenths = rate_hz % 1000 / 100;
    if tenths == 0 {
        format!("{} kHz", rate_hz / 1000)
    } else {
        format!("{}.{} kHz", rate_hz / 1000, tenths)
    }
}

/// The resolved lossless format. Silent on Opus, and when the host reported no format.
fn audio_format(s: &StatsSnapshot) -> Option<HudLine> {
    if !s.audio_lossless || s.audio_rate_hz == 0 || s.audio_bits == 0 {
        return None;
    }
    let mut t = format!(
        "audio lossless {} / {}-bit",
        khz(s.audio_rate_hz),
        s.audio_bits
    );
    match s.audio_channels {
        6 => t.push_str(" · 5.1"),
        8 => t.push_str(" · 7.1"),
        _ => {}
    }
    Some(text(Role::Muted, t))
}

fn audio_buffer(s: &StatsSnapshot) -> Option<HudLine> {
    if s.audio_buffer_ms == 0 {
        return None;
    }
    let mut t = format!("audio buffer {} ms", s.audio_buffer_ms);
    if s.av_offset_ms != 0 {
        t.push_str(&format!(" · a/v {:+} ms", s.av_offset_ms));
    }
    Some(text(Role::Muted, t))
}

fn mic(s: &StatsSnapshot) -> Option<HudLine> {
    if s.mic_sent == 0 && s.mic_dropped == 0 {
        return None;
    }
    let mut t = format!("mic {:.0} f/s", f64::from(s.mic_sent) / s.secs());
    if s.mic_dropped > 0 {
        t.push_str(&format!(" · dropped {}", s.mic_dropped));
    }
    Some(text(Role::Muted, t))
}

/// `(p50, p95, endpoint)` with the OS floor shaved where the platform asks for it.
fn headline(s: &StatsSnapshot) -> Option<(u32, u32, Endpoint)> {
    let ep = s.endpoint()?;
    let floor = s.floor_us(|f| f.p50_us);
    Some(match ep {
        Endpoint::OnGlass | Endpoint::Displayed => (
            s.e2e.p50_us.saturating_sub(floor),
            s.e2e.p95_us.saturating_sub(floor),
            ep,
        ),
        Endpoint::Decoded => (s.e2e_decoded.p50_us, s.e2e_decoded.p95_us, ep),
        Endpoint::Received => (s.host_net.p50_us, s.host_net.p95_us, ep),
    })
}

/// The stages that tile the headline interval, as p50s. They sum only roughly: percentiles
/// are not additive, and the headline is measured directly.
fn equation(s: &StatsSnapshot, ep: Endpoint) -> Option<HudLine> {
    let mut terms = Vec::new();
    if s.host.is_measured() {
        terms.push(format!("host {}", ms(s.host.p50_us)));
        terms.push(format!("network {}", ms(s.net.p50_us)));
    } else if s.host_net.is_measured() && ep != Endpoint::Received {
        terms.push(format!("host+network {}", ms(s.host_net.p50_us)));
    }
    if ep != Endpoint::Received && s.decode.is_measured() && !s.decode_overlaps_display {
        terms.push(format!("decode {}", ms(s.decode.p50_us)));
    }
    if matches!(ep, Endpoint::Displayed | Endpoint::OnGlass) && s.display.is_measured() {
        let floor = s.floor_us(|f| f.p50_us);
        let mut t = format!("display {}", ms(s.display.p50_us.saturating_sub(floor)));
        // With the floor shaved, display is already pace alone; the split would count latch twice.
        if floor == 0 && s.pace.is_measured() && s.latch.is_measured() {
            t.push_str(&format!(
                " (pace {} + latch {})",
                ms(s.pace.p50_us),
                ms(s.latch.p50_us)
            ));
        }
        terms.push(t);
    }
    if terms.is_empty() {
        return None;
    }
    let mut t = format!("= {}", terms.join(" + "));
    if let Some(p) = s.presented_fps() {
        t.push_str(&format!(" · presented {p:.0}"));
    }
    Some(text(Role::Detail, t))
}

fn advanced_lines(s: &StatsSnapshot, tier: StatsVerbosity) -> Vec<HudLine> {
    let head = headline(s);
    let mut out = Vec::new();
    if tier == StatsVerbosity::Compact {
        let mut f = vec![fps(s.received_fps())];
        if let Some((p50, _, _)) = head {
            f.push(format!("{} ms", ms(p50)));
        }
        f.push(mbps(s.mbps()));
        if s.lost > 0 {
            f.push(s.lost_label());
        }
        f.extend(s.preset.clone());
        out.push(line(Role::Primary, f));
        return out;
    }
    let detailed = tier == StatsVerbosity::Detailed;
    let mut l1 = vec![mode(s), fps(s.received_fps()), mbps(s.mbps())];
    if detailed {
        l1.extend(target(s));
        media_fields(s, &mut l1);
    }
    l1.extend(s.preset.clone());
    out.push(line(Role::Primary, l1));
    if let Some((p50, p95, ep)) = head {
        out.push(text(
            Role::Primary,
            format!(
                "end-to-end {} ms p50 · {} p95 · capture→{}{}",
                ms(p50),
                ms(p95),
                ep.label(),
                if s.same_host_clock {
                    " (same-host clock)"
                } else {
                    ""
                }
            ),
        ));
    }
    if detailed {
        out.extend(head.and_then(|(_, _, ep)| equation(s, ep)));
        if s.decode_overlaps_display && s.decode.is_measured() && s.decode.p50_us > 0 {
            let samples = match s.decode.n {
                1 => "1 sample".to_string(),
                n => format!("{n} samples"),
            };
            out.push(text(
                Role::Detail,
                format!(
                    "decode {} ms ({samples}, inside display — not additive)",
                    ms(s.decode.p50_us)
                ),
            ));
        }
        if s.host_queue.is_measured() {
            out.push(text(
                Role::Detail,
                format!(
                    "host: queue {} · encode {} · xfer {} · pace {} ms",
                    ms(s.host_queue.p50_us),
                    ms(s.host_encode.p50_us),
                    ms(s.host_xfer.p50_us),
                    ms(s.host_pace.p50_us)
                ),
            ));
        }
        let floor = s.floor_us(|f| f.p50_us);
        if floor > 0 {
            out.push(text(
                Role::Muted,
                format!(
                    "os present +{} excluded (display pipeline minimum)",
                    ms(floor)
                ),
            ));
        }
        if let Some(r) = s.rtt_us {
            out.push(text(Role::Muted, format!("rtt {} ms", ms(r))));
        }
        if s.skew_trimmed > 0 {
            out.push(text(
                Role::Warn,
                format!(
                    "clock offset suspect — {:.0}/s impossible samples trimmed; e2e & \
                     host+network unreliable",
                    f64::from(s.skew_trimmed) / s.secs()
                ),
            ));
        }
        out.extend(mic(s));
        out.extend(audio_buffer(s));
    }
    out.extend(audio_format(s));
    let mut counters = Vec::new();
    if s.lost > 0 {
        counters.push(match s.lost_share() {
            Some(p) => format!("lost {} ({p:.1}%)", s.lost),
            None => format!("lost {}", s.lost),
        });
    }
    if detailed {
        match s.skipped {
            Some(k) if k > 0 && s.skipped_overflow > 0 => {
                counters.push(format!("skipped {k} (⚠ {} overflow)", s.skipped_overflow))
            }
            Some(k) if k > 0 => counters.push(format!("skipped {k}")),
            _ => {}
        }
        if s.fec > 0 {
            counters.push(format!("FEC {}", s.fec));
        }
    }
    counters.extend(rate_cut(s));
    if !counters.is_empty() {
        let role = if s.lost > 0 || rate_cut(s).is_some() {
            Role::Warn
        } else {
            Role::Detail
        };
        out.push(line(role, counters));
    }
    out
}

fn standard_lines(s: &StatsSnapshot, tier: StatsVerbosity) -> Vec<HudLine> {
    let floor = s.floor_us(|f| f.mean_us);
    let display = s
        .display
        .is_measured()
        .then(|| s.display.mean_us.saturating_sub(floor));
    let mut out = Vec::new();
    if tier == StatsVerbosity::Compact {
        let mut f = vec![fps(s.received_fps()), mbps(s.mbps())];
        if s.decode.is_measured() {
            f.push(format!("decode {} ms", ms(s.decode.mean_us)));
        }
        if s.lost > 0 {
            f.push(s.lost_label());
        }
        f.extend(s.preset.clone());
        out.push(line(Role::Primary, f));
        return out;
    }
    let mut l1 = vec![mode(s)];
    media_fields(s, &mut l1);
    l1.extend(s.preset.clone());
    out.push(line(Role::Primary, l1));

    let mut rates = vec![format!("received {}", fps(s.received_fps()))];
    if let Some(d) = s.decoded_fps() {
        rates.push(format!("decoded {d:.0}"));
    }
    if let Some(p) = s.presented_fps() {
        rates.push(format!("presented {p:.0}"));
    }
    rates.push(mbps(s.mbps()));
    out.push(line(Role::Detail, rates));

    let mut times = Vec::new();
    if s.host.is_measured() {
        times.push(format!("host {} ms", ms(s.host.mean_us)));
    }
    if s.decode.is_measured() {
        times.push(format!("decode {} ms", ms(s.decode.mean_us)));
    }
    if let Some(d) = display {
        times.push(format!("display {} ms", ms(d)));
    }
    if !times.is_empty() {
        out.push(text(Role::Detail, format!("{} (avg)", times.join(" · "))));
    }

    let mut link = vec![s.lost_label()];
    if let Some(k) = s.skipped {
        let shown = s.decoded.unwrap_or(s.received);
        link.push(format!("skipped {:.1}%", pct(k, shown.max(k))));
    }
    if let Some(r) = s.rtt_us {
        link.push(format!("rtt {} ms", ms(r)));
    }
    out.push(line(
        if s.lost > 0 { Role::Warn } else { Role::Detail },
        link,
    ));

    if tier == StatsVerbosity::Detailed {
        let mut spread = Vec::new();
        if s.host.is_measured() {
            spread.push(format!(
                "host min/max {}/{} ms",
                ms(s.host.min_us),
                ms(s.host.max_us)
            ));
        }
        if floor == 0 && s.pace.is_measured() && s.latch.is_measured() {
            spread.push(format!(
                "display queue {} + render {} ms (incl. vsync)",
                ms(s.pace.mean_us),
                ms(s.latch.mean_us)
            ));
        }
        if !spread.is_empty() {
            out.push(line(Role::Detail, spread));
        }
        out.extend(target(s).map(|t| text(Role::Detail, t)));
        out.extend(mic(s));
        out.extend(audio_buffer(s));
    }
    out.extend(audio_format(s));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(n: u32, p50_us: u32, p95_us: u32) -> Summary {
        Summary {
            n,
            mean_us: p50_us,
            min_us: p50_us,
            max_us: p95_us,
            p50_us,
            p95_us,
            p99_us: p95_us,
        }
    }

    /// The desktop shape: glass stamps, 0xCF stages, a synchronous decode rung.
    fn desktop() -> StatsSnapshot {
        StatsSnapshot {
            window_ms: 1000,
            width: 1920,
            height: 1080,
            refresh_hz: 120,
            codec: "HEVC".into(),
            bit_depth: 10,
            hdr: Hdr::ToneMapped,
            decoder: "native-vulkan".into(),
            received: 120,
            bytes: 3_037_500,
            decoded: Some(120),
            presented: Some(119),
            e2e: sum(119, 6400, 9100),
            e2e_decoded: sum(120, 4000, 6000),
            host_net: sum(120, 2100, 3000),
            host: sum(120, 1200, 1800),
            net: sum(120, 900, 1200),
            decode: sum(120, 1800, 2500),
            display: sum(119, 1100, 1500),
            host_queue: sum(120, 300, 400),
            host_encode: sum(120, 500, 700),
            host_xfer: sum(120, 100, 200),
            host_pace: sum(120, 300, 400),
            on_glass: true,
            lost: 3,
            fec: 0,
            ..StatsSnapshot::default()
        }
    }

    fn texts(s: &StatsSnapshot, tier: StatsVerbosity, advanced: bool) -> Vec<String> {
        format(s, tier, advanced)
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    fn all(s: &StatsSnapshot, tier: StatsVerbosity, advanced: bool) -> String {
        join(&format(s, tier, advanced), "\n")
    }

    #[test]
    fn advanced_tiers_on_the_desktop_shape() {
        let s = desktop();
        use StatsVerbosity::*;
        assert!(format(&s, Off, true).is_empty());
        assert_eq!(
            texts(&s, Compact, true),
            ["120 fps · 6.4 ms · 24.3 Mb/s · lost 2.4%"]
        );
        assert_eq!(
            texts(&s, Normal, true),
            [
                "1920×1080@120 · 120 fps · 24.3 Mb/s",
                "end-to-end 6.4 ms p50 · 9.1 p95 · capture→on-glass",
                "lost 3 (2.4%)",
            ]
        );
        let detailed = texts(&s, Detailed, true);
        assert_eq!(
            detailed[0],
            "1920×1080@120 · 120 fps · 24.3 Mb/s · HEVC 10-bit · native-vulkan · HDR→SDR"
        );
        assert_eq!(
            detailed[2],
            "= host 1.2 + network 0.9 + decode 1.8 + display 1.1 · presented 119"
        );
        assert!(detailed.contains(&"host: queue 0.3 · encode 0.5 · xfer 0.1 · pace 0.3 ms".into()));
        assert_eq!(detailed.last().unwrap(), "lost 3 (2.4%)");
    }

    #[test]
    fn glass_stamps_split_display_into_pace_and_latch() {
        let mut s = desktop();
        s.display = sum(119, 12_400, 14_000);
        s.pace = sum(119, 1100, 1300);
        s.latch = sum(119, 11_300, 12_000);
        assert!(all(&s, StatsVerbosity::Detailed, true)
            .contains("display 12.4 (pace 1.1 + latch 11.3)"));
        // Without glass stamps the unsplit figure stands alone rather than a zero latch.
        s.latch = Summary::default();
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("pace 1.1"));
    }

    #[test]
    fn an_overlapping_decode_leaves_the_equation_and_says_so() {
        let mut s = desktop();
        s.decode_overlaps_display = true;
        s.decode = sum(1, 1800, 1800);
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(d.contains("= host 1.2 + network 0.9 + display 1.1"), "{d}");
        assert!(d.contains("\ndecode 1.8 ms (1 sample, inside display — not additive)"));
        // A fence wait that timed out is no measurement, not an instant decode.
        s.decode = sum(1, 0, 0);
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("decode"));
    }

    #[test]
    fn an_old_host_keeps_the_combined_stage() {
        let mut s = desktop();
        s.host = Summary::default();
        s.net = Summary::default();
        s.host_queue = Summary::default();
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(
            d.contains("= host+network 2.1 + decode 1.8 + display 1.1"),
            "{d}"
        );
        assert!(!d.contains("host:"));
    }

    /// iOS, tvOS and Android: the OS present floor comes off the headline and `display`,
    /// and Detailed names what came off.
    #[test]
    fn a_shaved_floor_is_named() {
        let mut s = desktop();
        s.on_glass = false;
        s.shave_os_floor = true;
        s.e2e = sum(119, 30_900, 35_000);
        s.display = sum(119, 19_000, 21_000);
        s.pace = sum(119, 2300, 2600);
        s.latch = sum(119, 16_700, 17_000);
        s.os_floor = sum(119, 16_700, 17_000);
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(
            d.contains("end-to-end 14.2 ms p50 · 18.3 p95 · capture→displayed"),
            "{d}"
        );
        assert!(d.contains("+ display 2.3 ·"), "shaved, and no split: {d}");
        assert!(d.contains("\nos present +16.7 excluded (display pipeline minimum)"));
        assert_eq!(
            texts(&s, StatsVerbosity::Compact, true)[0],
            "120 fps · 14.2 ms · 24.3 Mb/s · lost 2.4%"
        );
        assert!(!all(&s, StatsVerbosity::Normal, true).contains("os present"));
    }

    #[test]
    fn the_headline_stops_where_the_window_stopped() {
        let mut s = desktop();
        s.e2e = Summary::default();
        s.display = Summary::default();
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(
            d.contains("end-to-end 4.0 ms p50 · 6.0 p95 · capture→decoded"),
            "{d}"
        );
        assert!(d.contains("= host 1.2 + network 0.9 + decode 1.8"));
        assert!(!d.contains("display"));

        // Apple's fallback presenter and webOS: nothing past receipt.
        s.e2e_decoded = Summary::default();
        s.decode = Summary::default();
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(
            d.contains("end-to-end 2.1 ms p50 · 3.0 p95 · capture→received"),
            "{d}"
        );
        assert!(d.contains("= host 1.2 + network 0.9"));
        s.host = Summary::default();
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("= "));

        s.host_net = Summary::default();
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("end-to-end"));
    }

    #[test]
    fn an_unmeasured_offset_says_so_once() {
        let mut s = desktop();
        s.same_host_clock = true;
        let n = all(&s, StatsVerbosity::Detailed, true);
        assert_eq!(n.matches("(same-host clock)").count(), 1);
        assert!(n.contains("capture→on-glass (same-host clock)"));
    }

    #[test]
    fn a_lying_clock_is_flagged() {
        let mut s = desktop();
        s.skew_trimmed = 40;
        s.window_ms = 2000;
        let d = texts(&s, StatsVerbosity::Detailed, true);
        let warn = format(&s, StatsVerbosity::Detailed, true)
            .into_iter()
            .find(|l| l.role == Role::Warn && l.text.starts_with("clock"))
            .unwrap();
        assert_eq!(
            warn.text,
            "clock offset suspect — 20/s impossible samples trimmed; e2e & host+network unreliable"
        );
        assert!(d.len() > 3);
    }

    #[test]
    fn detailed_names_target_and_chroma() {
        let mut s = desktop();
        let l1 = |s: &StatsSnapshot| texts(s, StatsVerbosity::Detailed, true)[0].clone();
        s.target_kbps = 200_000;
        assert!(l1(&s).contains("24.3 Mb/s · target 200 Mb/s · HEVC"));
        s.auto_rate = true;
        s.target_kbps = 20_000;
        assert!(l1(&s).contains("target 20 Mb/s (auto)"));
        assert!(!texts(&s, StatsVerbosity::Normal, true)[0].contains("target"));
        (s.asked_444, s.chroma_444) = (true, true);
        assert!(l1(&s).ends_with("· 4:4:4"));
        s.chroma_444 = false;
        assert!(l1(&s).ends_with("· 4:4:4→4:2:0"));
        s.asked_444 = false;
        assert!(!l1(&s).contains("4:4:4"));
    }

    #[test]
    fn hdr_tags() {
        let mut s = desktop();
        for (hdr, tag) in [
            (Hdr::Hdr, " · HDR"),
            (Hdr::ToneMapped, " · HDR→SDR"),
            (Hdr::Untonemapped, " · HDR→SDR (raw)"),
        ] {
            s.hdr = hdr;
            assert!(texts(&s, StatsVerbosity::Detailed, true)[0].ends_with(tag));
        }
        s.hdr = Hdr::Sdr;
        assert!(!texts(&s, StatsVerbosity::Detailed, true)[0].contains("HDR"));
    }

    #[test]
    fn the_preset_closes_the_first_line_at_every_tier() {
        let mut s = desktop();
        s.preset = Some("Work".into());
        for adv in [false, true] {
            for tier in [
                StatsVerbosity::Compact,
                StatsVerbosity::Normal,
                StatsVerbosity::Detailed,
            ] {
                assert!(
                    texts(&s, tier, adv)[0].ends_with(" · Work"),
                    "{tier:?} {adv}"
                );
            }
        }
        s.preset = None;
        assert!(!all(&s, StatsVerbosity::Normal, true).contains(" ·  "));
    }

    #[test]
    fn audio_lines() {
        let mut s = desktop();
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("audio"));
        s.audio_lossless = true;
        s.audio_rate_hz = 96_000;
        s.audio_bits = 24;
        assert!(all(&s, StatsVerbosity::Normal, true).contains("\naudio lossless 96 kHz / 24-bit"));
        assert!(all(&s, StatsVerbosity::Normal, false).contains("\naudio lossless 96 kHz / 24-bit"));
        assert!(!all(&s, StatsVerbosity::Compact, true).contains("audio"));
        s.audio_rate_hz = 44_100;
        s.audio_channels = 6;
        assert!(all(&s, StatsVerbosity::Normal, true)
            .contains("audio lossless 44.1 kHz / 24-bit · 5.1"));
        s.audio_rate_hz = 176_400;
        assert!(all(&s, StatsVerbosity::Normal, true).contains("176.4 kHz"));
        s.audio_rate_hz = 0;
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("lossless"));

        s.audio_buffer_ms = 28;
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("\naudio buffer 28 ms"));
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("a/v"));
        s.av_offset_ms = 4;
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("audio buffer 28 ms · a/v +4 ms"));
        s.av_offset_ms = -3;
        assert!(all(&s, StatsVerbosity::Detailed, false).contains("a/v -3 ms"));
        assert!(!all(&s, StatsVerbosity::Normal, true).contains("audio buffer"));
    }

    #[test]
    fn mic_line_only_while_voice_goes_out() {
        let mut s = desktop();
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("mic"));
        s.mic_sent = 100;
        let d = all(&s, StatsVerbosity::Detailed, true);
        assert!(d.contains("\nmic 100 f/s") && !d.contains("dropped"));
        s.mic_dropped = 7;
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("mic 100 f/s · dropped 7"));
        assert!(!all(&s, StatsVerbosity::Normal, true).contains("mic"));
    }

    /// Automatic names why it runs lower in Advanced stats, and only while that holds.
    #[test]
    fn a_rate_cut_is_named_while_automatic_holds_it() {
        let mut s = desktop();
        s.lost = 0;
        s.auto_rate = true;
        s.rate_cut = Some(RateCut::Delay);
        let lines = format(&s, StatsVerbosity::Normal, true);
        let cut = lines
            .iter()
            .find(|l| {
                l.text
                    .contains("bitrate lowered: network delay · 0 loss repairs/min")
            })
            .expect("the cut is named at Normal");
        assert_eq!(cut.role, Role::Warn);
        s.rfis_last_min = 7;
        assert!(all(&s, StatsVerbosity::Normal, true).contains("delay · 7 loss repairs/min"));
        assert!(!all(&s, StatsVerbosity::Detailed, false).contains("lowered"));
        assert!(!all(&s, StatsVerbosity::Detailed, false).contains("repairs"));
        s.auto_rate = false;
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("lowered"));
        s.auto_rate = true;
        s.rate_cut = None;
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("lowered"));
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("repairs"));
        assert_eq!(
            RateCut::from_code(RateCut::Encoder as u8),
            Some(RateCut::Encoder)
        );
        assert_eq!(RateCut::from_code(0), None);
    }

    #[test]
    fn detailed_counters() {
        let mut s = desktop();
        s.skipped = Some(1);
        s.fec = 12;
        assert!(all(&s, StatsVerbosity::Detailed, true)
            .contains("\nlost 3 (2.4%) · skipped 1 · FEC 12"));
        assert!(all(&s, StatsVerbosity::Normal, true).ends_with("\nlost 3 (2.4%)"));
        s.skipped = Some(4);
        s.skipped_overflow = 3;
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("skipped 4 (⚠ 3 overflow)"));
        s.lost = 0;
        s.skipped = Some(0);
        s.fec = 0;
        assert!(!all(&s, StatsVerbosity::Detailed, true).contains("lost"));
        assert!(!all(&s, StatsVerbosity::Compact, true).contains("lost"));
    }

    /// A still screen sends few frames; one loss there shows as a count, not a share.
    #[test]
    fn thin_window_counts_loss() {
        let mut s = desktop();
        s.received = 4;
        s.lost = 1;
        for advanced in [true, false] {
            for tier in [
                StatsVerbosity::Compact,
                StatsVerbosity::Normal,
                StatsVerbosity::Detailed,
            ] {
                let text = all(&s, tier, advanced);
                assert!(text.contains("lost 1"), "{tier:?} {advanced}: {text}");
                assert!(!text.contains("lost 20.0%"), "{tier:?} {advanced}: {text}");
                assert!(!text.contains("(20.0%)"), "{tier:?} {advanced}: {text}");
            }
        }
        s.received = 29;
        assert!(all(&s, StatsVerbosity::Compact, true).contains("lost 3.3%"));
    }

    #[test]
    fn standard_speaks_moonlights_slice() {
        let mut s = desktop();
        s.host.mean_us = 3100;
        s.host.min_us = 2100;
        s.host.max_us = 6300;
        s.decode.mean_us = 2100;
        s.display.mean_us = 2300;
        s.pace = sum(119, 1200, 1400);
        s.latch = sum(119, 1100, 1300);
        s.rtt_us = Some(4200);
        s.skipped = Some(0);
        s.target_kbps = 30_000;
        s.auto_rate = true;
        use StatsVerbosity::*;
        assert_eq!(
            texts(&s, Compact, false),
            ["120 fps · 24.3 Mb/s · decode 2.1 ms · lost 2.4%"]
        );
        assert_eq!(
            texts(&s, Normal, false),
            [
                "1920×1080@120 · HEVC 10-bit · native-vulkan · HDR→SDR",
                "received 120 fps · decoded 120 · presented 119 · 24.3 Mb/s",
                "host 3.1 ms · decode 2.1 ms · display 2.3 ms (avg)",
                "lost 2.4% · skipped 0.0% · rtt 4.2 ms",
            ]
        );
        let d = texts(&s, Detailed, false);
        assert_eq!(
            d[4],
            "host min/max 2.1/6.3 ms · display queue 1.2 + render 1.1 ms (incl. vsync)"
        );
        assert_eq!(d[5], "target 30 Mb/s (auto)");
    }

    /// D2: the switch exists to hold end-to-end back, at every tier.
    #[test]
    fn standard_never_shows_end_to_end() {
        let s = desktop();
        for tier in StatsVerbosity::ALL {
            let t = all(&s, tier, false);
            assert!(
                !t.contains("end-to-end") && !t.contains("p50"),
                "{tier:?}: {t}"
            );
        }
    }

    /// D1: under jitter a mean sits above the median, so Standard never reads better than
    /// Moonlight's averages.
    #[test]
    fn standard_reports_means() {
        let mut samples = vec![1000, 1000, 1000, 1000, 9000];
        let sm = Summary::of(&mut samples);
        assert!(sm.mean_us > sm.p50_us);
        let mut s = desktop();
        s.decode = sm;
        assert!(all(&s, StatsVerbosity::Normal, false).contains("decode 2.6 ms"));
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("decode 1.0"));
    }

    #[test]
    fn standard_omits_what_the_platform_cannot_see() {
        let mut s = desktop();
        s.decoded = None;
        s.presented = None;
        s.host = Summary::default();
        s.decode = Summary::default();
        s.display = Summary::default();
        let n = texts(&s, StatsVerbosity::Normal, false);
        assert_eq!(n[1], "received 120 fps · 24.3 Mb/s");
        assert_eq!(n[2], "lost 2.4%");
        assert_eq!(
            texts(&s, StatsVerbosity::Compact, false),
            ["120 fps · 24.3 Mb/s · lost 2.4%"]
        );
    }

    #[test]
    fn extras_obey_tier_and_vocabulary() {
        let mut s = desktop();
        s.extras = vec![
            Extra::detail("present: fifo · vrr yes"),
            Extra {
                text: "⚠ panel 60 Hz, not 120".into(),
                tier: StatsVerbosity::Compact,
                advanced_only: false,
                role: Role::Warn,
            },
        ];
        let c = format(&s, StatsVerbosity::Compact, false);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].role, Role::Warn);
        assert!(!all(&s, StatsVerbosity::Detailed, false).contains("present:"));
        assert!(all(&s, StatsVerbosity::Detailed, true).ends_with("\n⚠ panel 60 Hz, not 120"));
        assert!(all(&s, StatsVerbosity::Detailed, true).contains("\npresent: fifo · vrr yes"));
        assert!(!all(&s, StatsVerbosity::Normal, true).contains("present:"));
    }

    /// Every field a Normal line shows sits on some Detailed line, in both vocabularies.
    #[test]
    fn detailed_is_a_superset_of_normal() {
        let mut shapes = vec![desktop()];
        let mut a = desktop();
        a.shave_os_floor = true;
        a.os_floor = sum(119, 16_700, 17_000);
        a.skipped = Some(2);
        a.audio_lossless = true;
        a.audio_rate_hz = 48_000;
        a.audio_bits = 24;
        a.preset = Some("Game".into());
        shapes.push(a);
        let mut received_only = desktop();
        received_only.e2e = Summary::default();
        received_only.e2e_decoded = Summary::default();
        received_only.decoded = None;
        received_only.presented = None;
        received_only.same_host_clock = true;
        received_only.lost = 0;
        shapes.push(received_only);
        for s in &shapes {
            for adv in [false, true] {
                let detailed: Vec<Vec<String>> = texts(s, StatsVerbosity::Detailed, adv)
                    .iter()
                    .map(|l| l.split(" · ").map(str::to_string).collect())
                    .collect();
                for normal in texts(s, StatsVerbosity::Normal, adv) {
                    let fields: Vec<&str> = normal.split(" · ").collect();
                    assert!(
                        detailed
                            .iter()
                            .any(|d| fields.iter().all(|f| d.iter().any(|x| x == f))),
                        "{normal:?} missing from Detailed (advanced {adv})"
                    );
                }
            }
        }
    }

    #[test]
    fn lines_encode_with_their_role() {
        let lines = [
            HudLine {
                role: Role::Primary,
                text: "a · b".into(),
            },
            HudLine {
                role: Role::Warn,
                text: "lost 3".into(),
            },
        ];
        assert_eq!(encode_lines(&lines), "0\ta · b\n3\tlost 3\n");
        assert_eq!(Role::from_code(3), Role::Warn);
        assert_eq!(Role::from_code(9), Role::Primary);
    }

    #[test]
    fn tiers_round_trip_their_index() {
        for t in StatsVerbosity::ALL {
            assert_eq!(StatsVerbosity::from_index(t.index()), t);
        }
        assert_eq!(StatsVerbosity::from_index(77), StatsVerbosity::Normal);
        assert_eq!(StatsVerbosity::Detailed.next(), StatsVerbosity::Off);
    }

    #[test]
    fn summary_percentiles() {
        assert_eq!(Summary::of(&mut []), Summary::default());
        let one = Summary::of(&mut [7]);
        assert_eq!((one.n, one.p50_us, one.p95_us, one.mean_us), (1, 7, 7, 7));
        let two = Summary::of(&mut [9, 1]);
        assert_eq!(
            (two.min_us, two.p50_us, two.max_us, two.mean_us),
            (1, 9, 9, 5)
        );
        let mut many: Vec<u32> = (1..=1000).rev().collect();
        let m = Summary::of(&mut many);
        assert_eq!(
            (m.p50_us, m.p95_us, m.p99_us, m.min_us, m.max_us),
            (501, 951, 991, 1, 1000)
        );
    }

    const S: u64 = 1_000_000_000;

    fn stats(offset: i64) -> Stats {
        Stats::new(Arc::new(AtomicI64::new(offset)))
    }

    #[test]
    fn the_window_measures_every_stage() {
        let st = stats(5 * S as i64); // host clock runs 5 s ahead of ours
        let t0 = Instant::now();
        let host = |ms: u64| 100 * S + ms * 1_000_000; // host capture clock
        let local = |ms: u64| 95 * S + ms * 1_000_000; // our clock
        for i in 0..10u64 {
            let pts = host(i * 10);
            st.note_received(pts, local(i * 10 + 3), 1000, true);
            st.note_decoded(pts, local(i * 10 + 5));
            st.note_decode_us(2000, false);
            st.note_displayed(pts, local(i * 10 + 5), local(i * 10 + 6), local(i * 10 + 9));
            st.note_host_timing(&crate::quic::HostTiming {
                pts_ns: pts,
                host_us: 1000,
                stages: Some(crate::quic::HostStages {
                    queue_us: 200,
                    encode_us: 500,
                    pace_us: 100,
                }),
                applied_phase_ns: None,
            });
        }
        let s = st.drain_at(
            &Counters {
                frames_dropped: 2,
                fec_recovered: 5,
                rtt_us: 900,
                target_kbps: 20_000,
                ..Counters::default()
            },
            t0 + Duration::from_millis(1000),
        );
        assert_eq!(s.received, 10);
        assert_eq!((s.decoded, s.presented), (Some(10), Some(10)));
        assert_eq!(s.host_net.p50_us, 3000);
        assert_eq!(s.e2e_decoded.p50_us, 5000);
        assert_eq!(s.e2e.p50_us, 9000);
        assert_eq!(
            (s.display.p50_us, s.pace.p50_us, s.latch.p50_us),
            (4000, 1000, 3000)
        );
        assert_eq!((s.host.p50_us, s.net.p50_us), (1000, 2000));
        assert_eq!(s.host_xfer.p50_us, 200);
        assert_eq!(s.decode.p50_us, 2000);
        assert_eq!((s.lost, s.fec, s.rtt_us), (2, 5, Some(900)));
        assert!(!s.same_host_clock);
        assert_eq!(s.skew_trimmed, 0);
        assert_eq!(s.bytes, 10_000);

        // Counters are windowed, samples reset, stickiness survives.
        let s2 = st.drain(&Counters {
            frames_dropped: 3,
            fec_recovered: 5,
            ..Counters::default()
        });
        assert_eq!((s2.lost, s2.fec, s2.received), (1, 0, 0));
        assert_eq!(
            (s2.decoded, s2.presented, s2.skipped),
            (Some(0), Some(0), None)
        );
        assert!(!s2.e2e.is_measured());
    }

    #[test]
    fn slices_count_one_frame_and_a_late_timing_still_matches() {
        let st = stats(0);
        st.note_received(100 * S, 0, 500, false);
        st.note_received(100 * S, 100 * S + 2_000_000, 700, true);
        let s = st.drain(&Counters::default());
        assert_eq!((s.received, s.bytes, s.host_net.p50_us), (1, 1200, 2000));
        assert!(s.same_host_clock);
        // The 0xCF for that frame lands after the window closed: still matched.
        st.note_host_timing(&crate::quic::HostTiming {
            pts_ns: 100 * S,
            host_us: 1500,
            stages: None,
            applied_phase_ns: None,
        });
        let s = st.drain(&Counters::default());
        assert_eq!((s.host.p50_us, s.net.p50_us), (1500, 500));
        assert!(!s.host_queue.is_measured());
    }

    #[test]
    fn impossible_samples_are_counted_not_kept() {
        let st = stats(-10 * S as i64);
        st.note_received(100 * S, 100 * S + 1_000_000, 10, true);
        let s = st.drain(&Counters::default());
        assert_eq!((s.skew_trimmed, s.host_net.n), (1, 0));
    }

    #[test]
    fn a_disabled_window_notes_nothing_and_reopens_clean() {
        let st = stats(0);
        let c = Counters {
            frames_dropped: 50,
            ..Counters::default()
        };
        st.set_enabled(false, &c);
        st.note_received(100 * S, 100 * S + 1_000_000, 10, true);
        st.note_skipped(3, 1);
        assert_eq!(st.drain(&c).received, 0);
        st.set_enabled(true, &c);
        st.note_skipped(3, 1);
        let s = st.drain(&Counters {
            frames_dropped: 52,
            ..Counters::default()
        });
        assert_eq!((s.lost, s.skipped, s.skipped_overflow), (2, Some(4), 1));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn snapshots_round_trip_as_json() {
        let mut s = desktop();
        s.extras.push(Extra::detail("present: mailbox"));
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<StatsSnapshot>(&json).unwrap(), s);
        // A reader older than a field still parses a newer writer's line.
        let partial: StatsSnapshot = serde_json::from_str(r#"{"received":5,"future":1}"#).unwrap();
        assert_eq!(partial.received, 5);
    }

    /// Slot 0 is Player 1, and the line leads the overlay so a co-op guest reads it
    /// first. Silent for the sessions that hold no pad, which is most of them.
    #[test]
    fn the_player_line_names_every_slot_this_session_holds() {
        assert_eq!(player_label(0), None);
        assert_eq!(player_label(0b1).as_deref(), Some("player 1"));
        assert_eq!(player_label(0b10).as_deref(), Some("player 2"));
        assert_eq!(player_label(0b1010).as_deref(), Some("players 2 · 4"));

        let mut s = StatsSnapshot {
            window_ms: 1000,
            received: 60,
            ..StatsSnapshot::default()
        };
        assert!(!format(&s, StatsVerbosity::Compact, false)[0]
            .text
            .starts_with("player"));
        s.pad_slots = 0b10;
        for advanced in [true, false] {
            for tier in [
                StatsVerbosity::Compact,
                StatsVerbosity::Normal,
                StatsVerbosity::Detailed,
            ] {
                assert_eq!(format(&s, tier, advanced)[0].text, "player 2");
            }
        }
        assert!(format(&s, StatsVerbosity::Off, false).is_empty());
    }
}
