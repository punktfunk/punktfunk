//! Wire channel order, the Opus (multi)stream layouts, the audio bitrate budget and the
//! per-platform channel maps.

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

/// PipeWire / SPA `enum spa_audio_channel` ids and names in wire order: MONO=2 FL=3 FR=4
/// FC=5 LFE=6 SL=7 SR=8 RL=12 RR=13. Names are what `spa_audio_parse_position` accepts. The
/// host's capture node and the client's playback node both read this one list, so a wire slot
/// always lands on the matching speaker. Counts other than 1, 6 and 8 get stereo.
pub fn spa_channel_order(channels: u8) -> &'static [(u32, &'static str)] {
    const MONO: (u32, &str) = (2, "MONO");
    const FL: (u32, &str) = (3, "FL");
    const FR: (u32, &str) = (4, "FR");
    const FC: (u32, &str) = (5, "FC");
    const LFE: (u32, &str) = (6, "LFE");
    const SL: (u32, &str) = (7, "SL");
    const SR: (u32, &str) = (8, "SR");
    const RL: (u32, &str) = (12, "RL");
    const RR: (u32, &str) = (13, "RR");
    match channels {
        1 => &[MONO],
        6 => &[FL, FR, FC, LFE, RL, RR],
        8 => &[FL, FR, FC, LFE, RL, RR, SL, SR],
        _ => &[FL, FR],
    }
}

/// [`spa_channel_order`] as the 64-slot (`SPA_AUDIO_MAX_CHANNELS`) position array of a
/// format pod. Slots past the count stay 0, unset. Identity routing: PipeWire maps each wire
/// slot to its speaker and downmixes when the sink has fewer.
pub fn spa_positions(channels: u8) -> [u32; 64] {
    let mut pos = [0u32; 64];
    for (slot, (id, _)) in pos.iter_mut().zip(spa_channel_order(channels)) {
        *slot = *id;
    }
    pos
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

    #[test]
    fn wasapi_masks_are_correct() {
        assert_eq!(wasapi_channel_mask(2), 0x3);
        assert_eq!(wasapi_channel_mask(6), 0x3F);
        assert_eq!(wasapi_channel_mask(8), 0x63F); // not 0xFF
        assert_eq!(wasapi_channel_mask(2).count_ones(), 2);
        assert_eq!(wasapi_channel_mask(6).count_ones(), 6);
        assert_eq!(wasapi_channel_mask(8).count_ones(), 8);
    }

    /// Pod form and property form of the channel map are the same layout, in wire order. A
    /// disagreement would swap channels with nothing in the log.
    #[test]
    fn spa_positions_match_wire_order() {
        let ids =
            |ch: u8| -> Vec<u32> { spa_channel_order(ch).iter().map(|(id, _)| *id).collect() };
        assert_eq!(ids(1), [2]);
        assert_eq!(ids(2), [3, 4]);
        assert_eq!(ids(6), [3, 4, 5, 6, 12, 13]);
        assert_eq!(ids(8), [3, 4, 5, 6, 12, 13, 7, 8]);
        for ch in [1u8, 2, 6, 8] {
            let pod = spa_positions(ch);
            assert_eq!(pod[..ch as usize], ids(ch)[..], "{ch} channels");
            assert!(
                pod[ch as usize..].iter().all(|&p| p == 0),
                "unset past the count"
            );
        }
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
}
