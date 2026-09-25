//! Shared audio layout: Opus (multi)stream surround for the host, GameStream, and every
//! client decoder.
//!
//! Wire order is `FL FR FC LFE RL RR SL SR` (GameStream/Moonlight and the PipeWire/PulseAudio
//! 6/8 map). Capturers and decoders both use it; the Opus multistream `mapping` only decides
//! which slots share a coupled stream ([`AudioLayout`]), never the order samples come out in.
//! GFE pre-rotation (`gamestream::audio::surround_params`) is GameStream-only; it never
//! touches `punktfunk/1`.
//!
//! Negotiated counts: `2`, `6`, `8`. Anything else clamps to stereo ([`normalize_channels`]).
//! Opus is 48 kHz; the lossless plane is a second plane — [`pcm`], `design/hi-res-audio.md`.

pub mod pcm;

use std::time::Duration;

/// This client silenced its own speakers (`client::NativeClient::set_audio_muted`). The host
/// keeps sending, so a session joined to the same sink still hears the game.
pub const AUDIO_MUTE_LOCAL: u8 = 1 << 0;
/// The operator muted this session from the console (`quic::AudioState`). The host
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

/// How long a mute the player made themselves names itself on screen.
pub const LOCAL_MUTE_NOTICE: Duration = Duration::from_secs(5);

/// [`audio_mute_label`] with the badge's lifetime applied, `since` the mask last changed.
///
/// A host mute stands for the whole session: an operator silencing a client must not be
/// able to hide behind a local unmute. A local mute is the player's own press from the dial
/// that still shows its state, so it says so long enough to read and then leaves the picture
/// alone — a standing badge over the game is the operator's language, not the player's.
pub fn audio_mute_notice(mask: u8, since: Duration) -> Option<&'static str> {
    if mask & AUDIO_MUTE_HOST == 0 && since >= LOCAL_MUTE_NOTICE {
        return None;
    }
    audio_mute_label(mask)
}

/// Slot in the interleaved PCM frame. A count of N uses `0..N` of this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WirePos {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    Lfe = 3,
    RearLeft = 4,
    RearRight = 5,
    SideLeft = 6,
    SideRight = 7,
}

/// The full 8-channel wire order; the N-channel order is its first N entries.
pub const WIRE_ORDER_8: [WirePos; 8] = {
    use WirePos::*;
    [
        FrontLeft,
        FrontRight,
        FrontCenter,
        Lfe,
        RearLeft,
        RearRight,
        SideLeft,
        SideRight,
    ]
};

/// How a surround session couples its Opus streams. One table row per (count, layout):
/// [`layout_for`]. Negotiated per session — [`Hello::audio_layout`](crate::quic::Hello::audio_layout)
/// asks, [`Welcome::audio_layout`](crate::quic::Welcome::audio_layout) answers with what the
/// host encodes — and `0` on the wire, or no byte at all, is [`Self::Legacy`], so an older
/// peer on either side keeps today's stream. Coupling changes which slots share a stream,
/// never the order samples come out in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum AudioLayout {
    /// (FL,FR)+(FC,LFE) coupled, (RL,RR) too on 7.1, the rest mono. What punktfunk/1 shipped.
    #[default]
    Legacy = 0,
    /// (FL,FR)+(RL,RR) coupled, (SL,SR) too on 7.1, FC and LFE mono — RFC 7845 mapping family
    /// 1, Sunshine's, and the one layout LG's NDL decoder takes.
    Standard = 1,
    /// One mono stream per channel at a fixed high bitrate (GameStream `AudioQuality=1`).
    Uncoupled = 2,
}

impl AudioLayout {
    /// The wire id, or `None` for one this build does not know — never decode with a guess.
    pub fn from_wire(id: u8) -> Option<AudioLayout> {
        match id {
            0 => Some(AudioLayout::Legacy),
            1 => Some(AudioLayout::Standard),
            2 => Some(AudioLayout::Uncoupled),
            _ => None,
        }
    }

    pub fn wire(self) -> u8 {
        self as u8
    }
}

/// One Opus (multi)stream layout. Both native ends use [`WIRE_ORDER_8`]; `mapping` is the
/// permutation that pairs slots into coupled streams ([`AudioLayout`]). Stereo is 128 kbps;
/// the rest match Sunshine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusLayout {
    pub channels: u8,
    pub streams: u8,
    pub coupled: u8,
    /// libopus multistream mapping: `mapping[slot]` is the stream channel that feeds wire slot
    /// `slot`. Identity except on [`AudioLayout::Standard`].
    pub mapping: &'static [u8],
    /// [`AudioTier::Standard`] bitrate, bits/sec. GameStream encodes hard-CBR from this (FEC
    /// needs a constant packet size); native uses constrained VBR.
    pub bitrate: i32,
}

pub const LAYOUT_STEREO: OpusLayout = OpusLayout {
    channels: 2,
    streams: 1,
    coupled: 1,
    mapping: &[0, 1],
    bitrate: 128_000,
};
/// 5.1 normal quality: (FL,FR)+(FC,LFE) coupled, RL+RR mono.
pub const LAYOUT_51: OpusLayout = OpusLayout {
    channels: 6,
    streams: 4,
    coupled: 2,
    mapping: &[0, 1, 2, 3, 4, 5],
    bitrate: 256_000,
};
/// 5.1 standard: (FL,FR)+(RL,RR) coupled, FC and LFE mono. Same bitrate as [`LAYOUT_51`].
pub const LAYOUT_51_STANDARD: OpusLayout = OpusLayout {
    channels: 6,
    streams: 4,
    coupled: 2,
    mapping: &[0, 1, 4, 5, 2, 3],
    bitrate: 256_000,
};
/// 5.1 high quality: uncoupled, one stream per channel.
pub const LAYOUT_51_HQ: OpusLayout = OpusLayout {
    channels: 6,
    streams: 6,
    coupled: 0,
    mapping: &[0, 1, 2, 3, 4, 5],
    bitrate: 1_536_000,
};
/// 7.1 normal quality: (FL,FR)+(FC,LFE)+(RL,RR) coupled, SL+SR mono.
pub const LAYOUT_71: OpusLayout = OpusLayout {
    channels: 8,
    streams: 5,
    coupled: 3,
    mapping: &[0, 1, 2, 3, 4, 5, 6, 7],
    bitrate: 450_000,
};
/// 7.1 standard: (FL,FR)+(RL,RR)+(SL,SR) coupled, FC and LFE mono. Same bitrate as [`LAYOUT_71`].
pub const LAYOUT_71_STANDARD: OpusLayout = OpusLayout {
    channels: 8,
    streams: 5,
    coupled: 3,
    mapping: &[0, 1, 6, 7, 2, 3, 4, 5],
    bitrate: 450_000,
};
/// 7.1 high quality: uncoupled, one stream per channel.
pub const LAYOUT_71_HQ: OpusLayout = OpusLayout {
    channels: 8,
    streams: 8,
    coupled: 0,
    mapping: &[0, 1, 2, 3, 4, 5, 6, 7],
    bitrate: 2_048_000,
};

/// Encode bitrate for the desktop-audio downlink. The layout table's `bitrate` is
/// [`AudioTier::Standard`].
///
/// 5 ms Opus frames are less efficient than 20 ms, so 128 kbps stereo here is roughly 100 kbps
/// at 20 ms. Video is tens of Mbps; 256 kbps audio is ~1 % of that budget, so [`AudioTier::High`]
/// is the default. Lower tiers are for a constrained link.
///
/// Host-side only: libopus reads the bitrate from the packet. A tier change needs no negotiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AudioTier {
    /// Constrained links. Lossy on music; fine for game/voice.
    Low,
    /// The layout table's `bitrate` (stereo 128 kbps).
    Standard,
    /// The default. Transparent at 5 ms frames; ~1 % of a normal video budget.
    #[default]
    High,
}

impl AudioTier {
    /// Parse a config/CLI spelling (`low` / `standard` / `high`). `None` for anything else so the
    /// caller can warn and fall back rather than silently changing the tier.
    pub fn parse(s: &str) -> Option<AudioTier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(AudioTier::Low),
            "standard" | "normal" | "medium" => Some(AudioTier::Standard),
            "high" => Some(AudioTier::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioTier::Low => "low",
            AudioTier::Standard => "standard",
            AudioTier::High => "high",
        }
    }
}

impl OpusLayout {
    /// Target bitrate at `tier`. HQ layouts ([`LAYOUT_51_HQ`] / [`LAYOUT_71_HQ`]) are already
    /// past transparency, so they ignore the tier.
    pub fn bitrate_for(&self, tier: AudioTier) -> i32 {
        if self.coupled == 0 && self.streams == self.channels {
            return self.bitrate;
        }
        match (self.channels, tier) {
            (6, AudioTier::Low) => 192_000,
            (6, AudioTier::High) => 448_000,
            (8, AudioTier::Low) => 320_000,
            (8, AudioTier::High) => 768_000,
            (_, AudioTier::Low) => 96_000,
            (_, AudioTier::High) => 256_000,
            (_, AudioTier::Standard) => self.bitrate,
        }
    }
}

/// Encode tier and whether the redundant `0xD2` plane fits. From [`plan_audio_budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioBudget {
    pub tier: AudioTier,
    pub redundancy: bool,
    pub kbps: u32,
}

/// Share of the session video bitrate audio may spend. Audio rides QUIC datagrams outside ABR,
/// so whatever it takes is taken off the top and cannot be reclaimed.
const AUDIO_BUDGET_PCT: u32 = 5;
/// Never encode below Low. Unintelligible audio is worse than spending a few percent more.
const AUDIO_BUDGET_FLOOR_KBPS: u32 = 96;

/// Choose encode tier and redundancy from the session's resolved VIDEO bitrate.
///
/// The ladder is preference, not cost: transparent audio beats redundant audio (redundancy only
/// pays under loss), so `High` alone outranks `Standard` + redundancy at the same cost.
/// `requested` is a ceiling: the budget may lower the tier, never raise it. `layout` prices
/// the session's coupling: [`AudioLayout::Uncoupled`] ignores the tier.
pub fn plan_audio_budget(
    video_kbps: u32,
    channels: u8,
    layout: AudioLayout,
    requested: AudioTier,
    client_wants_redundancy: bool,
) -> AudioBudget {
    let budget = (video_kbps.saturating_mul(AUDIO_BUDGET_PCT) / 100).max(AUDIO_BUDGET_FLOOR_KBPS);
    let layout = layout_for(channels, layout);
    let cost = |tier: AudioTier, red: bool| -> u32 {
        let one = (layout.bitrate_for(tier) / 1000).max(0) as u32;
        if red {
            one.saturating_mul(2)
        } else {
            one
        }
    };
    // Rank is a ceiling: a request of `Low` must not be handed `High`.
    let rank = |t: AudioTier| match t {
        AudioTier::Low => 0,
        AudioTier::Standard => 1,
        AudioTier::High => 2,
    };
    let ladder = [
        (AudioTier::High, true),
        (AudioTier::High, false),
        (AudioTier::Standard, true),
        (AudioTier::Standard, false),
        (AudioTier::Low, false),
    ];
    for (tier, red) in ladder {
        if rank(tier) > rank(requested) || (red && !client_wants_redundancy) {
            continue;
        }
        let kbps = cost(tier, red);
        if kbps <= budget {
            return AudioBudget {
                tier,
                redundancy: red,
                kbps,
            };
        }
    }
    // Nothing fit. Encode Low rather than mute.
    AudioBudget {
        tier: AudioTier::Low,
        redundancy: false,
        kbps: cost(AudioTier::Low, false),
    }
}

/// Layout for a negotiated channel count and coupling. Unknown counts fall back to stereo,
/// which every [`AudioLayout`] shares.
pub fn layout_for(channels: u8, layout: AudioLayout) -> &'static OpusLayout {
    use AudioLayout::{Legacy, Standard, Uncoupled};
    match (channels, layout) {
        (6, Legacy) => &LAYOUT_51,
        (6, Standard) => &LAYOUT_51_STANDARD,
        (6, Uncoupled) => &LAYOUT_51_HQ,
        (8, Legacy) => &LAYOUT_71,
        (8, Standard) => &LAYOUT_71_STANDARD,
        (8, Uncoupled) => &LAYOUT_71_HQ,
        _ => &LAYOUT_STEREO,
    }
}

/// Clamp to a negotiable count: 2, 6, or 8.
pub fn normalize_channels(requested: u8) -> u8 {
    match requested {
        6 => 6,
        8 => 8,
        _ => 2,
    }
}

/// Loss detector for the client audio plane, shared by every platform decoder.
///
/// `0xC9` datagrams carry a wrapping per-packet sequence on the lossy plane, with no FEC. Each
/// received sequence tells the decoder how many packets were missing immediately before it, so
/// it can run that many frames of libopus PLC (`decode` with empty input) first.
///
/// Reorders and duplicates conceal nothing (no reorder buffer). A gap is capped at
/// [`MAX_CONCEAL_MS`]; past that, libopus PLC has faded to silence and the ring underrun path
/// takes over.
#[derive(Debug)]
pub struct AudioGapTracker {
    last_seq: Option<u32>,
    /// One frame in microseconds; turns [`MAX_CONCEAL_MS`] into a packet cap. [`FRAME_MS`] until set.
    frame_us: u32,
}

impl Default for AudioGapTracker {
    fn default() -> Self {
        AudioGapTracker {
            last_seq: None,
            frame_us: FRAME_MS * 1000,
        }
    }
}

/// Longest gap one loss event conceals, in milliseconds — not a packet count, so a 2 ms lossless
/// frame still buys 50 ms. Same family as [`DroughtConceal::new_at_frame_us`].
///
/// Crate-internal: callers see [`AudioGapTracker::missing_before`]'s already-capped count.
/// Not part of the C ABI; cbindgen must not export this.
pub(crate) const MAX_CONCEAL_MS: u32 = 50;

/// [`MAX_CONCEAL_MS`] as a packet count at `frame_us`: 10 at 5 ms, 25 at 2 ms. Floors at 1 so a
/// zero cap cannot disable concealment.
///
/// `pub(crate)` so the PCM decoder in `abi.rs` can size its no-realloc buffer from the same
/// frame length. Buffer and cap must agree on how many frames can arrive at once.
pub(crate) const fn max_conceal_packets(frame_us: u32) -> u32 {
    let us = if frame_us == 0 {
        FRAME_MS * 1000
    } else {
        frame_us
    };
    let n = MAX_CONCEAL_MS * 1000 / us;
    if n == 0 {
        1
    } else {
        n
    }
}

impl AudioGapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cap seq-gap PLC at 50 ms of `frame_us`. [`new`](Self::new) is the Opus 5 ms default.
    pub fn new_at_frame_us(frame_us: u32) -> Self {
        let mut t = Self::new();
        t.set_frame_us(frame_us);
        t
    }

    /// Frame length in microseconds — `audio_frame_us` on a lossless session, [`FRAME_MS`] on Opus.
    /// Known after `Welcome`; [`new_at_frame_us`](Self::new_at_frame_us) when the value is already in hand.
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.frame_us = frame_us.max(1);
    }

    /// Packets missing immediately before `seq` (`0` for in-order, first, duplicates, reorders),
    /// capped at [`MAX_CONCEAL_MS`]. A sequence in the backward half of u32 is a reorder, not a
    /// 2³¹ gap.
    pub fn missing_before(&mut self, seq: u32) -> u32 {
        let Some(last) = self.last_seq else {
            self.last_seq = Some(seq);
            return 0;
        };
        let delta = seq.wrapping_sub(last);
        if delta == 0 || delta > u32::MAX / 2 {
            return 0; // duplicate, or a reorder older than the newest
        }
        self.last_seq = Some(seq);
        (delta - 1).min(max_conceal_packets(self.frame_us))
    }
}

/// Drops audio a later packet already superseded. A reordered or duplicate datagram lands after
/// its slot was concealed or rebuilt from `0xD2`; decoding it plays that slot twice, out of order,
/// and feeds the decoder a stale packet.
#[derive(Debug, Default)]
pub struct AudioSeqGate {
    newest: Option<u32>,
}

impl AudioSeqGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` for the first packet and anything newer than the newest passed (wrap-aware).
    pub fn fresh(&mut self, seq: u32) -> bool {
        let fresh = self
            .newest
            .is_none_or(|n| (1..=u32::MAX / 2).contains(&seq.wrapping_sub(n)));
        if fresh {
            self.newest = Some(seq);
        }
        fresh
    }
}

/// Rebuilds the stream from the redundant `0xD2` plane so a single lost datagram is recovered,
/// not concealed.
///
/// Lives in core on the demux side so every embedder sees a complete stream and
/// [`AudioGapTracker`] stops seeing the gap. Only the immediately-preceding frame can be
/// recovered — that is all the wire carries ([`crate::quic::encode_audio_red_datagram`]). A
/// longer burst still conceals, one frame shorter.
#[derive(Debug, Default)]
pub struct AudioRedRecovery {
    last_seq: Option<u32>,
}

impl AudioRedRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the redundant copy should be emitted as `seq - 1` BEFORE this packet.
    /// Reorders, duplicates, and the first packet of a session recover nothing.
    pub fn recover_before(&mut self, seq: u32, has_prev: bool) -> bool {
        let recover = match self.last_seq {
            // First packet of the session: inserting the predecessor would prepend audio
            // the client never missed.
            None => false,
            Some(last) => {
                let delta = seq.wrapping_sub(last);
                // `delta == 1` is in-order. `delta >= 2` in the forward half means at least
                // the predecessor is missing.
                has_prev && (2..u32::MAX / 2).contains(&delta)
            }
        };
        self.last_seq = Some(match self.last_seq {
            // A reorder must not move the anchor backwards.
            Some(last) if seq.wrapping_sub(last) > u32::MAX / 2 => last,
            _ => seq,
        });
        recover
    }
}

// ---- playback de-jitter -------------------------------------------------------------------

/// Opus-plane frame length in milliseconds, and the default. One datagram carries exactly one
/// ([`crate::quic::encode_audio_datagram`]), so it is also the smallest shed unit.
///
/// The lossless plane negotiates shorter frames ([`pcm::frame_us_for`]); the resolved value is
/// `audio_frame_us` on `Welcome`. Exported to C as `PUNKTFUNK_AUDIO_FRAME_MS` and kept at 5 —
/// embedders size rings from it. Sizing as *frames × this* is wrong by up to 2.5× on lossless;
/// drain [`crate::abi::punktfunk_connection_next_audio_pcm`] and use `frame_count` instead.
pub const FRAME_MS: u32 = 5;

/// Tuning for [`JitterPolicy`], in milliseconds. Depth is time, not device quanta: `3 × quantum`
/// is 15 ms at 5 ms and 64 ms at 20 ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterTuning {
    /// Depth to prime to before the first sample plays, and the depth drift correction pulls
    /// back toward. The adaptive floor may raise the live target above this; it never goes below.
    pub base_target_ms: u32,
    pub max_target_ms: u32,
    /// Slack above the live target before the depth AVERAGE is trimmed back. Sheds at the middle
    /// of this band ([`JitterTuning::shed_excess_ms`]). If the trim point sits below the shed
    /// point, drift correction never fires and every correction is the audible drop it replaced.
    pub headroom_ms: u32,
    /// Absolute bound on buffered audio, trimmed on sight. Wide enough to hold one delivery
    /// clump: a bunching link parks ~100 ms at once, and that audio is what bridges the hole.
    pub hard_cap_ms: u32,
    /// Starvation before the ring re-primes, in milliseconds — not a callback count. A floor of
    /// `MIN_DEPRIME_CALLBACKS` still applies so a large-quantum device keeps real hysteresis.
    /// `1` is `if ring.is_empty() { primed = false }`: one drain manufactures a whole target
    /// of silence.
    pub deprime_ms: u32,
}

impl JitterTuning {
    /// PipeWire adaptively rate-matches the stream to the graph clock and absorbs a shallow ring,
    /// so Linux can run tight.
    pub const PIPEWIRE: JitterTuning = JitterTuning {
        base_target_ms: 15,
        max_target_ms: 60,
        headroom_ms: 25,
        hard_cap_ms: 160,
        deprime_ms: 40,
    };
    /// WASAPI shared-mode event-driven render: the engine buffers for us, but nothing rate-matches.
    pub const WASAPI: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 180,
        deprime_ms: 50,
    };
    /// CoreAudio via AVAudioEngine. Same engine as WASAPI; longer `deprime_ms` because wireless
    /// clients stall more. See [`AAUDIO`].
    ///
    /// [`AAUDIO`]: JitterTuning::AAUDIO
    pub const COREAUDIO: JitterTuning = JitterTuning {
        base_target_ms: 20,
        max_target_ms: 70,
        headroom_ms: 30,
        hard_cap_ms: 180,
        deprime_ms: 60,
    };
    /// AAudio: raw realtime callback, we own the buffer. Starts at 25 ms; the adaptive floor
    /// raises it only on devices that underrun.
    pub const AAUDIO: JitterTuning = JitterTuning {
        base_target_ms: 25,
        max_target_ms: 90,
        headroom_ms: 40,
        hard_cap_ms: 240,
        deprime_ms: 60,
    };

    /// Longest packet drought to conceal before the ring may underrun. Twice `deprime_ms`: long
    /// enough to ride a delivery stall, short enough not to paper over a dead stream. Derived so
    /// it cannot drift from the fuse it protects.
    pub const fn plc_max_ms(&self) -> u32 {
        self.deprime_ms * 2
    }

    /// Depth-average excess that arms drift shed: middle of the headroom band, never less than
    /// two protocol frames. Derived from `headroom_ms` so the shed stays strictly below the trim.
    pub const fn shed_excess_ms(&self) -> u32 {
        let half = self.headroom_ms / 2;
        if half > 2 * FRAME_MS {
            half
        } else {
            2 * FRAME_MS
        }
    }
}

// Drought thresholds (quiet-wire duration and ring-empty floor) are two protocol frames, so they
// live on `DroughtConceal` as `after()`/`floor_ms()` rather than constants. `2 * FRAME_MS` waits
// five frames on a 2 ms lossless frame.

/// Bounded concealment of a packet drought. Client-side twin of the host capture-hole infill
/// (`design/host-source-stutter-fixes.md`).
///
/// [`AudioGapTracker`] conceals a sequence gap once a later packet arrives. A quiet wire
/// reveals nothing: the ring drains, [`JitterPolicy::note_read`] de-primes, and the re-prime
/// is a whole target of silence. A drought that is draining the ring is concealed from the
/// same decoder state, bounded in time (never frames or callbacks). Time is passed in so the
/// policy stays syscall-free.
pub struct DroughtConceal {
    /// Frames concealed since the last real packet — what [`packet`](Self::packet) returns, and
    /// the unit that survives a non-5 ms frame. See [`new_at_frame_us`](Self::new_at_frame_us).
    concealed: u32,
    max_ms: u32,
    frame_us: u32,
    /// Session concealment, for the 10 s `plc_ms=` line. A policy that papers over a failing
    /// link must be visible.
    total: u64,
}

impl DroughtConceal {
    /// At the protocol's default frame ([`FRAME_MS`]).
    pub fn new(max_ms: u32) -> DroughtConceal {
        Self::new_at_frame_us(max_ms, FRAME_MS * 1000)
    }

    /// At an explicitly negotiated frame length. Charges one frame per concealed frame and bounds
    /// itself in wall-clock milliseconds, so the two have to agree on how long a frame is.
    pub fn new_at_frame_us(max_ms: u32, frame_us: u32) -> DroughtConceal {
        DroughtConceal {
            concealed: 0,
            max_ms,
            frame_us: frame_us.max(1),
            total: 0,
        }
    }

    /// How long a drought must last before it is concealed at all. Two frames, so an ordinary
    /// inter-packet gap is never mistaken for a stall.
    fn after(&self) -> std::time::Duration {
        std::time::Duration::from_micros(2 * self.frame_us as u64)
    }

    /// Ring depth below which a drought is worth concealing, in ms. A drought a deep ring can
    /// cover is not audible, and concealing it would synthesize audio the late packets are about
    /// to duplicate.
    fn floor_ms(&self) -> u32 {
        (2 * self.frame_us).div_ceil(1000)
    }

    /// A packet arrived. Returns frames already concealed so the caller can subtract them from
    /// [`AudioGapTracker`]: loss inside a covered drought must not be covered twice.
    pub fn packet(&mut self) -> u32 {
        std::mem::take(&mut self.concealed)
    }

    /// Conceal one more frame? `depth_ms` is the playout ring as the callback last saw it.
    pub fn conceal(&mut self, since_last_packet: std::time::Duration, depth_ms: u32) -> bool {
        if since_last_packet < self.after()
            || depth_ms > self.floor_ms()
            || self.concealed_ms() >= self.max_ms
        {
            return false;
        }
        self.concealed += 1;
        self.total += 1;
        true
    }

    fn concealed_ms(&self) -> u32 {
        (self.concealed as u64 * self.frame_us as u64 / 1000) as u32
    }

    pub fn total_ms(&self) -> u64 {
        self.total * self.frame_us as u64 / 1000
    }
}

/// What one callback should do, from [`JitterPolicy::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct JitterStep {
    pub drop_front: usize,
    /// Samples to duplicate at the front ([`crossfade_insert`]). Never set in the same step as
    /// `drop_front`. How the ring gets deeper without a re-prime: one crossfaded frame at a time.
    /// De-prime stays for genuine starvation and for growth that was never banked (`hollow`).
    pub insert_front: usize,
    /// Linear crossfade across the `drop_front` / `insert_front` seam ([`crossfade_drop`] /
    /// [`crossfade_insert`]). Zero only when nothing moved. Both hard-cap trims and drift sheds
    /// fade: the samples either side of the seam are ordinary continuous audio.
    pub crossfade: usize,
    /// `drop_front` was the hard-cap backstop, not the smooth shed. Both fade; the flag is for
    /// logs and tests (sheds are the policy working, trims are the link outrunning the headroom).
    pub hard_trim: bool,
    /// Emit silence this callback: still priming, or re-priming after a sustained drain.
    pub silence: bool,
}

/// EWMA time constant for the depth average, in ms. Long enough that a burst does not trigger a
/// shed; short enough to track real drift.
const EWMA_TAU_MS: u32 = 1_000;
/// Consumed audio the depth EWMA must sit above the shed threshold. A shed is the only
/// correction a listener could notice, so it must never fire on a transient.
const SHED_SUSTAIN_MS: u32 = 2_000;
/// Mirror of the shed for a sync-driven insert. Equal time constants so the pair cannot fight.
/// Own constant so the insert can be sped up without touching the shed. Not `pub`: cbindgen.
const INSERT_SUSTAIN_MS: u32 = SHED_SUSTAIN_MS;
/// How far below the sync-requested target the depth average must sit before the insert arms.
/// Half [`AV_DEADBAND_MS`]: a margin at or above the deadband would leave every request the
/// loop is allowed to make unanswered. With the shed on the other side, the settling zone is
/// at least `shed_excess + this` wide.
const INSERT_MARGIN_MS: u32 = AV_DEADBAND_MS / 2;
const _: () = assert!(INSERT_MARGIN_MS < AV_DEADBAND_MS);
/// Linear crossfade across a drift shed. 2 ms is a fraction of a 5 ms Opus frame and the whole
/// of a 2 ms lossless one. [`JitterPolicy::set_frame_us`] caps the fade at half a frame.
const SHED_CROSSFADE_MS: u32 = 2;
/// Underruns inside [`GROW_WINDOW_MS`] before the live target grows.
const GROW_UNDERRUNS: u32 = 3;
const GROW_WINDOW_MS: u32 = 5_000;
const GROW_STEP_MS: u32 = 10;
/// Quiet time (no underrun) before a grown target relaxes one step back toward the base.
const SHRINK_QUIET_MS: u32 = 30_000;
/// The same, while the A/V sync loop is asking for a shallower ring. See the branch in
/// [`JitterPolicy::note_read`] that selects between them.
const SHRINK_QUIET_SYNC_MS: u32 = 5_000;
// The near-miss margin is one protocol frame, so it is [`JitterPolicy::frame_samples`] rather
// than a constant. See the use site in `step`.
/// How long a shrink remains a probe, in consumed audio. An underrun or near-miss inside this
/// window means the shrink was wrong; the previous target is restored at once instead of being
/// re-learned three audible underruns at a time.
const SHRINK_PROBE_MS: u32 = 5_000;
/// A ring is hollow when its depth average sits this far below the target: the target promises
/// a depth the ring does not hold. Growth only raises the promise; an underrun in a hollow ring
/// re-primes at once. A full ring's underrun (one packet a few ms late) keeps the hysteresis.
const DEPRIME_DEBT_MS: u32 = GROW_STEP_MS;
/// Floor under `JitterTuning::deprime_ms`, in callbacks. A device whose quantum is already
/// `deprime_ms` would otherwise de-prime on the first short read. Not `pub`: cbindgen would
/// export it unprefixed next to `PUNKTFUNK_AUDIO_*`.
const MIN_DEPRIME_CALLBACKS: u32 = 2;
// A de-prime on the first short read is the defect the hysteresis exists to prevent, so hold
// the floor at build time rather than in a test: tuning it to 1 should not compile.
const _: () = assert!(MIN_DEPRIME_CALLBACKS >= 2);
/// How long a failed probe blocks another sync-driven shrink. Doubles per consecutive failure
/// up to [`SYNC_BACKOFF_MAX_MS`]; a probe that survives its window resets it.
const SYNC_BACKOFF_MS: u32 = 60_000;
const SYNC_BACKOFF_MAX_MS: u32 = 480_000;

// ---- ms ⇄ interleaved-sample conversion ---------------------------------------------------
// Multiply first, divide last. `per_ms = rate_hz / 1000 * channels` truncates 44 100 Hz to 44
// samples/ms and puts every depth 2.3 % low. 48/96 kHz only look exact because they divide.
// See `design/hi-res-audio.md`.

/// Interleaved samples per second at a negotiated layout — the denominator both conversions
/// share. `u64` because it is a factor in products that reach 10¹² below.
const fn interleaved_per_sec(rate_hz: u32, channels: u8) -> u64 {
    // `max(1)` on both: a degenerate layout must not divide by zero in a realtime callback.
    // The constructors already clamp.
    let hz = if rate_hz == 0 { 1 } else { rate_hz };
    let ch = if channels == 0 { 1 } else { channels };
    hz as u64 * ch as u64
}

/// `ms` milliseconds of audio, in interleaved samples.
///
/// u64 intermediates: [`SYNC_BACKOFF_MAX_MS`] at 176 400 Hz × 8 ch is 6.8 × 10¹¹ before the
/// divide. Saturating: a wrapped window would fire immediately instead of never.
fn ms_to_samples(rate_hz: u32, channels: u8, ms: u32) -> usize {
    let n = ms as u64 * interleaved_per_sec(rate_hz, channels) / 1000;
    if n > u32::MAX as u64 {
        u32::MAX as usize
    } else {
        n as usize
    }
}

/// Interleaved samples back to whole milliseconds — the inverse of [`ms_to_samples`], so
/// `depth_ms(target)` round-trips to `target_ms()` at every rate.
///
/// u128 because `samples` arrives unbounded through [`JitterPolicy::depth_ms`]:
/// `usize::MAX * 1000` overflows a u64 on a 64-bit target.
fn samples_to_ms(rate_hz: u32, channels: u8, samples: usize) -> u32 {
    let ms = samples as u128 * 1000 / interleaved_per_sec(rate_hz, channels) as u128;
    if ms > u32::MAX as u128 {
        u32::MAX
    } else {
        ms as u32
    }
}

/// Playback de-jitter state machine shared by every client's audio ring.
///
/// Rings that only prime up and clamp at a ceiling ratchet latency under burst, stall, or DAC
/// skew. A depth EWMA above target for [`SHED_SUSTAIN_MS`] sheds one frame with a crossfade.
/// The mirror: when A/V sync asks for a deeper ring, an EWMA [`INSERT_MARGIN_MS`] below the
/// request for [`INSERT_SUSTAIN_MS`] duplicates one frame ([`JitterStep::insert_front`]).
///
/// Driven by samples consumed, not wall clock: allocation-free, syscall-free, deterministic
/// under test.
#[derive(Clone, Debug)]
pub struct JitterPolicy {
    tuning: JitterTuning,
    /// Negotiated rate and interleaved channel count, kept as two numbers rather than pre-divided
    /// into samples-per-ms. Both clamped to ≥ 1 by the constructor.
    rate_hz: u32,
    channels: u8,
    /// One protocol frame, microseconds. [`FRAME_MS`] for Opus; lossless negotiates shorter
    /// ([`pcm::frame_us_for`]). Default is the Opus 5 ms frame.
    frame_us: u32,
    target: usize,
    primed: bool,
    /// Consecutive short reads, and the audio they starved, in interleaved samples. Both gate
    /// de-prime: ≥ [`JitterTuning::deprime_ms`] AND ≥ [`MIN_DEPRIME_CALLBACKS`] callbacks.
    empties: u32,
    empties_run: usize,
    depth_avg: f32,
    over_run: usize,
    /// Consumed samples the EWMA has sat below the sync target by more than [`INSERT_MARGIN_MS`].
    under_run: usize,
    underruns: u32,
    window_run: usize,
    quiet_run: usize,
    /// `want` from the last [`step`](Self::step), so [`note_read`](Self::note_read) can tick
    /// sample-denominated timers without the caller repeating it.
    last_want: usize,
    /// Depth the A/V sync loop wants ([`AvSync::desired_depth`]). `None` = unsynchronised.
    sync_target: Option<usize>,
    /// Set by [`step`](Self::step) when the read it authorised leaves less than
    /// one protocol frame buffered; consumed by [`note_read`](Self::note_read).
    near_miss: bool,
    /// A near-miss already grew the target this window. One step, so a run of near-misses
    /// while the ring refills does not sprint to the ceiling.
    near_miss_grown: bool,
    /// Depth average more than [`DEPRIME_DEBT_MS`] below the adaptive target (never the
    /// sync-inflated one), so an underrun should re-prime at once.
    hollow: bool,
    probe_run: usize,
    probe_prev_target: usize,
    /// Consumed samples before the sync loop may drive another shrink (`0` = allowed).
    sync_backoff_run: usize,
    sync_backoff_ms: u32,
}

impl JitterPolicy {
    /// `channels` is 2/6/8 at [`SAMPLE_RATE_HZ`]. Hi-res uses [`new_at_rate`](Self::new_at_rate).
    pub fn new(tuning: JitterTuning, channels: u8) -> JitterPolicy {
        Self::new_at_rate(tuning, channels, SAMPLE_RATE_HZ)
    }

    /// As [`new`](Self::new), at an explicitly negotiated `rate_hz`.
    ///
    /// Conversions multiply first and divide last, so 44 100 / 48 000 / 88 200 / 96 000 / 176 400
    /// are exact. The tell that a change has broken it: [`depth_ms`](Self::depth_ms)`(target)`
    /// not round-tripping to [`target_ms`](Self::target_ms) — asserted in
    /// `the_shipping_rate_ladder_round_trips_ms_to_samples_exactly`.
    ///
    /// A `rate_hz` or `channels` of zero is clamped to 1: this type is built from wire values on
    /// a path that must not panic in a realtime callback.
    pub fn new_at_rate(tuning: JitterTuning, channels: u8, rate_hz: u32) -> JitterPolicy {
        let rate_hz = rate_hz.max(1);
        let channels = channels.max(1);
        JitterPolicy {
            tuning,
            rate_hz,
            channels,
            frame_us: FRAME_MS * 1000,
            target: ms_to_samples(rate_hz, channels, tuning.base_target_ms),
            primed: false,
            empties: 0,
            empties_run: 0,
            depth_avg: 0.0,
            over_run: 0,
            under_run: 0,
            underruns: 0,
            window_run: 0,
            quiet_run: 0,
            last_want: 0,
            sync_target: None,
            near_miss: false,
            near_miss_grown: false,
            hollow: false,
            probe_run: 0,
            probe_prev_target: 0,
            sync_backoff_run: 0,
            sync_backoff_ms: SYNC_BACKOFF_MS,
        }
    }

    /// Frame length in microseconds.
    ///
    /// The floor under the effective target (quantum plus one frame) and the smooth shed (drop
    /// exactly one frame) are denominated in frames. Default [`FRAME_MS`] is the Opus frame;
    /// `audio_frame_us` is known only after `Welcome`.
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.frame_us = frame_us.max(1);
    }

    fn ms_samples(&self, ms: u32) -> usize {
        ms_to_samples(self.rate_hz, self.channels, ms)
    }

    fn samples_ms(&self, samples: usize) -> u32 {
        samples_to_ms(self.rate_hz, self.channels, samples)
    }

    /// One frame in interleaved samples.
    ///
    /// From [`pcm::samples_per_frame`], the source the host fills from. At 44 100 Hz a 5 ms frame
    /// is 220 samples/channel, not 220.5 — computing 441 where the wire delivers 440 puts the
    /// near-miss margin and the shed one sample off the packet they describe. Computed in µs so
    /// 2 500 µs at 48 kHz stereo is 240 samples, not 192.
    fn frame_samples(&self) -> usize {
        pcm::samples_per_frame(self.rate_hz, self.frame_us, self.channels).max(1)
    }

    /// The seam crossfade, capped at half a frame. [`SHED_CROSSFADE_MS`]'s flat 2 ms is a
    /// fraction of a 5 ms Opus frame and the whole of a 2 ms lossless one. A fade as long as
    /// the material it is fading is not a crossfade.
    fn crossfade_samples(&self) -> usize {
        self.ms_samples(SHED_CROSSFADE_MS)
            .min(self.frame_samples() / 2)
    }

    /// Depth the A/V sync loop wants ([`AvSync::desired_depth`]), or `None` to run unsynchronised.
    /// A request, not a command: [`effective_target`](Self::effective_target) clamps it between
    /// the underrun-driven floor and the hard cap, so sync can never starve the ring.
    pub fn set_sync_target(&mut self, target: Option<usize>) {
        self.sync_target = target;
    }

    fn sync_wants_less(&self) -> bool {
        self.sync_target.is_some_and(|s| s < self.target)
    }

    /// Sync is asking for a deeper ring than the adaptive target. Without a request the policy
    /// never adds depth on its own, so `sync_target == None` never inserts.
    fn sync_wants_more(&self) -> bool {
        self.sync_target.is_some_and(|s| s > self.target)
    }

    pub fn target_ms(&self) -> u32 {
        self.samples_ms(self.target)
    }

    pub fn depth_ms(&self, depth: usize) -> u32 {
        self.samples_ms(depth)
    }

    /// Smoothed ring depth in ms — what drift correction actually reacts to, and the honest
    /// number to publish as "audio buffer" (the instantaneous depth swings by a whole quantum).
    pub fn avg_depth_ms(&self) -> u32 {
        self.samples_ms(self.depth_avg.max(0.0) as usize)
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Live target grown by underrun pressure, lifted to serve one quantum plus a packet. A
    /// large-buffer device is lifted to `want` plus one frame rather than oscillating
    /// prime → dropout → re-prime. The floor sync is clamped against, and what `hollow` is judged against.
    fn adaptive_target(&self, want: usize) -> usize {
        self.target.max(want + self.frame_samples())
    }

    /// [`adaptive_target`](Self::adaptive_target), or the sync request clamped into `[adaptive, hard_cap]`.
    fn effective_target(&self, want: usize) -> usize {
        let floor = self.adaptive_target(want);
        match self.sync_target {
            // Continuity outranks sync — see `set_sync_target`. Ceiling is raised to the floor
            // rather than passed to `clamp`: a quantum above `hard_cap_ms` makes `floor > cap`,
            // and `Ord::clamp` panics when min > max, in a realtime audio callback.
            Some(s) => {
                let cap = self.ms_samples(self.tuning.hard_cap_ms).max(floor);
                s.clamp(floor, cap)
            }
            None => floor,
        }
    }

    /// Decide this callback: what to trim, and whether to play. Call BEFORE reading, with the
    /// ring's current `depth` and the device's `want`, both in interleaved samples.
    pub fn step(&mut self, depth: usize, want: usize) -> JitterStep {
        self.last_want = want;
        let target = self.effective_target(want);

        // Weight by `want` so the time constant stays EWMA_TAU_MS whether the device pulls
        // 5 ms or 20 ms at a time.
        let alpha = (want as f32 / self.ms_samples(EWMA_TAU_MS) as f32).clamp(0.0, 1.0);
        self.depth_avg += (depth as f32 - self.depth_avg) * alpha;

        // Both caps must leave room to serve this callback, or a large-quantum device
        // trims itself into a permanent underrun.
        let cap = (target + self.ms_samples(self.tuning.headroom_ms))
            .min(self.ms_samples(self.tuning.hard_cap_ms))
            .max(target + want);
        let hard = self.ms_samples(self.tuning.hard_cap_ms).max(cap);

        let mut out = JitterStep::default();
        // A clump that drains before the next one arrives is the audio bridging the hole, so
        // the headroom line is judged on the AVERAGE. Only the hard cap trims on sight.
        let keep = if depth > hard {
            Some(hard)
        } else if depth > cap && self.depth_avg > cap as f32 {
            Some(cap)
        } else {
            None
        };
        if let Some(keep) = keep {
            // Wedged: discard down to the line and restart the drift clock from what is left,
            // so the trim is not counted as drift. Faded like any other drop. Whole frames:
            // a ms line at 44.1 kHz can fall mid-frame, and a split frame swaps channels.
            let keep = keep - keep % self.channels as usize;
            out.drop_front = depth - keep;
            out.hard_trim = true;
            out.crossfade = self.crossfade_samples().min(keep);
            self.depth_avg = keep as f32;
            self.over_run = 0;
            self.under_run = 0;
        } else if self.depth_avg > (target + self.ms_samples(self.tuning.shed_excess_ms())) as f32 {
            self.over_run += want;
            self.under_run = 0;
            if self.over_run >= self.ms_samples(SHED_SUSTAIN_MS) {
                out.drop_front = self.frame_samples().min(depth);
                out.crossfade = self
                    .crossfade_samples()
                    .min(depth.saturating_sub(out.drop_front));
                self.over_run = 0;
            }
        } else if self.primed
            && self.sync_wants_more()
            && (self.depth_avg as usize + self.ms_samples(INSERT_MARGIN_MS)) < target
        {
            // Mirror of the shed: sync asked for a deeper ring and the depth average has sat
            // below it for the sustain window. Duplicate one frame, crossfaded. Below-target,
            // sync-only, primed-only. A ring that cannot hold a whole frame is running dry.
            self.over_run = 0;
            self.under_run += want;
            if self.under_run >= self.ms_samples(INSERT_SUSTAIN_MS) && depth >= self.frame_samples()
            {
                out.insert_front = self.frame_samples();
                out.crossfade = self.crossfade_samples();
                self.under_run = 0;
            }
        } else {
            self.over_run = 0;
            self.under_run = 0;
        }
        // Shed is no longer buffered; insert is. Reflect both now so the next callback does
        // not re-fire on a stale average. A trim already restarted it at `keep`.
        if !out.hard_trim {
            self.depth_avg =
                (self.depth_avg - out.drop_front as f32 + out.insert_front as f32).max(0.0);
        }

        if !self.primed && depth.saturating_sub(out.drop_front) >= target {
            self.primed = true;
            self.empties = 0;
            self.empties_run = 0;
            // Seed the average with the refill. Leaving it at the drought value reads as hollow
            // for the EWMA's settling time, and the first late packet re-primes a full ring.
            self.depth_avg = depth.saturating_sub(out.drop_front) as f32;
        }
        out.silence = !self.primed;
        // Unconditional so a stale near-miss cannot survive a de-prime into the next primed read.
        let after = depth.saturating_sub(out.drop_front) + out.insert_front;
        self.near_miss = self.primed
            && after >= want
            // Served, but less than one resolved frame left. Against a 2 ms lossless frame a
            // frozen 5 ms margin would grow the target on a ring that was never close.
            && after - want < self.frame_samples();
        // Hollow: depth average runs a debt against the adaptive target — growth that was
        // never banked. A sync request is alignment, not starvation. Judged on the average
        // so one late packet keeps the consecutive-empties hysteresis.
        let adaptive = self.adaptive_target(want);
        self.hollow =
            self.primed && (self.depth_avg as usize + self.ms_samples(DEPRIME_DEBT_MS)) < adaptive;
        out
    }

    /// Outcome of the read `step` authorised. `ran_short` is a genuine underrun and drives
    /// de-prime hysteresis and the adaptive floor. Silence from `step` is not an underrun —
    /// the ring is re-priming — so un-primed calls are ignored.
    pub fn note_read(&mut self, ran_short: bool) {
        if !self.primed {
            return;
        }
        let want = self.last_want.max(1);
        let near_miss = std::mem::take(&mut self.near_miss);
        self.window_run += want;
        if self.window_run >= self.ms_samples(GROW_WINDOW_MS) {
            self.window_run = 0;
            self.underruns = 0;
            self.near_miss_grown = false;
        }
        self.sync_backoff_run = self.sync_backoff_run.saturating_sub(want);
        let mut restored = false;
        if self.probe_run > 0 {
            self.probe_run = self.probe_run.saturating_sub(want);
            if ran_short || near_miss {
                // Probe failed: restore the proven depth at once and back the sync loop off,
                // doubling per consecutive failure. Consumed as growth evidence so growing
                // past the proven target on top would overshoot.
                self.probe_run = 0;
                self.target = self.target.max(self.probe_prev_target);
                self.sync_backoff_run = self.ms_samples(self.sync_backoff_ms);
                self.sync_backoff_ms = (self.sync_backoff_ms * 2).min(SYNC_BACKOFF_MAX_MS);
                restored = true;
            } else if self.probe_run == 0 {
                // Survived the window: the shallower depth is safe here, so the next probe
                // starts from a clean slate.
                self.sync_backoff_ms = SYNC_BACKOFF_MS;
            }
        }
        if ran_short {
            self.quiet_run = 0;
            self.empties += 1;
            self.empties_run += want;
            // Starved for `deprime_ms` of audio, over at least MIN_DEPRIME_CALLBACKS callbacks.
            // Time alone is a hair trigger when one quantum already exceeds the window; a
            // callback count alone is a different fuse on every device.
            let starved = self.empties_run >= self.ms_samples(self.tuning.deprime_ms)
                && self.empties >= MIN_DEPRIME_CALLBACKS;
            if starved || self.hollow {
                // Hysteresis protects a full ring from one late packet. A hollow ring is the
                // opposite: the target rose but depth never re-banked. The underrun already
                // paid for the refill.
                self.primed = false;
                self.empties = 0;
                self.empties_run = 0;
            }
            if !restored {
                self.underruns += 1;
            }
            if self.underruns >= GROW_UNDERRUNS {
                // This device needs more slack than the base. Grow once per window, capped.
                self.underruns = 0;
                self.window_run = 0;
                let grown = self.target + self.ms_samples(GROW_STEP_MS);
                self.target = grown.min(self.ms_samples(self.tuning.max_target_ms));
            }
        } else if near_miss {
            // Within one frame of an underrun: same evidence, inaudible. Grow here, before
            // the underrun. One step per window (a bunching episode is a run of near-misses).
            // A near-miss is pressure, not quiet.
            self.quiet_run = 0;
            self.empties = 0;
            self.empties_run = 0;
            if !self.near_miss_grown && !restored {
                self.near_miss_grown = true;
                let grown = self.target + self.ms_samples(GROW_STEP_MS);
                self.target = grown.min(self.ms_samples(self.tuning.max_target_ms));
            }
        } else {
            self.empties = 0;
            self.empties_run = 0;
            self.quiet_run += want;
            // A grown target relaxes after a long quiet spell. Sync asking for less is
            // evidence the extra depth costs alignment now, so test sooner. Every shrink is
            // a probe: a failed sync-driven guess is not retried for a backoff.
            let sync_shrink = self.sync_wants_less() && self.sync_backoff_run == 0;
            let quiet_needed = if sync_shrink {
                SHRINK_QUIET_SYNC_MS
            } else {
                SHRINK_QUIET_MS
            };
            if self.quiet_run >= self.ms_samples(quiet_needed) {
                // Give a grown target one step back, so a short bad spell does not cost
                // latency for the rest of the session.
                self.quiet_run = 0;
                let base = self.ms_samples(self.tuning.base_target_ms);
                let prev = self.target;
                self.target = self
                    .target
                    .saturating_sub(self.ms_samples(GROW_STEP_MS))
                    .max(base);
                if self.target < prev {
                    self.probe_run = self.ms_samples(SHRINK_PROBE_MS);
                    self.probe_prev_target = prev;
                }
            }
        }
    }
}

/// Opus-plane sample rate, and the protocol default. Lossless negotiates via [`pcm`].
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Discard `drop` interleaved samples from the front of `ring`, linearly crossfading the seam
/// over `fade` samples. `fade == 0` discards hard — honoured for callers that splice at a point
/// they know is already discontinuous. Shared by the three `VecDeque<f32>` rings.
pub fn crossfade_drop(ring: &mut std::collections::VecDeque<f32>, drop: usize, fade: usize) {
    if drop == 0 || ring.len() < drop {
        return;
    }
    let fade = fade.min(drop).min(ring.len() - drop);
    if fade == 0 {
        ring.drain(..drop);
        return;
    }
    // Fade-out is the first `fade` discarded (continuation of the sample just played), not the
    // last — that end is adjacent to the survivors but the device just played `ring[0]`. Writes
    // at `drop + i` sit above every fade-out read (`i < fade <= drop`), so one in-place
    // ascending pass is safe in a realtime callback.
    for i in 0..fade {
        let old = ring[i];
        let new = ring[drop + i];
        let t = (i + 1) as f32 / (fade + 1) as f32;
        ring[drop + i] = old * (1.0 - t) + new * t;
    }
    ring.drain(..drop);
}

/// Mirror of [`crossfade_drop`]: duplicate the first `insert` samples at the front and
/// crossfade the seam. Net length `+insert`. Allocation-free when the ring has spare capacity
/// (`push_front` inside capacity). No-op on `insert == 0` or a ring shorter than `insert`;
/// `fade == 0` splices hard.
pub fn crossfade_insert(ring: &mut std::collections::VecDeque<f32>, insert: usize, fade: usize) {
    if insert == 0 || ring.len() < insert {
        return;
    }
    let fade = fade.min(insert).min(ring.len() - insert);
    // Build the copy at the front, last sample first. Before iteration `k` the front `k`
    // samples of the ring are `orig[insert-k .. insert]`, so `orig[insert-1-k]` — the sample
    // to push next — always sits at index `insert - 1`. After `insert` pushes the ring reads
    // `orig[0..insert] ++ orig`, and `orig[j]` sits at `insert + j`.
    for _ in 0..insert {
        let s = ring[insert - 1];
        ring.push_front(s);
    }
    // Seam: after the copy the original head plays again. Fade-out is `orig[insert..]` at
    // `2·insert + i`, blended into `orig[i]` at `insert + i`. Fade-out reads sit at or above
    // `2·insert`, above every write, so one ascending pass is safe.
    for i in 0..fade {
        let old = ring[2 * insert + i];
        let new = ring[insert + i];
        let t = (i + 1) as f32 / (fade + 1) as f32;
        ring[insert + i] = old * (1.0 - t) + new * t;
    }
}

/// Where [`apply_gain`]'s soft knee begins, in linear amplitude (≈ −3.1 dBFS). Below this the
/// gained signal is passed through exactly: a boost whose peaks never reach the knee is plain
/// multiplication, sample for sample, so the limiter costs nothing on material that does not
/// need it.
pub const SOFT_LIMIT_KNEE: f32 = 0.7;

/// Multiply `samples` by `gain`, bending anything that would overshoot full scale into a soft
/// knee instead of a hard clip. `(s * gain).clamp(-1.0, 1.0)` replaces peaks with flat tops —
/// a discontinuity in the first derivative that radiates high-order harmonics.
///
/// The curve is `tanh`-based:
/// 1. C¹-continuous at the knee: slope at `m == KNEE` is 1, matching the linear branch.
/// 2. Bounded: `tanh` is asymptotic to 1; `±inf` maps to `±1.0`.
/// 3. Odd-symmetric: `f(-x) == -f(x)`, so distortion is odd-harmonic with no DC.
///
/// Callers gate on `gain != 1.0`. Memoryless (zero latency), not a lookahead limiter.
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    // Unity is a no-op, not "multiply by one and shape". The shaper is only correct on a
    // signal somebody asked to boost. Calling at unity would bend every peak above the knee;
    // the callers' `gain != 1.0` guards are a convenience, not a load-bearing contract.
    if gain == 1.0 {
        return;
    }
    for s in samples {
        *s = soft_limit(*s * gain);
    }
}

/// Waveshaper behind [`apply_gain`]: identity below [`SOFT_LIMIT_KNEE`], asymptotic to ±1.0 above.
pub fn soft_limit(x: f32) -> f32 {
    let m = x.abs();
    if m <= SOFT_LIMIT_KNEE {
        return x;
    }
    let head = 1.0 - SOFT_LIMIT_KNEE;
    let shaped = SOFT_LIMIT_KNEE + head * ((m - SOFT_LIMIT_KNEE) / head).tanh();
    if x < 0.0 {
        -shaped
    } else {
        shaped
    }
}

// ---- per-platform channel-layout helpers (pure data; no platform deps) --------------------

/// Windows `WAVEFORMATEXTENSIBLE.dwChannelMask` for the wire layout.
///
/// 7.1 is `0x63F` (FL FR FC LFE **BL BR SL SR**), not `0xFF`. `0xFF` selects the
/// front-of-center pair FLC/FRC, the wrong speakers. WASAPI delivers channels in ascending
/// mask-bit order, which equals the wire order, so the decoded PCM needs no permutation.
pub const fn wasapi_channel_mask(channels: u8) -> u32 {
    const FL: u32 = 0x1;
    const FR: u32 = 0x2;
    const FC: u32 = 0x4;
    const LFE: u32 = 0x8;
    const BL: u32 = 0x10; // back left (wire RL)
    const BR: u32 = 0x20; // back right (wire RR)
    const SL: u32 = 0x200; // side left
    const SR: u32 = 0x400; // side right
    match channels {
        6 => FL | FR | FC | LFE | BL | BR,           // 0x3F
        8 => FL | FR | FC | LFE | BL | BR | SL | SR, // 0x63F
        _ => FL | FR,                                // 0x3 (stereo)
    }
}

/// PipeWire / SPA `enum spa_audio_channel` positions in wire order — identical to the host
/// capture side (`punktfunk-host` `audio::linux::spa_positions`): FL=3 FR=4 FC=5 LFE=6 SL=7
/// SR=8 RL=12 RR=13. Identity routing: the client sets these on its playback node so PipeWire
/// maps each wire slot to the matching speaker (and downmixes when the sink has fewer).
pub fn spa_positions(channels: u8) -> &'static [u32] {
    const STEREO: [u32; 2] = [3, 4]; // FL FR
    const C51: [u32; 6] = [3, 4, 5, 6, 12, 13]; // FL FR FC LFE RL RR
    const C71: [u32; 8] = [3, 4, 5, 6, 12, 13, 7, 8]; // FL FR FC LFE RL RR SL SR
    match channels {
        6 => &C51,
        8 => &C71,
        _ => &STEREO,
    }
}

/// Lock-free hand-off between the decode/pull thread (timestamps) and the realtime callback
/// (ring + [`JitterPolicy`]). The callback must not block, so they trade two words.
///
/// `usize::MAX` encodes "no target", not `0` — `0` is a valid depth and would silently drain
/// the ring.
#[derive(Debug)]
pub struct AudioSyncCell {
    depth: std::sync::atomic::AtomicUsize,
    target: std::sync::atomic::AtomicUsize,
    /// Concealment the decode side has synthesized this session, ms. Produced on decode, read
    /// from the callback's 10 s playback line.
    plc_ms: std::sync::atomic::AtomicU64,
    /// Device output latency past the ring, ns ([`AvSyncObservation::output_latency_ns`]).
    /// Produced by the backend, read on decode.
    output_latency_ns: std::sync::atomic::AtomicU64,
}

impl Default for AudioSyncCell {
    fn default() -> Self {
        AudioSyncCell {
            depth: std::sync::atomic::AtomicUsize::new(0),
            target: std::sync::atomic::AtomicUsize::new(usize::MAX),
            plc_ms: std::sync::atomic::AtomicU64::new(0),
            output_latency_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl AudioSyncCell {
    pub fn publish_depth(&self, depth: usize) {
        self.depth
            .store(depth, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn depth(&self) -> usize {
        self.depth.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn publish_plc_ms(&self, ms: u64) {
        self.plc_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn plc_ms(&self) -> u64 {
        self.plc_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn publish_output_latency_ns(&self, ns: u64) {
        self.output_latency_ns
            .store(ns, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn output_latency_ns(&self) -> u64 {
        self.output_latency_ns
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Decode side: ask the ring to aim for this depth (`None` = unsynchronised).
    pub fn set_target(&self, target: Option<usize>) {
        self.target.store(
            target.unwrap_or(usize::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn target(&self) -> Option<usize> {
        match self.target.load(std::sync::atomic::Ordering::Relaxed) {
            usize::MAX => None,
            t => Some(t),
        }
    }
}

/// Smoothing time constant for the measured A/V offset, in ms of consumed audio. Long enough
/// that network jitter and a single late datagram do not move it; short enough to track real
/// drift.
const AV_EWMA_TAU_MS: u32 = 2_000;
/// Offsets inside this band are left alone. Correcting a few ms costs a real discontinuity and
/// buys nothing a listener can perceive; the deadband stops the loop hunting around zero.
const AV_DEADBAND_MS: u32 = 10;
/// Observations folded before the first correction is offered. The offset is derived from a
/// clock skew estimate and a video figure that both need a moment to settle after connect;
/// acting on the first sample would chase the handshake, not the stream.
const AV_MIN_OBSERVATIONS: u32 = 100;
/// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale
/// video figure can all produce an enormous apparent misalignment, and steering the ring by
/// it would empty or overfill it. Beyond this the loop reports and waits rather than acting.
const AV_SANE_LIMIT_MS: u32 = 1_000;

/// Turns "when will this audio play" and "when did its picture reach the glass" into a ring
/// depth [`JitterPolicy`] should aim for.
///
/// Video is the master: the video leg is the input-feel budget and must not inflate to satisfy
/// the audio clock. Audio moves, via small crossfaded corrections ([`crossfade_drop`]).
/// Continuity outranks sync: this type only proposes a depth; [`JitterPolicy`] clamps it to
/// the underrun-driven floor. See [`JitterPolicy::set_sync_target`].
#[derive(Clone, Debug)]
pub struct AvSync {
    /// Negotiated layout, same two numbers [`JitterPolicy`] keeps, so a millisecond agrees down
    /// to the sample.
    rate_hz: u32,
    channels: u8,
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play late relative
    /// to the picture it belongs with.
    offset_avg_ns: f32,
    observations: u32,
    implausible: bool,
    /// Last depth offered outside the deadband; what the deadband keeps asking for.
    held: Option<usize>,
}

/// One measurement for [`AvSync::observe`]. Each field is in the units its source already produces.
#[derive(Clone, Copy, Debug)]
pub struct AvSyncObservation {
    pub pts_ns: u64,
    /// Local wall-clock now, same basis the client's video latency math uses (CLOCK_REALTIME).
    pub now_local_ns: i128,
    /// Host clock minus client clock, from the skew handshake (`clock_offset_now_ns`).
    pub clock_offset_ns: i64,
    /// How much audio is already queued ahead of this frame, in interleaved samples.
    pub buffered_ahead: usize,
    /// Device output latency past the ring: from a sample leaving it to the speaker (the
    /// graph or endpoint buffer, a Bluetooth link). `0` = unknown.
    pub output_latency_ns: u64,
    /// Video end-to-end in ns: `displayed + clock_offset − pts`. `None` until a frame is presented.
    pub video_e2e_ns: Option<u64>,
}

impl AvSync {
    /// `channels` is 2/6/8 at [`SAMPLE_RATE_HZ`]. Hi-res uses [`new_at_rate`](Self::new_at_rate).
    pub fn new(channels: u8) -> AvSync {
        Self::new_at_rate(channels, SAMPLE_RATE_HZ)
    }

    /// As [`new`](Self::new), at an explicitly negotiated `rate_hz`. Multiply-before-divide so
    /// the 44.1 kHz family is representable and the proposed depth is in the ring's units.
    pub fn new_at_rate(channels: u8, rate_hz: u32) -> AvSync {
        AvSync {
            rate_hz: rate_hz.max(1),
            channels: channels.max(1),
            offset_avg_ns: 0.0,
            observations: 0,
            implausible: false,
            held: None,
        }
    }

    fn samples_ms(&self, samples: usize) -> u32 {
        samples_to_ms(self.rate_hz, self.channels, samples)
    }

    /// Fold one measurement. Smoothed offset in ns once there is enough evidence (positive =
    /// audio late), or `None` while settling. Rejects the implausible rather than clamping it:
    /// a clamped wrong value would be acted on as a small real one.
    pub fn observe(&mut self, o: AvSyncObservation) -> Option<i64> {
        // No frame on the glass yet: no reference to align against.
        let video_e2e_ns = o.video_e2e_ns?;
        // Play-at in the host capture clock, same shape as the video figure. Rounded to whole
        // milliseconds; ≤ 1 ms is inside [`AV_DEADBAND_MS`]. The conversion itself is exact at
        // every rate.
        let buffered_ns = self.samples_ms(o.buffered_ahead) as i128 * 1_000_000;
        let play_at_host =
            o.now_local_ns + buffered_ns + o.output_latency_ns as i128 + o.clock_offset_ns as i128;
        let audio_e2e_ns = play_at_host - o.pts_ns as i128;
        let offset_ns = audio_e2e_ns - video_e2e_ns as i128;

        if offset_ns.unsigned_abs() > (AV_SANE_LIMIT_MS as u128) * 1_000_000 {
            self.implausible = true;
            return None;
        }
        self.implausible = false;

        // Weight by one protocol frame so the time constant means the same thing regardless of
        // how often the caller observes.
        let alpha = (FRAME_MS as f32 / AV_EWMA_TAU_MS as f32).clamp(0.0, 1.0);
        if self.observations == 0 {
            self.offset_avg_ns = offset_ns as f32;
        } else {
            self.offset_avg_ns += (offset_ns as f32 - self.offset_avg_ns) * alpha;
        }
        self.observations = self.observations.saturating_add(1);
        self.settled().then_some(self.offset_avg_ns as i64)
    }

    pub fn settled(&self) -> bool {
        self.observations >= AV_MIN_OBSERVATIONS
    }

    /// Smoothed offset in ms (positive = audio late), for the HUD. Reported while still settling
    /// so the operator can watch it converge.
    pub fn offset_ms(&self) -> i32 {
        (self.offset_avg_ns / 1_000_000.0) as i32
    }

    pub fn implausible(&self) -> bool {
        self.implausible
    }

    /// The ring depth that would place audio with the picture, given where the ring is now.
    /// `None` while unsettled: the caller runs unsynchronised. Inside the deadband, the last
    /// request again: a ring that reached its depth stays there, where `None` would drop it to
    /// the floor and shed what the insert just built.
    ///
    /// Audio late (offset > 0) means there is too much queued: aim shallower. Audio early means
    /// aim deeper.
    pub fn desired_depth(&mut self, current_depth: usize) -> Option<usize> {
        if !self.settled() {
            self.held = None;
            return None;
        }
        let offset_ms = self.offset_avg_ns / 1_000_000.0;
        if offset_ms.abs() < AV_DEADBAND_MS as f32 {
            return self.held;
        }
        // One millisecond of samples as a float, so a fractional offset scales smoothly.
        // Divide the constant, not the product: `x * 96000.0 / 1000.0` can land one sample off
        // the `x * 96.0` every 48 kHz session computes. This way 44 100 Hz stereo is 88.2, not 88.
        let per_ms = interleaved_per_sec(self.rate_hz, self.channels) as f32 / 1000.0;
        let delta = (offset_ms * per_ms) as i64;
        self.held = Some((current_depth as i64 - delta).max(0) as usize);
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_table_is_consistent() {
        for l in [
            &LAYOUT_STEREO,
            &LAYOUT_51,
            &LAYOUT_51_STANDARD,
            &LAYOUT_51_HQ,
            &LAYOUT_71,
            &LAYOUT_71_STANDARD,
            &LAYOUT_71_HQ,
        ] {
            assert_eq!(l.mapping.len(), l.channels as usize);
            // A permutation: every wire slot fed by exactly one stream channel. Identity for
            // all but the standard coupling, which only moves the pairs.
            let mut fed = vec![false; l.channels as usize];
            for &m in l.mapping {
                assert!(
                    !std::mem::replace(&mut fed[m as usize], true),
                    "mapping must be a permutation for {l:?}"
                );
            }
            // libopus: channels == coupled*2 + (streams - coupled).
            assert_eq!(
                l.coupled * 2 + (l.streams - l.coupled),
                l.channels,
                "stream/coupled accounting for {l:?}"
            );
            assert!(l.coupled <= l.streams);
            assert!(l.bitrate > 0);
        }
    }

    #[test]
    fn layout_for_picks_expected() {
        assert_eq!(layout_for(2, AudioLayout::Legacy), &LAYOUT_STEREO);
        assert_eq!(layout_for(6, AudioLayout::Legacy), &LAYOUT_51);
        assert_eq!(layout_for(6, AudioLayout::Uncoupled), &LAYOUT_51_HQ);
        assert_eq!(layout_for(8, AudioLayout::Legacy), &LAYOUT_71);
        assert_eq!(layout_for(8, AudioLayout::Uncoupled), &LAYOUT_71_HQ);
        assert_eq!(layout_for(0, AudioLayout::Legacy), &LAYOUT_STEREO);
        assert_eq!(layout_for(3, AudioLayout::Legacy), &LAYOUT_STEREO);
        assert_eq!(layout_for(7, AudioLayout::Uncoupled), &LAYOUT_STEREO);
        assert_eq!(layout_for(6, AudioLayout::Standard), &LAYOUT_51_STANDARD);
        assert_eq!(layout_for(8, AudioLayout::Standard), &LAYOUT_71_STANDARD);
        for l in [
            AudioLayout::Legacy,
            AudioLayout::Standard,
            AudioLayout::Uncoupled,
        ] {
            assert_eq!(AudioLayout::from_wire(l.wire()), Some(l));
        }
        assert_eq!(
            AudioLayout::from_wire(3),
            None,
            "an id this build does not know"
        );
        assert_eq!(
            AudioLayout::default().wire(),
            0,
            "absence on the wire is legacy"
        );
    }

    #[test]
    fn normalize_clamps_to_negotiable() {
        assert_eq!(normalize_channels(2), 2);
        assert_eq!(normalize_channels(6), 6);
        assert_eq!(normalize_channels(8), 8);
        for bad in [0u8, 1, 3, 4, 5, 7, 9, 255] {
            assert_eq!(normalize_channels(bad), 2, "{bad} must clamp to stereo");
        }
    }

    #[test]
    fn gap_tracker_counts_only_forward_gaps() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(100), 0, "first packet");
        assert_eq!(t.missing_before(101), 0, "in order");
        assert_eq!(t.missing_before(104), 2, "102+103 lost");
        assert_eq!(t.missing_before(104), 0, "duplicate");
        assert_eq!(t.missing_before(103), 0, "late reorder conceals nothing");
        assert_eq!(t.missing_before(105), 0, "reorder didn't move the anchor");
        // A huge gap is capped; the stream continues from the new anchor.
        assert_eq!(t.missing_before(105 + 1000), 10);
        assert_eq!(t.missing_before(105 + 1001), 0);
    }

    /// The cap is 50 ms of audio, not ten packets. A short frame must still buy the
    /// milliseconds it says.
    #[test]
    fn the_conceal_cap_is_fifty_milliseconds_of_the_negotiated_frame() {
        // Opus: ten 5 ms frames.
        assert_eq!(max_conceal_packets(FRAME_MS * 1000), 10);
        let mut t = AudioGapTracker::new();
        t.missing_before(0);
        assert_eq!(t.missing_before(9_999), 10);

        // 2 ms lossless: twenty-five frames for the same 50 ms.
        assert_eq!(max_conceal_packets(2_000), 25);
        let mut p = AudioGapTracker::new_at_frame_us(2_000);
        p.missing_before(0);
        assert_eq!(p.missing_before(9_999), 25);

        for &us in &pcm::FRAME_US_LADDER {
            let n = max_conceal_packets(us);
            assert!(n >= 1, "{us} µs must conceal at least one frame");
            assert!(
                n as u64 * us as u64 <= MAX_CONCEAL_MS as u64 * 1000,
                "{us} µs × {n} frames exceeds the {MAX_CONCEAL_MS} ms this cap promises"
            );
        }
        assert_eq!(max_conceal_packets(0), 10, "0 µs falls back to the default");
        assert_eq!(max_conceal_packets(u32::MAX), 1);
    }

    #[test]
    fn gap_tracker_survives_seq_wraparound() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(u32::MAX - 1), 0);
        assert_eq!(t.missing_before(u32::MAX), 0, "in order at the edge");
        assert_eq!(t.missing_before(1), 1, "seq 0 lost across the wrap");
        assert_eq!(t.missing_before(0), 0, "pre-wrap reorder, not a 2^31 gap");
    }

    // ---- redundant-plane recovery ---------------------------------------------------------

    #[test]
    fn a_superseded_audio_packet_is_dropped() {
        let mut g = AudioSeqGate::new();
        assert!(g.fresh(100));
        assert!(g.fresh(102), "a gap is concealed downstream");
        assert!(!g.fresh(101), "late: its slot was already concealed");
        assert!(!g.fresh(102), "duplicate");
        assert!(g.fresh(103));
        let mut w = AudioSeqGate::new();
        assert!(w.fresh(u32::MAX));
        assert!(w.fresh(0), "a wrap is newer");
    }

    #[test]
    fn red_recovery_rebuilds_exactly_the_single_missing_frame() {
        let mut r = AudioRedRecovery::new();
        assert!(!r.recover_before(10, true));
        assert!(!r.recover_before(11, true));
        // 12 lost: 13 carries it.
        assert!(r.recover_before(13, true));
        assert!(!r.recover_before(14, true));
    }

    #[test]
    fn red_recovery_is_conservative() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        assert!(!r.recover_before(20, false));
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        r.recover_before(11, true);
        assert!(!r.recover_before(11, true), "duplicate");
        assert!(!r.recover_before(9, true), "late reorder");
        assert!(
            !r.recover_before(12, true),
            "the reorder must not have moved the anchor"
        );
    }

    /// A longer burst still recovers its last frame. The remaining gap is one frame shorter.
    #[test]
    fn red_recovery_shortens_a_longer_burst() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(100, true);
        assert!(
            r.recover_before(105, true),
            "104 is recoverable even though 101-103 are not"
        );
    }

    #[test]
    fn red_recovery_survives_seq_wraparound() {
        let mut r = AudioRedRecovery::new();
        assert!(!r.recover_before(u32::MAX - 1, true));
        assert!(
            !r.recover_before(u32::MAX, true),
            "in order across the edge"
        );
        assert!(r.recover_before(1, true), "seq 0 lost across the wrap");
        assert!(!r.recover_before(2, true));
    }

    /// Whatever `AudioRedRecovery` rebuilds, `AudioGapTracker` must then see as no gap. Recovery
    /// lives on the demux side so every embedder sees a complete stream.
    #[test]
    fn recovery_and_the_gap_tracker_agree() {
        let mut rec = AudioRedRecovery::new();
        let mut gaps = AudioGapTracker::new();
        let mut concealed = 0;
        // Deliver 0..20 with 7 and 13 lost; each survivor carries its predecessor.
        let mut emitted: Vec<u32> = Vec::new();
        for seq in (0..20u32).filter(|s| *s != 7 && *s != 13) {
            if rec.recover_before(seq, true) {
                emitted.push(seq - 1);
            }
            emitted.push(seq);
        }
        for seq in &emitted {
            concealed += gaps.missing_before(*seq);
        }
        assert_eq!(
            concealed, 0,
            "recovered stream must need no concealment: {emitted:?}"
        );
        assert_eq!(emitted.len(), 20, "every frame accounted for");
        assert!(
            emitted.windows(2).all(|w| w[1] == w[0] + 1),
            "and in order: {emitted:?}"
        );
    }

    // ---- drought concealment --------------------------------------------------------------

    /// Concealment is for a ring that is running out. A drought a deep ring can cover is
    /// inaudible, and synthesizing over it would duplicate the late packets.
    #[test]
    fn a_drought_is_concealed_only_while_the_ring_is_running_out() {
        let mut c = DroughtConceal::new(JitterTuning::PIPEWIRE.plc_max_ms());
        let stalled = drought_after() + std::time::Duration::from_millis(FRAME_MS as u64);
        assert!(
            !c.conceal(stalled, 40),
            "a 40 ms ring covers this drought by itself"
        );
        assert!(c.conceal(stalled, 0), "an empty ring does not");
        assert_eq!(c.total_ms(), FRAME_MS as u64);
    }

    #[test]
    fn ordinary_jitter_is_not_a_drought() {
        let mut c = DroughtConceal::new(JitterTuning::AAUDIO.plc_max_ms());
        for _ in 0..1_000 {
            assert!(!c.conceal(std::time::Duration::from_millis(FRAME_MS as u64), 0));
        }
        assert_eq!(c.total_ms(), 0);
        assert_eq!(c.packet(), 0);
    }

    /// Every preset gets twice its own de-prime fuse, so no platform silently gets a third of
    /// another's protection.
    #[test]
    fn drought_concealment_is_bounded_at_twice_the_deprime_fuse() {
        for t in [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ] {
            assert_eq!(t.plc_max_ms(), t.deprime_ms * 2);
            let mut c = DroughtConceal::new(t.plc_max_ms());
            let mut ms = 0u32;
            while c.conceal(drought_after(), 0) {
                ms += FRAME_MS;
                assert!(ms <= t.plc_max_ms(), "ran past the budget for {t:?}");
            }
            assert_eq!(ms, t.plc_max_ms(), "must use exactly the budget for {t:?}");
        }
    }

    /// Packets lost inside a drought already covered must not be covered a second time by the
    /// loss path: doing both would insert audio the stream never carried.
    #[test]
    fn concealment_already_paid_for_is_not_paid_for_twice() {
        let mut c = DroughtConceal::new(JitterTuning::WASAPI.plc_max_ms());
        for _ in 0..4 {
            assert!(c.conceal(drought_after(), 0));
        }
        let mut gaps = AudioGapTracker::new();
        gaps.missing_before(10);
        // Four frames concealed; the wire then reveals six were lost. Only two are still owed.
        let already = c.packet();
        assert_eq!(already, 4);
        assert_eq!(gaps.missing_before(17).saturating_sub(already), 2);
        assert!(c.conceal(drought_after(), 0));
    }

    // ---- bitrate tiers -------------------------------------------------------------------

    /// `Standard` must equal the layout table's `bitrate`.
    #[test]
    fn standard_tier_is_the_legacy_table() {
        for l in [
            &LAYOUT_STEREO,
            &LAYOUT_51,
            &LAYOUT_51_STANDARD,
            &LAYOUT_51_HQ,
            &LAYOUT_71,
            &LAYOUT_71_STANDARD,
            &LAYOUT_71_HQ,
        ] {
            assert_eq!(l.bitrate_for(AudioTier::Standard), l.bitrate, "{l:?}");
        }
    }

    #[test]
    fn tiers_are_monotonic_and_hq_layouts_are_invariant() {
        for l in [
            &LAYOUT_STEREO,
            &LAYOUT_51,
            &LAYOUT_51_STANDARD,
            &LAYOUT_71,
            &LAYOUT_71_STANDARD,
        ] {
            let (lo, std, hi) = (
                l.bitrate_for(AudioTier::Low),
                l.bitrate_for(AudioTier::Standard),
                l.bitrate_for(AudioTier::High),
            );
            assert!(lo < std && std < hi, "{l:?}: {lo} < {std} < {hi}");
        }
        for l in [&LAYOUT_51_HQ, &LAYOUT_71_HQ] {
            for t in [AudioTier::Low, AudioTier::Standard, AudioTier::High] {
                assert_eq!(l.bitrate_for(t), l.bitrate, "{l:?} at {t:?}");
            }
        }
    }

    #[test]
    fn tier_default_is_high_and_parses() {
        assert_eq!(AudioTier::default(), AudioTier::High);
        for t in [AudioTier::Low, AudioTier::Standard, AudioTier::High] {
            assert_eq!(AudioTier::parse(t.as_str()), Some(t));
        }
        assert_eq!(AudioTier::parse("  HIGH "), Some(AudioTier::High));
        assert_eq!(AudioTier::parse("normal"), Some(AudioTier::Standard));
        assert_eq!(AudioTier::parse("transparent"), None);
        assert_eq!(AudioTier::parse(""), None);
    }

    // ---- the audio bandwidth budget --------------------------------------------------------

    /// `High` (256 kbps stereo) times the redundant plane is 512 kbps — ~10 % of a 5 Mbps
    /// session — and audio is outside ABR, so ABR cannot reclaim it.
    #[test]
    fn budget_steps_down_as_the_link_narrows() {
        let plan = |kbps| plan_audio_budget(kbps, 2, AudioLayout::Legacy, AudioTier::High, true);
        let b = plan(20_000);
        assert_eq!((b.tier, b.redundancy), (AudioTier::High, true));
        assert_eq!(b.kbps, 512);
        // Halve it and redundancy goes first: quality outranks recovery that only pays under loss.
        assert_eq!(plan(10_000).tier, AudioTier::High);
        assert!(!plan(10_000).redundancy);
        assert_eq!(plan(5_000).tier, AudioTier::Standard);
        assert!(!plan(5_000).redundancy);
        assert_eq!(plan(1_000).tier, AudioTier::Low);
        assert_eq!(plan(1).tier, AudioTier::Low);
        assert_eq!(
            plan(0).kbps,
            96,
            "audio must survive an absurd video bitrate"
        );
    }

    #[test]
    fn budget_never_exceeds_its_share() {
        for kbps in [0u32, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 100_000] {
            for ch in [2u8, 6, 8] {
                let b = plan_audio_budget(kbps, ch, AudioLayout::Legacy, AudioTier::High, true);
                let allowed =
                    (kbps.saturating_mul(AUDIO_BUDGET_PCT) / 100).max(AUDIO_BUDGET_FLOOR_KBPS);
                let floor =
                    plan_audio_budget(0, ch, AudioLayout::Legacy, AudioTier::Low, false).kbps;
                assert!(
                    b.kbps <= allowed || b.kbps == floor,
                    "{ch}ch at {kbps} kbps: spent {} of {allowed}",
                    b.kbps
                );
            }
        }
    }

    /// Surround costs more per tier, so the same link must step it down sooner than stereo.
    /// The budget is total wire cost, not the tier name.
    #[test]
    fn budget_accounts_for_the_channel_count() {
        let stereo = plan_audio_budget(10_000, 2, AudioLayout::Legacy, AudioTier::High, true);
        let surround = plan_audio_budget(10_000, 8, AudioLayout::Legacy, AudioTier::High, true);
        assert_eq!(stereo.tier, AudioTier::High);
        assert!(surround.kbps <= stereo.kbps.max(surround.kbps), "sanity");
        // 7.1 High is 768 kbps, past a 500 kbps allowance.
        assert!(
            surround.kbps < 768,
            "7.1 High must not fit a 10 Mbps budget"
        );
    }

    /// The budget may lower what was asked for, never raise it. An operator who set `low` gets
    /// `low` on a 100 Mbps link; a client that never asked for redundancy never gets it.
    #[test]
    fn budget_respects_the_request() {
        let b = plan_audio_budget(100_000, 2, AudioLayout::Legacy, AudioTier::Low, true);
        assert_eq!(b.tier, AudioTier::Low);
        let b = plan_audio_budget(100_000, 2, AudioLayout::Legacy, AudioTier::Standard, true);
        assert_eq!(b.tier, AudioTier::Standard);
        assert!(b.redundancy, "Standard + redundancy fits a huge link");
        let b = plan_audio_budget(100_000, 2, AudioLayout::Legacy, AudioTier::High, false);
        assert_eq!(b.tier, AudioTier::High);
        assert!(
            !b.redundancy,
            "a client that did not ask must never be sent 0xD2"
        );
    }

    // ---- the de-jitter policy ------------------------------------------------------------

    fn per_ms(channels: u8) -> usize {
        (SAMPLE_RATE_HZ / 1000) as usize * channels as usize
    }

    #[derive(Debug, Default)]
    struct Sim {
        final_ms: u32,
        peak_ms: u32,
        /// Smooth drift corrections (crossfaded, one frame each).
        soft_sheds: u32,
        /// Hard-cap trims. Any of these in a plain-drift run means the smooth correction is
        /// not doing its job.
        hard_trims: u32,
        underruns: u32,
    }

    /// Drive a policy through `ms` of simulated audio at a `quantum_ms` device, where the
    /// producer delivers `drift_ppm` more (or less) than the consumer takes — host-vs-client
    /// clock skew.
    fn simulate(
        tuning: JitterTuning,
        channels: u8,
        ms: u32,
        quantum_ms: u32,
        drift_ppm: i64,
        start_ms: u32,
    ) -> Sim {
        let pm = per_ms(channels);
        let want = quantum_ms as usize * pm;
        let mut p = JitterPolicy::new(tuning, channels);
        let mut depth = start_ms as usize * pm;
        let mut out = Sim::default();
        // Fractional producer accumulator, so a sub-sample-per-callback drift still accumulates.
        let mut carry: i64 = 0;
        for _ in 0..(ms / quantum_ms) {
            carry += want as i64 * drift_ppm;
            let extra = carry / 1_000_000;
            carry -= extra * 1_000_000;
            depth = (depth as i64 + want as i64 + extra).max(0) as usize;

            let s = p.step(depth, want);
            if s.drop_front > 0 {
                // Told apart by `hard_trim`, not fade length — both kinds fade.
                if s.hard_trim {
                    out.hard_trims += 1;
                } else {
                    out.soft_sheds += 1;
                }
                assert!(
                    s.crossfade > 0,
                    "every drop must be faded: dropped {} with no crossfade",
                    s.drop_front
                );
                depth -= s.drop_front.min(depth);
            }
            // No sync target: an unsynchronised ring must not insert. Pinned on every drift
            // run rather than in one test.
            assert_eq!(s.insert_front, 0, "an unsynced ring inserted");
            if s.silence {
                p.note_read(false);
                continue;
            }
            let short = depth < want;
            depth -= want.min(depth);
            if short {
                out.underruns += 1;
            }
            p.note_read(short);
            out.peak_ms = out.peak_ms.max((depth / pm) as u32);
        }
        out.final_ms = (depth / pm) as u32;
        out
    }

    /// On every preset the smooth shed point must sit strictly below the hard trim point.
    /// Invert it and the ring is trimmed before the depth average can reach the shed
    /// threshold, so the smooth path becomes dead code.
    #[test]
    fn every_preset_sheds_before_it_trims() {
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            assert!(
                t.shed_excess_ms() < t.headroom_ms,
                "{name}: sheds at +{} ms but trims at +{} ms — drift correction can never fire",
                t.shed_excess_ms(),
                t.headroom_ms
            );
            assert!(
                t.base_target_ms + t.headroom_ms <= t.hard_cap_ms,
                "{name}: the headroom band is cut short by the hard cap"
            );
            assert!(t.max_target_ms >= t.base_target_ms, "{name}");
            // A drought has to outlast several protocol frames before the ring gives up, or
            // one late packet manufactures a whole target of silence.
            assert!(
                t.deprime_ms >= 4 * FRAME_MS,
                "{name}: de-primes after {} ms — a single late packet would trip it",
                t.deprime_ms
            );
            // Never longer than the deepest buffer this preset would ever hold: past that the
            // drought has already cost more than the re-prime it is trying to avoid.
            assert!(
                t.deprime_ms <= t.max_target_ms,
                "{name}: waits {} ms to de-prime but never buffers more than {} ms",
                t.deprime_ms,
                t.max_target_ms
            );
        }
    }

    /// Host clock running fast must not pin the ring at the ceiling. Drift correction holds
    /// depth near target with the smooth crossfaded shed, never the hard cap.
    #[test]
    fn drift_does_not_ratchet_latency_to_the_ceiling() {
        // +200 ppm is a harsh skew (real DAC pairs are tens of ppm); 5 minutes.
        let s = simulate(JitterTuning::AAUDIO, 2, 300_000, 5, 200, 25);
        assert!(
            s.soft_sheds > 0,
            "drift must be shed, not accumulated: {s:?}"
        );
        assert_eq!(
            s.hard_trims, 0,
            "plain drift must never reach the hard cap: {s:?}"
        );
        assert_eq!(
            s.underruns, 0,
            "shedding must never cause an underrun: {s:?}"
        );
        let ceiling = JitterTuning::AAUDIO.base_target_ms + JitterTuning::AAUDIO.headroom_ms;
        assert!(
            s.peak_ms <= ceiling,
            "peaked at {} ms (band ends at {ceiling}) — that is the ratchet, not a correction",
            s.peak_ms
        );
    }

    #[test]
    fn no_preset_ratchets_under_drift() {
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            let s = simulate(t, 2, 300_000, 5, 200, t.base_target_ms);
            assert!(s.soft_sheds > 0, "{name}: {s:?}");
            assert!(
                s.peak_ms <= t.base_target_ms + t.headroom_ms,
                "{name} peaked at {} ms: {s:?}",
                s.peak_ms
            );
        }
    }

    /// A host clock running slow must not be "corrected" into permanent underruns. The
    /// adaptive floor may grow the target, but nothing may be shed.
    #[test]
    fn negative_drift_grows_the_target_instead_of_stuttering() {
        let s = simulate(JitterTuning::AAUDIO, 2, 120_000, 5, -200, 25);
        assert_eq!(
            s.soft_sheds, 0,
            "nothing to shed when the ring is draining: {s:?}"
        );
        assert_eq!(s.hard_trims, 0, "{s:?}");
    }

    /// A shed must never fire on a transient. A burst that arrives and drains is normal jitter.
    /// The spike here sits above the shed threshold but below the trim point, so only the
    /// sustain requirement can reject it.
    #[test]
    fn a_transient_burst_does_not_shed() {
        let t = JitterTuning::AAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let spike_ms = t.base_target_ms + t.shed_excess_ms() + FRAME_MS; // inside the band
        assert!(
            spike_ms < t.base_target_ms + t.headroom_ms,
            "test spike must not hit the trim"
        );
        let mut p = JitterPolicy::new(t, 2);
        let mut sheds = 0;
        // 300 ms spiked out of every 1 s, for 20 s.
        for round in 0..20 {
            for i in 0..200 {
                let depth = if round > 0 && i < 60 {
                    spike_ms
                } else {
                    t.base_target_ms
                } as usize;
                let s = p.step(depth * pm, want);
                if s.drop_front > 0 {
                    sheds += 1;
                }
                p.note_read(false);
            }
        }
        assert_eq!(
            sheds, 0,
            "a repeated short burst must not trigger drift correction"
        );
    }

    /// The hard cap is the only absolute latency guarantee. It trims immediately, without
    /// waiting for the depth average.
    #[test]
    fn hard_cap_trims_at_once() {
        let pm = per_ms(2);
        let mut p = JitterPolicy::new(JitterTuning::AAUDIO, 2);
        let s = p.step(500 * pm, 5 * pm);
        assert!(
            s.drop_front > 0,
            "a 500 ms backlog must be trimmed on the spot"
        );
        assert!(s.hard_trim, "a cap trim must announce itself as one");
        // Faded: the samples either side of the splice are ordinary continuous sound.
        assert!(
            s.crossfade > 0,
            "a cap trim splices real audio and must be faded"
        );
        assert!(
            s.crossfade <= s.drop_front,
            "the fade cannot outrun what is being dropped"
        );
        let left = 500 * pm - s.drop_front;
        assert!(
            left <= JitterTuning::AAUDIO.hard_cap_ms as usize * pm,
            "trim must land at or under the hard cap"
        );
    }

    /// A trim drops whole frames. 25 ms at 44.1 kHz stereo is 2 205 samples, half a frame;
    /// dropping to it would play every later sample on the wrong channel.
    #[test]
    fn a_trim_never_splits_a_frame() {
        for ch in [2u8, 6, 8] {
            for sync in [None, Some(2_851)] {
                let mut p = JitterPolicy::new_at_rate(JitterTuning::AAUDIO, ch, 44_100);
                p.set_sync_target(sync);
                let depth = 100 * 441 / 10 * ch as usize;
                let want = 96 * ch as usize;
                p.step(depth, want);
                p.note_read(false);
                let s = p.step(depth, want);
                assert!(s.hard_trim, "{ch}ch {sync:?}: {s:?}");
                assert_eq!(s.drop_front % ch as usize, 0, "{ch}ch {sync:?}: {s:?}");
                // What was kept is what the drift clock restarts from.
                assert_eq!(
                    p.avg_depth_ms(),
                    p.depth_ms(depth - s.drop_front),
                    "{ch}ch {sync:?}"
                );
            }
        }
    }

    /// One transient drain must not manufacture a fresh target's worth of silence.
    #[test]
    fn deprime_requires_hysteresis() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        // An empty ring must emit silence and stay un-primed, however many callbacks it sees.
        for _ in 0..10 {
            assert!(p.step(0, want).silence, "an empty ring cannot play");
        }
        assert!(!p.is_primed());
        assert!(
            !p.step(50 * pm, want).silence,
            "a ring holding well over target must start immediately"
        );
        assert!(p.is_primed());
        p.note_read(true); // one short read
        assert!(p.is_primed(), "a single short read must not de-prime");
        let deprime = JitterTuning::PIPEWIRE.deprime_ms as usize;
        for _ in 1..(deprime / 5) {
            p.note_read(true);
        }
        assert!(!p.is_primed(), "a sustained drain must re-prime");
    }

    /// The de-prime fuse is the same span of time whatever the device's IO quantum. A callback
    /// count is not: the same `4` is ~40 ms at 10 ms and 20 ms at 5 ms.
    #[test]
    fn deprime_fuse_is_a_duration_not_a_callback_count() {
        for quantum_ms in [5usize, 8, 10, 16, 21] {
            let t = JitterTuning::COREAUDIO;
            let pm = per_ms(2);
            let want = quantum_ms * pm;
            let mut p = JitterPolicy::new(t, 2);
            // Prime well above target so the hysteresis path is what we measure, not `hollow`.
            assert!(!p.step(80 * pm, want).silence);
            assert!(p.is_primed());
            let mut starved_ms = 0;
            while p.is_primed() && starved_ms < 10 * t.deprime_ms as usize {
                p.note_read(true);
                starved_ms += quantum_ms;
            }
            assert!(!p.is_primed(), "q={quantum_ms}ms: never de-primed at all");
            // One quantum of granularity either side: the fuse can only be checked per callback.
            let floor = (t.deprime_ms as usize).min(quantum_ms * MIN_DEPRIME_CALLBACKS as usize);
            assert!(
                starved_ms >= floor && starved_ms < t.deprime_ms as usize + quantum_ms,
                "q={quantum_ms}ms de-primed after {starved_ms} ms, not ~{} ms — the fuse is still \
                 scaling with the quantum",
                t.deprime_ms
            );
        }
    }

    /// A device that pulls a big quantum cannot sustain a target below it: the effective target
    /// must lift, or the ring oscillates prime → dropout → re-prime forever.
    #[test]
    fn target_lifts_above_a_large_device_quantum() {
        let pm = per_ms(2);
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2); // base target 15 ms
        let want = 40 * pm; // a 40 ms graph quantum, far above the base target
        assert!(
            p.step(15 * pm, want).silence,
            "15 ms cannot serve a 40 ms quantum"
        );
        let s = p.step((40 + FRAME_MS as usize) * pm, want);
        assert!(!s.silence, "quantum + one frame must be enough to start");
    }

    /// The rate parameter must move samples without moving milliseconds. A 96 kHz ring holds
    /// twice the samples for the same latency, and every ms-denominated figure a client reports
    /// — `target_ms`, `depth_ms`, `avg_depth_ms` — must read identically at both rates. If this
    /// ever fails, the hi-res plane is buying latency it did not intend to.
    #[test]
    fn a_hires_policy_holds_the_same_milliseconds_as_a_48k_one() {
        for t in [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ] {
            for ch in [2u8, 6, 8] {
                let lo = JitterPolicy::new_at_rate(t, ch, 48_000);
                let hi = JitterPolicy::new_at_rate(t, ch, 96_000);
                assert_eq!(
                    lo.target_ms(),
                    hi.target_ms(),
                    "96 kHz must start at the same latency as 48 kHz ({t:?}, {ch}ch)"
                );
                assert_eq!(
                    hi.depth_ms(2 * lo.ms_samples(1)),
                    lo.depth_ms(lo.ms_samples(1)),
                    "a 96 kHz ring needs twice the samples for the same ms ({t:?}, {ch}ch)"
                );
                assert_eq!(
                    hi.ms_samples(1),
                    2 * lo.ms_samples(1),
                    "a millisecond must scale with the rate"
                );
            }
        }
    }

    /// The shed drops exactly one frame and fades across part of it. A short frame must shed a
    /// short frame rather than 2.5 of them.
    #[test]
    fn the_shed_follows_the_negotiated_frame_length() {
        let pm = per_ms(2);

        // Default: one 5 ms frame dropped, a 2 ms fade.
        let p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        assert_eq!(p.frame_samples(), FRAME_MS as usize * pm);
        assert_eq!(p.crossfade_samples(), SHED_CROSSFADE_MS as usize * pm);

        // 2 ms lossless sheds 2 ms; fade is capped at half a frame.
        let mut q = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        q.set_frame_us(2_000);
        assert_eq!(q.frame_samples(), 2 * pm);
        assert_eq!(q.crossfade_samples(), pm, "fade must be half a 2 ms frame");
        assert!(
            q.crossfade_samples() < q.frame_samples(),
            "a fade as long as the frame is not a crossfade"
        );

        // 2 500 µs at 48 kHz stereo is 240 interleaved samples, and must not truncate to 192
        // by going through integer milliseconds first.
        let mut r = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        r.set_frame_us(2_500);
        assert_eq!(r.frame_samples(), 240);

        let mut h = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, 96_000);
        h.set_frame_us(2_000);
        assert_eq!(h.frame_samples(), 2 * per_ms_at(96_000, 2));

        let mut z = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        z.set_frame_us(0);
        assert!(z.frame_samples() >= 1);
    }

    /// The drought budget is wall-clock, spent one frame at a time, so the two have to agree
    /// about how long a frame is.
    #[test]
    fn the_drought_budget_is_spent_at_the_negotiated_frame_length() {
        // Opus: 100 ms of budget is twenty 5 ms frames, and the reported total agrees.
        let mut o = DroughtConceal::new(100);
        let mut n = 0;
        while o.conceal(drought_after(), 0) {
            n += 1;
        }
        assert_eq!(n, 20, "100 ms of 5 ms frames");
        assert_eq!(o.total_ms(), 100);
        assert_eq!(o.packet(), 20, "the caller is owed a FRAME count");

        // Same budget at 2 ms must buy the same wall clock: fifty frames, not twenty.
        let mut p = DroughtConceal::new_at_frame_us(100, 2_000);
        let mut m = 0;
        while p.conceal(std::time::Duration::from_millis(10), 0) {
            m += 1;
        }
        assert_eq!(m, 50, "100 ms of 2 ms frames");
        assert_eq!(p.total_ms(), 100, "plc_ms must not over-report");
        assert_eq!(p.packet(), 50);

        // A short frame also stops waiting five frames before it concedes a stall.
        let q = DroughtConceal::new_at_frame_us(100, 2_000);
        assert_eq!(q.after(), std::time::Duration::from_millis(4));
        assert_eq!(DroughtConceal::new(100).after(), drought_after());
    }

    /// The near-miss margin is less than one packet left in hand. Frozen at 5 ms it would mean
    /// two and a half packets on a 2 ms lossless frame. Identical on Opus.
    #[test]
    fn the_near_miss_margin_is_one_negotiated_frame() {
        let pm = per_ms(2);
        let want = 5 * pm;

        // A depth one sample short of a full frame in hand is a near miss.
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        p.set_frame_us(2_000);
        p.step(60 * pm, want); // prime
        p.note_read(false);
        p.step(want + 2 * pm - 1, want);
        assert!(p.near_miss, "under one 2 ms frame in hand is a near miss");

        // A full frame in hand is not a near miss.
        let mut q = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        q.set_frame_us(2_000);
        q.step(60 * pm, want);
        q.note_read(false);
        q.step(want + 2 * pm, want);
        assert!(
            !q.near_miss,
            "a whole 2 ms frame in hand is not a near miss"
        );
    }

    fn per_ms_at(rate: u32, channels: u8) -> usize {
        (rate / 1000) as usize * channels as usize
    }

    fn drought_after() -> std::time::Duration {
        std::time::Duration::from_millis(2 * FRAME_MS as u64)
    }

    /// `new` is exactly `new_at_rate` at the protocol default.
    #[test]
    fn the_default_constructor_is_the_default_rate() {
        let a = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        let b = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, SAMPLE_RATE_HZ);
        assert_eq!(a.ms_samples(1), b.ms_samples(1));
        assert_eq!(a.target, b.target);
        assert_eq!(a.target_ms(), b.target_ms());
        let x = AvSync::new(2);
        let y = AvSync::new_at_rate(2, SAMPLE_RATE_HZ);
        assert_eq!((x.rate_hz, x.channels), (y.rate_hz, y.channels));
    }

    /// `depth_ms(target)` must round-trip to `target_ms()`. Multiply-before-divide holds for the
    /// whole ladder; `per_ms = rate_hz / 1000 × channels` truncated 44 100 Hz to 44 samples/ms.
    #[test]
    fn the_shipping_rate_ladder_round_trips_ms_to_samples_exactly() {
        let presets = [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ];
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400] {
            assert!(
                pcm::rate_is_supported(rate),
                "{rate} Hz must be on the ladder"
            );
            for ch in [2u8, 6, 8] {
                for t in presets {
                    let p = JitterPolicy::new_at_rate(t, ch, rate);
                    assert_eq!(
                        p.depth_ms(p.target),
                        p.target_ms(),
                        "depth_ms(target) must round-trip to target_ms at {rate} Hz / {ch}ch"
                    );
                    assert_eq!(
                        p.target_ms(),
                        t.base_target_ms,
                        "the base target must be exactly the preset's ms at {rate} Hz / {ch}ch"
                    );
                    // Every other ms the preset names is a threshold `step`/`note_read` compares
                    // a sample count against.
                    for ms in [
                        t.base_target_ms,
                        t.max_target_ms,
                        t.headroom_ms,
                        t.hard_cap_ms,
                        t.deprime_ms,
                        t.plc_max_ms(),
                        GROW_STEP_MS,
                        GROW_WINDOW_MS,
                        SHRINK_QUIET_MS,
                        SYNC_BACKOFF_MAX_MS,
                        EWMA_TAU_MS,
                    ] {
                        assert_eq!(
                            p.samples_ms(p.ms_samples(ms)),
                            ms,
                            "{ms} ms does not survive ms → samples → ms at {rate} Hz / {ch}ch"
                        );
                    }
                    // Against `ms × rate × ch` and only then the divide by 1000, not against
                    // itself.
                    for ms in [1u32, 12, 15, 47, 1000, SYNC_BACKOFF_MAX_MS] {
                        let want = ms as u64 * rate as u64 * ch as u64 / 1000;
                        assert_eq!(
                            p.ms_samples(ms) as u64,
                            want,
                            "{ms} ms at {rate} Hz / {ch}ch"
                        );
                    }
                }
            }
        }

        let p = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, 44_100);
        assert_eq!(p.ms_samples(15), 1_323, "15 ms of 44.1 kHz stereo");
        assert_eq!(15 * (44_100 / 1000) * 2, 1_320, "what it used to compute");

        // Exact is not lossless both ways: 12 ms at 44 100 Hz stereo is 1 058.4 samples, which
        // floors to 1 058 and reads back as 11 ms — at most one sample on a threshold inside a
        // 25 ms band, not 2.3 % on every figure.
        assert_eq!(JitterTuning::PIPEWIRE.shed_excess_ms(), 12);
        assert_eq!(p.ms_samples(12), 1_058); // 1 058.4, floored
        assert_eq!(p.samples_ms(1_058), 11);
    }

    /// The policy's idea of a frame must be the wire's. A frame carries a whole number of samples
    /// per channel, so 5 ms at 44 100 Hz stereo is 440 interleaved samples, not the 441 that
    /// `frame_us × samples-per-ms` produces. Near-miss and shed both mean exactly one packet.
    #[test]
    fn the_policys_frame_is_the_wires_frame() {
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400] {
            for &us in &pcm::FRAME_US_LADDER {
                for ch in [2u8, 6] {
                    let mut p = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, ch, rate);
                    p.set_frame_us(us);
                    assert_eq!(
                        p.frame_samples(),
                        pcm::samples_per_frame(rate, us, ch),
                        "{rate} Hz / {ch}ch at {us} µs"
                    );
                }
            }
        }
        // 5 ms of 44.1 kHz stereo audio is 441 interleaved samples; a 5 ms frame of it carries
        // 440 — whole samples per channel, and 220.5 is not a sample count.
        let mut p = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, 44_100);
        p.set_frame_us(5_000);
        assert_eq!(p.frame_samples(), 440, "220 samples per channel, not 220.5");
        assert_eq!(p.ms_samples(5), 441, "5 ms of audio, which is not a frame");
    }

    /// Clustered underruns raise the floor (that device needs the slack); a long quiet spell
    /// gives it back, so a short bad spell does not cost latency for the whole session.
    #[test]
    fn target_grows_on_underruns_and_relaxes_when_quiet() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::AAUDIO, 2);
        let base = p.target_ms();
        assert_eq!(base, JitterTuning::AAUDIO.base_target_ms);
        for _ in 0..40 {
            // Keep it primed and starve it: depth is always enough to prime, never to serve.
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        assert!(
            grown > base,
            "clustered underruns must raise the floor ({base} → {grown})"
        );
        assert!(
            grown <= JitterTuning::AAUDIO.max_target_ms,
            "growth must respect max_target_ms"
        );
        for _ in 0..(SHRINK_QUIET_MS as usize * 3 / 5) {
            p.step(grown as usize * pm + want, want);
            p.note_read(false);
        }
        assert!(
            p.target_ms() < grown,
            "a quiet spell must give the growth back"
        );
        assert!(p.target_ms() >= base, "…but never below the base target");
    }

    /// The crossfade must leave a continuous waveform: splicing a ramp must not introduce a step
    /// bigger than the ramp's own per-sample slope.
    #[test]
    fn crossfade_drop_splices_without_a_step() {
        use std::collections::VecDeque;
        // A slow ramp: any hard splice shows up as a visible jump.
        let mut ring: VecDeque<f32> = (0..1000).map(|i| i as f32).collect();
        let (drop, fade) = (240, 96);
        crossfade_drop(&mut ring, drop, fade);
        assert_eq!(ring.len(), 1000 - drop);
        // Across the whole faded region the step between neighbours stays bounded. A hard drop
        // would show a `drop`-sized jump at index 0.
        for i in 0..fade {
            let step = (ring[i + 1] - ring[i]).abs();
            assert!(
                step < drop as f32,
                "sample {i}: step {step} looks like a hard splice"
            );
        }
        assert_eq!(ring[ring.len() - 1], 999.0);
    }

    #[test]
    fn crossfade_drop_handles_degenerate_inputs() {
        use std::collections::VecDeque;
        let mut ring: VecDeque<f32> = (0..10).map(|i| i as f32).collect();
        crossfade_drop(&mut ring, 0, 4); // nothing to drop
        assert_eq!(ring.len(), 10);
        crossfade_drop(&mut ring, 99, 4); // more than we hold — refuse
        assert_eq!(ring.len(), 10);
        crossfade_drop(&mut ring, 10, 4); // exactly all of it: no room to fade, hard drop
        assert!(ring.is_empty());
    }

    #[test]
    fn wasapi_masks_are_correct() {
        assert_eq!(wasapi_channel_mask(2), 0x3);
        assert_eq!(wasapi_channel_mask(6), 0x3F);
        assert_eq!(wasapi_channel_mask(8), 0x63F); // not 0xFF
        assert_eq!(wasapi_channel_mask(2).count_ones(), 2);
        assert_eq!(wasapi_channel_mask(6).count_ones(), 6);
        assert_eq!(wasapi_channel_mask(8).count_ones(), 8);
    }

    #[test]
    fn spa_positions_match_wire_order() {
        assert_eq!(spa_positions(2), &[3, 4]);
        assert_eq!(spa_positions(6), &[3, 4, 5, 6, 12, 13]);
        assert_eq!(spa_positions(8), &[3, 4, 5, 6, 12, 13, 7, 8]);
        assert_eq!(spa_positions(2).len(), 2);
        assert_eq!(spa_positions(6).len(), 6);
        assert_eq!(spa_positions(8).len(), 8);
    }

    /// A tone fed into wire channel N comes back out on channel N for stereo / 5.1 / 7.1.
    /// Encoder layout == decoder layout == identity mapping. Gated on `quic`.
    #[cfg(feature = "quic")]
    #[test]
    fn multistream_layout_roundtrips_with_channel_identity() {
        const SR: u32 = 48_000;
        const SAMPLES: usize = 240; // 5 ms at 48 kHz
        let layouts = [
            AudioLayout::Legacy,
            AudioLayout::Standard,
            AudioLayout::Uncoupled,
        ];
        for (channels, layout) in [2u8, 6, 8]
            .into_iter()
            .flat_map(|c| layouts.map(|l| (c, l)))
        {
            let l = layout_for(channels, layout);
            let ch = l.channels as usize;
            let mut enc = opus::MSEncoder::new(
                SR,
                l.streams,
                l.coupled,
                l.mapping,
                opus::Application::LowDelay,
            )
            .expect("MSEncoder");
            enc.set_bitrate(opus::Bitrate::Bits(l.bitrate)).unwrap();
            enc.set_vbr(false).unwrap();
            let mut dec =
                opus::MSDecoder::new(SR, l.streams, l.coupled, l.mapping).expect("MSDecoder");

            for tone_ch in 0..ch {
                let mut out = vec![0u8; 4000];
                let mut energy = vec![0f64; ch];
                // A few frames to clear the codec startup transient before measuring.
                for f in 0..8 {
                    let mut frame = vec![0f32; SAMPLES * ch];
                    for t in 0..SAMPLES {
                        let phase = (f * SAMPLES + t) as f32 * 440.0 * 2.0 * std::f32::consts::PI
                            / SR as f32;
                        frame[t * ch + tone_ch] = 0.5 * phase.sin();
                    }
                    let n = enc.encode_float(&frame, &mut out).unwrap();
                    let mut decoded = vec![0f32; SAMPLES * ch];
                    let got = dec.decode_float(&out[..n], &mut decoded, false).unwrap();
                    assert_eq!(got, SAMPLES, "{channels}ch frame size");
                    if f >= 4 {
                        for t in 0..SAMPLES {
                            for (c, e) in energy.iter_mut().enumerate() {
                                *e += (decoded[t * ch + c] as f64).powi(2);
                            }
                        }
                    }
                }
                let loudest = (0..ch)
                    .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
                    .unwrap();
                assert_eq!(
                    loudest, tone_ch,
                    "{channels}ch {layout:?}: tone in channel {tone_ch} must come out on {tone_ch} (energies {energy:?})"
                );
            }
        }
    }

    // ---- A/V sync ------------------------------------------------------------------------

    fn obs(offset_ms: i64, depth: usize, per_ms: usize) -> AvSyncObservation {
        // audio_e2e = buffered + (now + skew - pts). Pin now/skew/pts so the only free term is
        // the buffered depth, then choose video_e2e so the difference lands on `offset_ms`.
        let buffered_ms = (depth / per_ms) as i64;
        let audio_e2e_ms = buffered_ms + 40; // 40 ms of transport, arbitrary but fixed
        let video_e2e_ms = audio_e2e_ms - offset_ms;
        AvSyncObservation {
            pts_ns: 1_000_000_000,
            now_local_ns: 1_000_000_000i128 + 40 * 1_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            output_latency_ns: 0,
            video_e2e_ns: Some((video_e2e_ms.max(0) as u64) * 1_000_000),
        }
    }

    fn settle(sync: &mut AvSync, offset_ms: i64, depth: usize, per_ms: usize, n: u32) {
        for _ in 0..n {
            sync.observe(obs(offset_ms, depth, per_ms));
        }
    }

    #[test]
    fn av_sync_needs_evidence_before_acting() {
        let pm = per_ms(2);
        let mut s = AvSync::new(2);
        assert!(s.observe(obs(50, 30 * pm, pm)).is_none());
        assert!(!s.settled());
        assert!(s.desired_depth(30 * pm).is_none());
        settle(&mut s, 50, 30 * pm, pm, AV_MIN_OBSERVATIONS);
        assert!(s.settled(), "should act once the evidence is in");
    }

    #[test]
    fn av_sync_aims_shallower_when_audio_is_late() {
        let pm = per_ms(2);
        let depth = 60 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 40, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 40 ms offset is actionable");
        assert!(
            want < depth,
            "audio late must aim shallower: {want} vs {depth}"
        );
        let shed_ms = (depth - want) / pm;
        assert!(
            (35..=45).contains(&shed_ms),
            "should aim to shed ~40 ms, got {shed_ms}"
        );
    }

    #[test]
    fn av_sync_aims_deeper_when_audio_is_early() {
        let pm = per_ms(2);
        let depth = 20 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, -30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 30 ms offset is actionable");
        assert!(
            want > depth,
            "audio early must aim deeper: {want} vs {depth}"
        );
    }

    #[test]
    fn av_sync_deadbands_what_no_one_can_hear() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(
            &mut s,
            (AV_DEADBAND_MS - 2) as i64,
            depth,
            pm,
            AV_MIN_OBSERVATIONS * 4,
        );
        assert!(
            s.desired_depth(depth).is_none(),
            "an offset inside the deadband must not provoke a (real, if crossfaded) discontinuity"
        );
    }

    /// Audio that leaves the ring still has the device to cross. A 150 ms Bluetooth link on a
    /// ring that alone looks 30 ms early is audio 120 ms late: aim shallower, never deeper.
    #[test]
    fn av_sync_counts_the_device_behind_the_ring() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        for _ in 0..AV_MIN_OBSERVATIONS * 4 {
            s.observe(AvSyncObservation {
                output_latency_ns: 150_000_000,
                ..obs(-30, depth, pm)
            });
        }
        assert_eq!(s.offset_ms(), 120);
        let want = s
            .desired_depth(depth)
            .expect("a 120 ms offset is actionable");
        assert!(
            want < depth,
            "late audio must aim shallower: {want} vs {depth}"
        );
    }

    #[test]
    fn av_sync_rejects_the_implausible_instead_of_clamping_it() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let before = s.offset_ms();
        // A wall-clock step / stale video figure. Built directly rather than through `obs`:
        // that helper floors the video figure at zero, which would cap the offset at a merely
        // large value and let this test pass without ever exercising the rejection.
        let wild = AvSyncObservation {
            pts_ns: 0,
            now_local_ns: 5_000_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            output_latency_ns: 0,
            video_e2e_ns: Some(40_000_000),
        };
        assert!(s.observe(wild).is_none());
        assert!(s.implausible(), "a ~5 s offset must be refused, not folded");
        assert_eq!(
            before,
            s.offset_ms(),
            "an implausible sample must be discarded, not folded in"
        );
    }

    #[test]
    fn sync_can_never_starve_the_ring() {
        // Sync only ever proposes. Continuity — the underrun-driven floor — outranks it, or a
        // lossy link would be "synced" into dropouts.
        for (name, t) in [
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            let pm = per_ms(2);
            let want = 5 * pm;
            let mut p = JitterPolicy::new(t, 2);
            let floor = p.effective_target(want);
            p.set_sync_target(Some(0));
            assert_eq!(
                p.effective_target(want),
                floor,
                "{name}: sync pulled the target below the continuity floor"
            );
            p.set_sync_target(Some(usize::MAX / 2));
            assert!(
                p.effective_target(want) <= t.hard_cap_ms as usize * pm,
                "{name}: sync pushed the target past the hard cap"
            );
        }
    }

    #[test]
    fn a_huge_device_quantum_does_not_panic_the_clamp() {
        // `Ord::clamp` panics when min > max. A quantum above the hard cap pushes the continuity
        // floor above the ceiling, in a realtime callback — so the ceiling yields to the floor.
        let t = JitterTuning::PIPEWIRE; // hard_cap 160 ms
        let pm = per_ms(2);
        let want = 500 * pm; // a 500 ms quantum: absurd, but not a reason to abort the process
        let mut p = JitterPolicy::new(t, 2);
        p.set_sync_target(Some(0));
        let target = p.effective_target(want); // must not panic
        assert!(
            target >= want,
            "the target must still be able to serve one callback"
        );
    }

    /// Closed loop, as every client wires it: observe per packet, hand `desired_depth` to the
    /// policy. Video sits a steady `early_ms` behind the ring's own audio. Once the ring is deep
    /// enough the offset enters the deadband; the loop must hold there, not hunt.
    #[test]
    fn sync_steering_settles_instead_of_hunting() {
        let pm = per_ms(2);
        let want = 5 * pm;
        for early_ms in [40u64, 60, 100] {
            let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
            let mut av = AvSync::new(2);
            let (mut depth, mut sheds, mut inserts, mut trims) = (0usize, 0u32, 0u32, 0u32);
            for cb in 0..24_000u32 {
                depth += want;
                // Video end-to-end is fixed; only the ring moves the audio side.
                av.observe(AvSyncObservation {
                    video_e2e_ns: Some((40 + early_ms) * 1_000_000),
                    ..obs(0, depth, pm)
                });
                p.set_sync_target(av.desired_depth(depth));
                let s = p.step(depth, want);
                depth -= s.drop_front.min(depth);
                depth += s.insert_front;
                // Converging costs a handful of inserts; count only what follows the first minute.
                if cb >= 12_000 {
                    sheds += (s.drop_front > 0 && !s.hard_trim) as u32;
                    trims += s.hard_trim as u32;
                    inserts += (s.insert_front > 0) as u32;
                }
                let short = !s.silence && depth < want;
                if !s.silence {
                    depth -= want.min(depth);
                }
                p.note_read(short);
            }
            assert_eq!(
                (sheds, trims, inserts),
                (0, 0, 0),
                "{early_ms} ms early: still hunting in the second minute (sheds, trims, inserts)"
            );
        }
    }

    #[test]
    fn no_sync_target_leaves_the_policy_exactly_as_it_was() {
        // An unsynchronised ring must match one that never called `set_sync_target`. `None` is
        // the default, so this also pins the constructor.
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut a = JitterPolicy::new(t, 2);
        let mut b = JitterPolicy::new(t, 2);
        b.set_sync_target(None);
        assert_eq!(a.effective_target(want), b.effective_target(want));
        for depth_ms in [0usize, 5, 15, 30, 60, 90, 200] {
            let sa = a.step(depth_ms * pm, want);
            let sb = b.step(depth_ms * pm, want);
            assert_eq!(sa, sb, "depth {depth_ms} ms diverged with an explicit None");
            a.note_read(sa.silence);
            b.note_read(sb.silence);
        }
    }

    #[test]
    fn sync_pressure_relaxes_a_grown_target_sooner_than_time_alone() {
        // A ring that ratcheted during a transient must not hold audio late after the cause is
        // gone. With sync asking for less, the relax window is the short one.
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;

        let grow = |p: &mut JitterPolicy| {
            // Drive underruns until the target has grown above the base. Each round hands `step`
            // a deep ring first: `note_read` ignores everything while un-primed (a priming
            // silence is not an underrun), and consecutive short reads un-prime the ring — so
            // hammering a zero-depth ring would report nothing and grow nothing.
            for _ in 0..10_000 {
                if p.target_ms() > t.base_target_ms {
                    return;
                }
                p.step(200 * pm, want); // (re-)prime
                p.note_read(true); // then one genuine short read
            }
            panic!("the adaptive floor never grew — the test cannot measure a relax");
        };
        let quiet_to_relax = |p: &mut JitterPolicy| -> usize {
            let start = p.target_ms();
            let mut reads = 0usize;
            while p.target_ms() == start && reads < 200_000 {
                p.step(60 * pm, want);
                p.note_read(false);
                reads += 1;
            }
            reads
        };

        let mut slow = JitterPolicy::new(t, 2);
        grow(&mut slow);
        slow.set_sync_target(None);
        let slow_reads = quiet_to_relax(&mut slow);

        let mut fast = JitterPolicy::new(t, 2);
        grow(&mut fast);
        fast.set_sync_target(Some(pm));
        let fast_reads = quiet_to_relax(&mut fast);

        assert!(
            fast_reads < slow_reads,
            "sync pressure should relax sooner: {fast_reads} vs {slow_reads} quiet reads"
        );
    }

    // ---- near-miss growth and shrink probes ----------------------------------------------

    /// A primed read that is served but leaves less than one frame buffered is a near-miss —
    /// the same evidence as an underrun, inaudible — and must grow the target before the
    /// underrun. One step per window: a bunching episode is a run of near-misses.
    #[test]
    fn a_near_miss_grows_the_target_without_an_underrun() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        p.step(t.base_target_ms as usize * pm, want); // primes exactly at target
        assert!(p.is_primed());
        let base = p.target_ms();
        // Serve the callback with less than one frame left over: depth = want + (margin − 1).
        p.step(want + FRAME_MS as usize * pm - 1, want);
        p.note_read(false); // not short — the device got its samples
        assert_eq!(
            p.target_ms(),
            base + GROW_STEP_MS,
            "a near-miss must buy one step"
        );
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), base + GROW_STEP_MS, "one step per window");
        let grown = p.target_ms();
        p.step(grown as usize * pm + want, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), grown);
    }

    /// A healthy steady depth must never read as a near-miss: the margin is one frame, and a
    /// ring hovering at target sits a whole target above it.
    #[test]
    fn steady_depth_never_grows_the_target() {
        let t = JitterTuning::PIPEWIRE;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        for _ in 0..(60_000 / 5) {
            // one minute of clean callbacks
            p.step(t.base_target_ms as usize * pm + want, want);
            p.note_read(false);
        }
        assert_eq!(p.target_ms(), t.base_target_ms);
    }

    /// A shrink answered by an underrun or near-miss inside its probe window is undone at once.
    #[test]
    fn a_failed_shrink_probe_is_undone_at_once() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        // Grow the floor two steps via underruns.
        for _ in 0..(2 * GROW_UNDERRUNS) {
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        assert!(grown > t.base_target_ms);
        // Sync asks for less; five quiet seconds later the shrink probes.
        p.set_sync_target(Some(pm));
        let depth = grown as usize * pm + want;
        while p.target_ms() == grown {
            p.step(depth, want);
            p.note_read(false);
        }
        assert_eq!(p.target_ms(), grown - GROW_STEP_MS);
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(
            p.target_ms(),
            grown,
            "a failed probe must restore the target on the first near-miss"
        );
    }

    /// After a failed probe the sync loop may not drive another shrink at the accelerated
    /// cadence. The slow window still applies; the five-second one does not.
    #[test]
    fn a_failed_probe_backs_the_sync_shrink_off() {
        let t = JitterTuning::COREAUDIO;
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(t, 2);
        for _ in 0..(2 * GROW_UNDERRUNS) {
            while !p.is_primed() {
                p.step(200 * pm, want);
            }
            p.step(200 * pm, want);
            p.note_read(true);
        }
        let grown = p.target_ms();
        p.set_sync_target(Some(pm));
        let depth = grown as usize * pm + want;
        // First sync-driven shrink, then fail its probe.
        while p.target_ms() == grown {
            p.step(depth, want);
            p.note_read(false);
        }
        p.step(want + pm, want);
        p.note_read(false);
        assert_eq!(p.target_ms(), grown, "restored");
        // Twice the accelerated window of clean audio: the backed-off loop must not have shrunk.
        for _ in 0..(2 * SHRINK_QUIET_SYNC_MS / 5) {
            p.step(depth, want);
            p.note_read(false);
        }
        assert_eq!(
            p.target_ms(),
            grown,
            "the accelerated cadence must be suspended after a failure"
        );
        for _ in 0..(2 * SHRINK_QUIET_MS / 5) {
            p.step(depth, want);
            p.note_read(false);
        }
        assert!(
            p.target_ms() < grown,
            "the slow window must still be allowed to test a shrink"
        );
    }

    #[derive(Debug, Default)]
    struct BunchSim {
        /// Reads that starved the device — each one is audible.
        audible: u32,
        /// Audible reads in the second half: non-zero means the policy never converged.
        audible_tail: u32,
        inserts: u32,
        trims: u32,
        /// Times the ring de-primed after its first prime: each is a `target` of silence.
        reprimes: u32,
        /// Simulated ms when the depth average first came within `INSERT_MARGIN_MS` of the
        /// sync target. `None` = never (or no sync target).
        settle_ms: Option<u32>,
    }

    /// Drive a policy over a link that bunches: delivery pauses for `gap_ms` every `period_ms`,
    /// then the withheld audio arrives at once. `drift_ppm` is host-vs-DAC skew; a slightly slow
    /// host erodes depth so a wrong target is re-tested. Without it a simulated ring freezes
    /// wherever priming left it.
    fn simulate_bunching(
        tuning: JitterTuning,
        sync_target: Option<usize>,
        ms: u32,
        gap_ms: u32,
        period_ms: u32,
        drift_ppm: i64,
    ) -> BunchSim {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(tuning, 2);
        let mut depth = 0usize;
        let mut withheld = 0usize;
        let mut carry: i64 = 0;
        let mut out = BunchSim::default();
        let mut was_primed = false;
        // The sync loop speaks only once it has evidence (`AV_MIN_OBSERVATIONS`), which is
        // always after the ring has primed at its own base — so the request lands on a primed
        // ring, never on one still filling. A request for less is clamped at the base anyway;
        // a request for more must be answered by the insert, not by priming straight to it.
        let mut sync_pending = sync_target;
        for cb in 0..(ms / 5) {
            // The host keeps producing (want ± drift per callback); the link decides delivery.
            carry += want as i64 * drift_ppm;
            let extra = carry / 1_000_000;
            carry -= extra * 1_000_000;
            let produced = (want as i64 + extra).max(0) as usize;
            let in_gap = (cb * 5) % period_ms < gap_ms;
            if in_gap {
                withheld += produced;
            } else {
                depth += produced + std::mem::take(&mut withheld);
            }
            let s = p.step(depth, want);
            if p.is_primed() {
                if let Some(t) = sync_pending.take() {
                    p.set_sync_target(Some(t));
                }
            }
            depth -= s.drop_front.min(depth);
            if s.hard_trim {
                out.trims += 1;
            }
            if s.insert_front > 0 {
                assert!(s.crossfade > 0, "every insert must be faded");
                assert_eq!(s.drop_front, 0, "a step never drops AND inserts");
                assert!(s.insert_front <= depth, "inserted more than the ring holds");
                depth += s.insert_front;
                out.inserts += 1;
            }
            if let Some(t) = sync_target {
                if out.settle_ms.is_none()
                    && p.depth_avg as usize + p.ms_samples(INSERT_MARGIN_MS) >= t
                {
                    out.settle_ms = Some(cb * 5);
                }
            }
            if was_primed && !p.is_primed() {
                out.reprimes += 1;
            }
            was_primed = p.is_primed();
            if s.silence {
                p.note_read(false);
                continue;
            }
            let short = depth < want;
            depth -= want.min(depth);
            if short {
                out.audible += 1;
                if cb >= ms / 10 {
                    out.audible_tail += 1;
                }
            }
            p.note_read(short);
        }
        out
    }

    /// A link that parks ~100 ms at once every ~100 ms delivers exactly the audio that bridges
    /// the next hole. Trimming the clump on sight deleted it and then concealed the gap it made
    /// — ten and more trims a second. The clump fits under the hard cap and drains before the
    /// average ever crosses the headroom line, so nothing is cut.
    #[test]
    fn a_clumping_link_keeps_its_clump() {
        for (name, t) in [
            ("COREAUDIO", JitterTuning::COREAUDIO),
            ("PIPEWIRE", JitterTuning::PIPEWIRE),
            ("WASAPI", JitterTuning::WASAPI),
            ("AAUDIO", JitterTuning::AAUDIO),
        ] {
            // 90 ms holes every 100 ms, a slightly fast host, ten minutes. (Trimmed on sight this
            // was 6 000 trims and 6 000 starved reads on COREAUDIO.)
            let s = simulate_bunching(t, None, 600_000, 90, 100, 50);
            assert!(s.trims <= 8, "{name}: the clump is being trimmed: {s:?}");
            assert!(s.audible <= 16, "{name}: {s:?}");
            assert_eq!(s.audible_tail, 0, "{name}: {s:?}");
        }
    }

    /// The headroom line still bounds latency: a backlog that stays in the ring is trimmed once
    /// the average crosses it, within a couple of EWMA time constants.
    #[test]
    fn a_sustained_backlog_is_trimmed_to_the_headroom_line() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let t = JitterTuning::COREAUDIO;
        let mut p = JitterPolicy::new(t, 2);
        let cap = (t.base_target_ms + t.headroom_ms) as usize * pm;
        // Prime at target, then a 120 ms clump lands and the producer keeps pace.
        let mut depth = t.base_target_ms as usize * pm;
        assert!(!p.step(depth, want).silence);
        depth -= want;
        p.note_read(false);
        depth += 120 * pm;
        let mut trimmed_at = None;
        for cb in 0..1_000 {
            depth += want;
            let s = p.step(depth, want);
            depth -= s.drop_front;
            if s.hard_trim {
                trimmed_at = Some(cb * 5);
                break;
            }
            depth -= want;
            p.note_read(false);
        }
        let at = trimmed_at.expect("a backlog that never drains must be trimmed");
        assert!(
            at > 100,
            "trimmed at {at} ms: a clump must be given time to drain"
        );
        assert!(at <= 3 * EWMA_TAU_MS as usize, "trimmed only after {at} ms");
        assert!(
            depth <= cap,
            "left {depth} samples above the headroom line {cap}"
        );
    }

    /// A bunching link needs ~30 ms of ring; the sync loop wants less. Near-misses grow the
    /// target before the first underrun, a failed shrink probe is undone at once, and a hollow
    /// ring cashes the refill on the underrun it already paid. Clock-skew re-anchor remains —
    /// a slightly slow host starves the ring — so the bound is a handful over ten minutes, not
    /// zero.
    #[test]
    fn sync_pressure_on_a_bunching_link_converges_instead_of_clicking_forever() {
        // 25 ms gaps every 300 ms, a slightly slow host, ten minutes, sync permanently asking
        // for a 5 ms ring.
        let s = simulate_bunching(
            JitterTuning::COREAUDIO,
            Some(per_ms(2) * 5),
            600_000,
            25,
            300,
            -50,
        );
        assert!(
            s.audible_tail <= 4,
            "the tug-of-war must converge to the skew floor: {s:?}"
        );
        assert!(
            s.audible <= 12,
            "learning the link may cost a handful of audible events, not a stream of them: {s:?}"
        );
    }

    /// The same link without sync pressure — the plain adaptive-growth behaviour — must land in
    /// the same place. Sync steering may not add a persistent audible cost over not steering.
    #[test]
    fn a_bunching_link_without_sync_stays_clean_after_growing() {
        let s = simulate_bunching(JitterTuning::COREAUDIO, None, 600_000, 25, 300, -50);
        assert!(s.audible_tail <= 4, "{s:?}");
        assert!(s.audible <= 12, "{s:?}");
        assert_eq!(s.inserts, 0, "an unsynced ring must never insert: {s:?}");
    }

    // ---- sync-driven deepening: the insert, the mirror of the shed -----------------------

    /// Sync asking for a deeper ring is answered with one crossfaded frame per sustain window,
    /// not a de-prime. A clean link must not gap.
    #[test]
    fn a_sync_request_for_more_depth_deepens_without_a_de_prime_on_a_clean_link() {
        // Ring primes at PIPEWIRE's 15 ms base; sync asks for 35 ms — 20 ms deeper. No gaps.
        let s = simulate_bunching(
            JitterTuning::PIPEWIRE,
            Some(per_ms(2) * 35),
            60_000,
            0,
            300,
            0,
        );
        assert_eq!(s.audible, 0, "a clean link must stay silent-free: {s:?}");
        assert_eq!(s.reprimes, 0, "sync must never cause a de-prime: {s:?}");
        assert!(
            s.inserts > 0,
            "the deepening has to come from somewhere: {s:?}"
        );
        let settle = s.settle_ms.expect("the ring never reached the sync target");
        assert!(
            settle <= 20_000,
            "deepening by 20 ms took {settle} ms — too slow to track a wandering reference: {s:?}"
        );
        // Having settled it stops: below-target-only. Four frames cover 20 ms; allow the EWMA
        // a couple more, not a stream of them.
        assert!(
            s.inserts <= 8,
            "the insert kept firing after the ring was deep enough: {s:?}"
        );
    }

    /// Same bunching link, sync asking for more. The insert must not make a bunching link worse
    /// than the unsynced case, and the deepening must not be paid for in re-primes.
    #[test]
    fn a_sync_request_for_more_depth_stays_clean_on_a_bunching_link() {
        let s = simulate_bunching(
            JitterTuning::COREAUDIO,
            Some(per_ms(2) * 45),
            600_000,
            25,
            300,
            -50,
        );
        assert!(s.audible_tail <= 4, "{s:?}");
        assert!(s.audible <= 12, "{s:?}");
        assert!(
            s.settle_ms.is_some(),
            "the ring must reach the requested depth on a link that delivers: {s:?}"
        );
    }

    /// A primed ring asked for +30 ms is not hollow (`hollow` is judged against the adaptive
    /// target), so one short read leaves it primed. Once the average has sat below the request
    /// for `INSERT_SUSTAIN_MS`, the step inserts exactly one faded frame.
    #[test]
    fn a_sync_request_for_more_depth_never_de_primes() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        let mut depth = 15 * pm;
        assert!(
            !p.step(depth, want).silence,
            "15 ms primes the PIPEWIRE base"
        );
        depth -= want;
        p.note_read(false);
        p.set_sync_target(Some(45 * pm));
        // Hold depth at ~15 ms while the sync target sits 30 ms above it. 100 ms in, one late
        // packet: a sync request is not growth debt, so the hysteresis must hold.
        let mut consumed = 0usize;
        let mut short_read_done = false;
        let mut first = None;
        for _ in 0..2_000 {
            if !short_read_done && consumed >= 100 * pm {
                short_read_done = true;
                assert!(
                    !p.hollow,
                    "a sync request is not growth debt — the ring must not read as hollow"
                );
                let s = p.step(want / 2, want);
                assert!(!s.silence);
                p.note_read(true);
                assert!(
                    p.is_primed(),
                    "a single short read on a sync-deepened ring must keep the hysteresis, not de-prime"
                );
                consumed += want;
                continue;
            }
            depth += want;
            let before = p.depth_avg;
            let s = p.step(depth, want);
            if s.insert_front > 0 {
                assert_eq!(s.insert_front, p.frame_samples(), "one frame, no more");
                assert_eq!(s.crossfade, p.crossfade_samples(), "faded");
                assert_eq!(s.drop_front, 0);
                assert!(
                    p.depth_avg >= before + s.insert_front as f32 - 1.0,
                    "the average must reflect the inserted frame at once: {before} -> {}",
                    p.depth_avg
                );
                // The arming step's own `want` is part of the sustain (counted before the read).
                first = Some(consumed + want);
                break;
            }
            depth -= want;
            consumed += want;
            p.note_read(false);
        }
        assert!(short_read_done, "the short read never happened");
        let first = first.expect("the insert never armed");
        assert!(
            first >= p.ms_samples(INSERT_SUSTAIN_MS),
            "armed after {} ms — before the sustain window",
            first / pm
        );
        assert!(
            first <= p.ms_samples(INSERT_SUSTAIN_MS + 500),
            "armed after {} ms — long after the sustain window",
            first / pm
        );
    }

    /// Growth that was never banked still re-primes on the underrun it already paid. Judged
    /// against the adaptive target, not the effective one. No sync target here.
    #[test]
    fn growth_not_banked_still_re_primes() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
        // Prime at the 15 ms base and settle the average there.
        let mut depth = 15 * pm;
        assert!(!p.step(depth, want).silence);
        depth -= want;
        p.note_read(false);
        for _ in 0..400 {
            depth += want;
            let s = p.step(depth, want);
            depth -= s.drop_front + want;
            p.note_read(false);
        }
        // Grow the target twice without letting the depth follow: average stays ~15 ms while
        // the promise climbs to 35.
        for _round in 0..2 {
            for _ in 0..GROW_UNDERRUNS {
                let s = p.step(want / 2, want);
                assert!(
                    !s.silence,
                    "the hysteresis must hold through a single short read"
                );
                p.note_read(true);
                for _ in 0..8 {
                    let s = p.step(15 * pm + want, want);
                    p.note_read(s.silence);
                }
            }
            let mut consumed = 0;
            while consumed < p.ms_samples(GROW_WINDOW_MS) {
                let s = p.step(15 * pm + want, want);
                p.note_read(s.silence);
                consumed += want;
            }
        }
        // Growth may already have de-primed via the path under test; either way the target is
        // grown and the ring, if primed, is hollow against it.
        assert!(
            p.target_ms() >= 25,
            "the target must have grown, got {} ms",
            p.target_ms()
        );
        // Re-prime at the grown target if the loop already spent the underrun, then drain the
        // average to the base without an underrun: primed, promise 20 ms above what it holds.
        let target = p.effective_target(want);
        let s = p.step(target + want, want);
        assert!(!s.silence);
        p.note_read(false);
        for _ in 0..600 {
            let s = p.step(15 * pm + want, want);
            assert!(!s.silence, "no starvation here — the depth is only shallow");
            p.note_read(false);
        }
        assert!(p.is_primed());
        assert!(p.hollow, "a grown promise the depth never banked is hollow");
        let s = p.step(want / 2, want);
        assert!(!s.silence);
        p.note_read(true);
        assert!(
            !p.is_primed(),
            "growth that was never banked must still re-prime on its first click"
        );
    }

    /// A ring at the hard cap being asked deeper: `effective_target` clamps at the cap, the
    /// insert is below-target-only, so it can never fight the trim.
    #[test]
    fn the_insert_never_fights_the_trim() {
        let pm = per_ms(2);
        let want = 5 * pm;
        let t = JitterTuning::PIPEWIRE;
        let mut p = JitterPolicy::new(t, 2);
        // Ask for far more than the cap; sit the ring right at the cap.
        p.set_sync_target(Some(usize::MAX / 2));
        let cap = t.hard_cap_ms as usize * pm;
        let mut depth = cap;
        assert!(!p.step(depth, want).silence);
        depth -= want;
        p.note_read(false);
        let (mut trims, mut inserts) = (0, 0);
        for _ in 0..4_000 {
            depth += want; // producer keeps pace exactly
            let s = p.step(depth, want);
            if s.hard_trim {
                trims += 1;
            }
            if s.insert_front > 0 {
                inserts += 1;
            }
            depth = depth + s.insert_front - s.drop_front.min(depth) - want;
            p.note_read(false);
        }
        assert_eq!(
            trims, 0,
            "a ring holding exactly the cap must not be trimmed"
        );
        assert_eq!(
            inserts, 0,
            "a ring at the cap is at its (clamped) target — nothing to insert"
        );
    }

    /// The insert on the lossless plane: 2 ms frames at 96 kHz. `frame_samples` follows
    /// `set_frame_us`, and the seam fade is capped at half a frame (1 ms). A fade as long as
    /// the material it fades is not a crossfade.
    #[test]
    fn the_insert_follows_the_negotiated_frame_length() {
        let rate = 96_000;
        let mut p = JitterPolicy::new_at_rate(JitterTuning::PIPEWIRE, 2, rate);
        p.set_frame_us(2_000);
        let frame = p.frame_samples();
        assert_eq!(frame, 96 * 2 * 2, "2 ms at 96 kHz stereo is 384 samples");
        let want = frame; // a 2 ms device quantum
        let base = ms_to_samples(rate, 2, JitterTuning::PIPEWIRE.base_target_ms);
        let mut depth = base;
        assert!(!p.step(depth, want).silence);
        depth -= want;
        p.note_read(false);
        p.set_sync_target(Some(base * 3));
        let mut got = None;
        for _ in 0..20_000 {
            depth += want;
            let s = p.step(depth, want);
            if s.insert_front > 0 {
                got = Some(s);
                break;
            }
            depth -= want;
            p.note_read(false);
        }
        let s = got.expect("the insert never armed at 96 kHz");
        assert_eq!(s.insert_front, frame, "insert exactly one 2 ms frame");
        assert_eq!(s.crossfade, frame / 2, "the fade is capped at half a frame");
    }

    /// Mirror of `crossfade_drop_splices_without_a_step`, and stricter: both ends of the seam,
    /// including against the sample the device played just before the ring's head.
    #[test]
    fn crossfade_insert_adds_exactly_one_frame_and_the_seam_is_continuous() {
        use std::collections::VecDeque;
        // A slow ramp starting at 1000 — the device just played 999.
        let mut ring: VecDeque<f32> = (1000..2000).map(|i| i as f32).collect();
        let (insert, fade) = (240, 96);
        crossfade_insert(&mut ring, insert, fade);
        assert_eq!(
            ring.len(),
            1000 + insert,
            "net length change is exactly +insert"
        );
        // The copy is verbatim: the device plays the head once.
        for (i, &s) in ring.iter().take(insert).enumerate() {
            assert_eq!(s, (1000 + i) as f32, "copy sample {i}");
        }
        // The whole played sequence — including the step from the previously played sample
        // (999) into the ring — never jumps by more than the fade's slope. The seam sits at
        // `insert`: the copy's last sample (1239) blends from 1240 toward the replayed 1000,
        // so the local slope is at most (insert / fade + 1) per sample.
        let max_slope = (insert as f32 / fade as f32) + 2.0;
        let mut prev = 999.0f32;
        for (i, &s) in ring.iter().enumerate() {
            let step = (s - prev).abs();
            assert!(
                step <= max_slope,
                "sample {i}: step {step} from {prev} to {s} is a splice, not a fade"
            );
            prev = s;
        }
        for (i, &s) in ring.iter().enumerate().skip(insert + fade) {
            assert_eq!(s, (1000 + i - insert) as f32, "original sample {i}");
        }
        assert_eq!(ring[ring.len() - 1], 1999.0);
    }

    #[test]
    fn crossfade_insert_handles_degenerate_inputs() {
        use std::collections::VecDeque;
        let mut ring: VecDeque<f32> = (0..10).map(|i| i as f32).collect();
        crossfade_insert(&mut ring, 0, 4); // nothing to insert
        assert_eq!(ring.len(), 10);
        crossfade_insert(&mut ring, 99, 4); // more than we hold — refuse
        assert_eq!(ring.len(), 10);
        crossfade_insert(&mut ring, 10, 4); // exactly all of it: no room to fade, hard splice
        assert_eq!(ring.len(), 20);
        let v: Vec<f32> = ring.iter().copied().collect();
        let mut want: Vec<f32> = (0..10).map(|i| i as f32).collect();
        want.extend((0..10).map(|i| i as f32));
        assert_eq!(v, want, "a hard splice is a verbatim repeat");
        // A fade longer than the insert is clamped to it, not read out of bounds.
        let mut ring: VecDeque<f32> = (0..100).map(|i| i as f32).collect();
        crossfade_insert(&mut ring, 8, 50);
        assert_eq!(ring.len(), 108);
    }

    /// With the spare capacity the client rings reserve, an insert must not reallocate.
    /// `VecDeque::push_front` never grows inside capacity.
    #[test]
    fn crossfade_insert_does_not_reallocate_inside_capacity() {
        use std::collections::VecDeque;
        let mut ring: VecDeque<f32> = VecDeque::with_capacity(4096);
        ring.extend((0..1000).map(|i| i as f32));
        let cap = ring.capacity();
        crossfade_insert(&mut ring, 240, 96);
        assert_eq!(ring.capacity(), cap, "the insert reallocated the ring");
        assert_eq!(ring.len(), 1240);
    }

    /// The drop's seam, checked against the sample played just before the ring's head. Fade-out
    /// at `drop - fade + i` fails by a step of `drop - fade` samples.
    #[test]
    fn crossfade_drop_is_continuous_with_what_was_just_played() {
        use std::collections::VecDeque;
        let mut ring: VecDeque<f32> = (1000..2000).map(|i| i as f32).collect();
        let (drop, fade) = (240, 96);
        crossfade_drop(&mut ring, drop, fade);
        assert_eq!(ring.len(), 1000 - drop);
        let max_slope = (drop as f32 / fade as f32) + 2.0;
        let mut prev = 999.0f32; // the device just played 999; the ring's head was 1000
        for (i, &s) in ring.iter().enumerate() {
            let step = (s - prev).abs();
            assert!(
                step <= max_slope,
                "sample {i}: step {step} from {prev} to {s} is a splice, not a fade"
            );
            prev = s;
        }
        for (i, &s) in ring.iter().enumerate().skip(fade) {
            assert_eq!(s, (1000 + drop + i) as f32, "survivor {i}");
        }
    }

    /// Unity must be bit-exact. Callers gate on `gain != 1.0`, but if this ever stopped holding,
    /// every default session's wire would shift.
    #[test]
    fn unity_gain_is_bit_exact() {
        let src: Vec<f32> = (0..512).map(|i| (i as f32 / 512.0) * 2.0 - 1.0).collect();
        let mut got = src.clone();
        apply_gain(&mut got, 1.0);
        assert_eq!(got, src, "unity gain must not touch a single sample");
    }

    /// Below the knee the limiter is not in circuit at all. A boost whose peaks stay under
    /// `SOFT_LIMIT_KNEE` must be plain multiplication, or quiet material pays for a limiter it
    /// never needed.
    #[test]
    fn below_the_knee_is_plain_multiplication() {
        let mut got = vec![0.0, 0.1, -0.2, 0.34, -0.05];
        apply_gain(&mut got, 2.0);
        for (i, (g, s)) in got.iter().zip([0.0f32, 0.1, -0.2, 0.34, -0.05]).enumerate() {
            assert_eq!(*g, s * 2.0, "sample {i} must be untouched below the knee");
        }
    }

    /// No input, however absurdly gained, may leave the shaper out of range. Non-finite input
    /// must not escape as something the encoder would choke on.
    #[test]
    fn nothing_escapes_full_scale() {
        for gain in [1.5f32, 4.0, 8.0, 64.0, 1000.0] {
            let mut got: Vec<f32> = (0..401).map(|i| (i as f32 - 200.0) / 200.0).collect();
            apply_gain(&mut got, gain);
            for s in &got {
                assert!(s.abs() <= 1.0, "gain {gain} produced {s}");
            }
        }
        assert_eq!(soft_limit(f32::INFINITY), 1.0);
        assert_eq!(soft_limit(f32::NEG_INFINITY), -1.0);
    }

    /// Monotonicity keeps the shaper a limiter rather than a fold-back distortion. Odd
    /// symmetry keeps its harmonics odd-only and its DC at zero.
    #[test]
    fn the_curve_is_monotonic_and_odd() {
        let mut prev = f32::NEG_INFINITY;
        for i in 0..=4000 {
            let x = (i as f32 - 2000.0) / 500.0; // -4.0 ..= 4.0
            let y = soft_limit(x);
            assert!(y >= prev, "not monotonic at {x}: {y} < {prev}");
            prev = y;
            assert!(
                (soft_limit(-x) + y).abs() < 1e-6,
                "not odd-symmetric at {x}"
            );
        }
    }

    /// The knee must not itself be an audible event. Both branches meet at the same value and
    /// the same slope, so the transfer curve has no corner. A piecewise limiter that gets this
    /// wrong just swaps the clip's discontinuity for a softer one.
    #[test]
    fn the_knee_has_no_corner() {
        let k = SOFT_LIMIT_KNEE;
        assert!((soft_limit(k) - k).abs() < 1e-6, "value jumps at the knee");
        let h = 1e-4;
        let below = (soft_limit(k) - soft_limit(k - h)) / h;
        let above = (soft_limit(k + h) - soft_limit(k)) / h;
        assert!((below - 1.0).abs() < 1e-2, "linear side slope {below}");
        assert!(
            (above - below).abs() < 1e-2,
            "slope jumps at the knee: {below} -> {above}"
        );
    }
}
