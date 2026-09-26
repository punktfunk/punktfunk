//! Handshake capability bits, codec pick, chroma idc, and CICP colour on Hello/Welcome.
//!
//! Each `*_CAP_*` constant is one bit of [`Hello::video_caps`], [`Hello::client_caps`],
//! [`Welcome::host_caps`], or [`Welcome::host_caps2`]. Zero (or a missing trailing byte)
//! is an older peer. Most features are capable-and-agreed: the client asks, the host
//! answers, and only then does the wire shape change.
//!
//! [`resolve_codec`] intersects advertised codecs with what the host can emit; PyroWave
//! is opt-in via [`Hello::preferred_codec`], not the default ladder. [`ColorInfo`] is the
//! static CICP; ST.2086 mastering metadata rides the HDR meta datagram instead.
//!
//! `VIDEO_CAP_MULTI_SLICE` is the last `video_caps` bit; `HOST_CAP_AUDIO_HIRES` is the
//! last `host_caps` bit — further host caps already use `host_caps2`. Evidence: `design/`.
//!
//! A new `*_CAP_*`, `CODEC_*` or `EXT_TAG_*` constant goes in the matching table in
//! `tests::CAP_TABLES` / `tests::EXT_TAGS`; `every_cap_constant_is_tabled` fails until it does.

/// [`Hello::video_caps`]: client can decode Main10. Without [`VIDEO_CAP_HDR`] this is
/// 10-bit SDR — Main10 under a BT.709 SDR VUI; neither display's colour state is touched.
pub const VIDEO_CAP_10BIT: u8 = 0x01;
/// [`Hello::video_caps`]: client can present BT.2020 PQ HDR10. Implies 10-bit; set with
/// [`VIDEO_CAP_10BIT`].
pub const VIDEO_CAP_HDR: u8 = 0x02;
/// [`Hello::video_caps`]: client can decode HEVC 4:4:4 and asked for it. The host emits
/// 4:4:4 only when this bit is set, HEVC won, the operator allows it, and the GPU can
/// encode 4:4:4; otherwise the session stays 4:2:0 and [`Welcome::chroma_format`] is the
/// real value. Independent of 10-bit / HDR.
pub const VIDEO_CAP_444: u8 = 0x04;
/// [`Hello::video_caps`]: client consumes per-AU host-timing datagrams (`HOST_TIMING_MAGIC`,
/// 0xCF). The host emits them only when this bit is set. Observability only.
pub const VIDEO_CAP_HOST_TIMING: u8 = 0x08;
/// [`Hello::video_caps`]: the reassembler keeps speed-test probe filler in its own
/// frame-index space ([`crate::packet::FLAG_PROBE`]). Without this, a mid-session probe
/// burns video indexes the pump never sees, so the next real AU looks like a multi-thousand
/// frame loss. The host probes only clients that set this bit; others get a zeroed
/// [`ProbeResult`].
pub const VIDEO_CAP_PROBE_SEQ: u8 = 0x10;
/// [`Hello::video_caps`]: the reassembler accepts streamed access units. Non-final blocks
/// use SENTINEL headers (`block_count == 0`, `frame_bytes == 0`, exactly
/// `max_data_per_block` data shards); the FINAL block carries real `frame_bytes` /
/// `block_count` and `FLAG_EOF`. A geometry mismatch drops the frame. Hosts stream only
/// to clients that set this bit; others get a whole-AU seal.
pub const VIDEO_CAP_STREAMED_AU: u8 = 0x20;
/// [`Hello::video_caps`]: client can open ChaCha20-Poly1305 session datagrams and wants
/// them (software-AES targets). The host grants only when `PUNKTFUNK_CHACHA20` allows,
/// answering [`Welcome::cipher`] `= 1` plus [`Welcome::key_chacha`]. Other clients keep
/// the AES-128-GCM Welcome byte-identical.
pub const VIDEO_CAP_CHACHA20: u8 = 0x40;
/// [`Hello::video_caps`]: the decoder accepts multi-slice AUs. The embedder sets this from
/// the decode stack — some mobile/TV SoCs wedge on multi-slice HEVC — not from a host
/// default. The host uses >1 slice only toward this bit (`PUNKTFUNK_NVENC_SLICES` still
/// overrides). Last free `video_caps` bit; the next cap needs a second byte (ABI bump).
pub const VIDEO_CAP_MULTI_SLICE: u8 = 0x80;

/// [`Welcome::host_caps`]: host applies
/// [`InputKind::GamepadState`](crate::input::InputKind::GamepadState) snapshots. A capable
/// client then sends full per-pad state (idempotent on the lossy datagram plane) instead
/// of per-transition button/axis events.
pub const HOST_CAP_GAMEPAD_STATE: u8 = 0x01;

/// [`Welcome::host_caps`]: host has a clipboard backend and the operator did not disable
/// it, so the client may offer the toggle. Nothing clipboard-related happens until a
/// [`ClipControl`] `{ enabled: true }` crosses (`design/clipboard-and-file-transfer.md`).
pub const HOST_CAP_CLIPBOARD: u8 = 0x02;

/// [`Welcome::host_caps`]: the inject backend can type committed Unicode
/// ([`InputKind::TextInput`](crate::input::InputKind::TextInput)). Windows and wlroots
/// can; KWin / libei / gamescope only press layout keycodes and leave this clear. Absent
/// the bit, the client keeps VK synthesis.
pub const HOST_CAP_TEXT_INPUT: u8 = 0x04;

/// [`Hello::client_caps`]: the client draws the host cursor locally from
/// [`CursorShape`](super::control::CursorShape) and
/// [`CursorState`](super::datagram::CursorState) `0xD0`. When the host answers
/// [`HOST_CAP_CURSOR`], it must stop blending the cursor into the video
/// (`SessionPlan.cursor_blend = false`) or the user sees it twice.
pub const CLIENT_CAP_CURSOR: u8 = 0x01;

/// [`Hello::client_caps`]: the presenter is vsync-aware and will send
/// [`PhaseReport`](super::control::PhaseReport)s so the host can phase-lock capture
/// (`design/phase-locked-capture.md`). Without the bit the host never arms the controller.
pub const CLIENT_CAP_PHASE_LOCK: u8 = 0x02;

/// [`Hello::client_caps`]: the client can decode the redundant desktop-audio plane
/// ([`AUDIO_RED_MAGIC`](super::datagram::AUDIO_RED_MAGIC), `0xD2`). Active only when the
/// host answers [`HOST_CAP_AUDIO_RED`]. A client may always set this bit: a host that
/// declines keeps the plain `0xC9` plane.
pub const CLIENT_CAP_AUDIO_RED: u8 = 0x04;
/// [`Hello::client_caps`]: the client understands the pad-audio plane
/// ([`PAD_AUDIO_MAGIC`](super::datagram::PAD_AUDIO_MAGIC), `0xD1`) and
/// [`HidOutput::AudioCtl`](super::datagram::HidOutput). Active only when the host answers
/// [`HOST_CAP_PAD_AUDIO`] and the pad's arrival declared a renderer
/// ([`crate::input::ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`] / `_SPEAKER`).
pub const CLIENT_CAP_PAD_AUDIO: u8 = 0x08;

/// [`Welcome::host_caps`]: the host can forward the cursor out-of-band (Linux portal
/// `SPA_META_Cursor`). Not gamescope (capture has no cursor) and not Windows (DWM
/// composites into the IDD frame). Set only when the client asked via [`CLIENT_CAP_CURSOR`].
pub const HOST_CAP_CURSOR: u8 = 0x08;

/// [`Welcome::host_caps`]: the host injects [`PenBatch`](super::pen::PenBatch) `0xCC/0x05`
/// into a virtual tablet (`design/pen-tablet-input.md`). Absent the bit, the client folds
/// pen into touch and [`NativeClient::send_pen`](crate::client::NativeClient::send_pen)
/// refuses.
pub const HOST_CAP_PEN: u8 = 0x10;

/// [`Welcome::host_caps`]: the wire is the redundant desktop-audio plane
/// ([`AUDIO_RED_MAGIC`](super::datagram::AUDIO_RED_MAGIC), `0xD2`), not plain `0xC9`. Set
/// only when the client asked. The host may drop back to `0xC9` mid-session (loss-gated),
/// so clients decode both tags and treat this bit as "expect redundancy", not "only
/// redundancy".
pub const HOST_CAP_AUDIO_RED: u8 = 0x20;
/// [`Welcome::host_caps`]: the host can capture pad audio onto
/// [`PAD_AUDIO_MAGIC`](super::datagram::PAD_AUDIO_MAGIC) `0xD1`. Set only when the client
/// asked via [`CLIENT_CAP_PAD_AUDIO`]. When both bits agree, the host emits `0xD1` toward
/// pads whose arrivals declared a renderer.
pub const HOST_CAP_PAD_AUDIO: u8 = 0x40;

/// [`Hello::client_caps`]: the client can play the lossless audio plane
/// ([`AUDIO_PCM_MAGIC`](super::datagram::AUDIO_PCM_MAGIC), `0xD3`) at the rate/depth it
/// asked in Hello, **and** the user turned it on. This plane costs 1.5–4.6 Mbps against
/// Opus's 256 kbps and sits outside the ABR loop, so both ends must ask. A client that
/// cannot open that output format must not set this bit.
pub const CLIENT_CAP_AUDIO_HIRES: u8 = 0x10;

/// [`Hello::client_caps`]: leave the host's own playback devices alone — tap the current
/// default instead of re-routing the mix onto a silent endpoint. Request-only: no
/// `HOST_CAP` echo; an older host ignores it and still re-routes. Concurrent sessions share
/// host-global wiring, so any live session that asked wins until it ends.
pub const CLIENT_CAP_KEEP_HOST_AUDIO: u8 = 0x20;

/// [`Hello::client_caps`]: the client parses the tagged extension block after Welcome's
/// frozen positional layout ([`EXT_TAG_PADDING`](super::EXT_TAG_PADDING)). The host
/// appends a block only toward this bit, so a client that leaves it clear still gets the
/// Welcome byte-identical to today's. `0x80` is the last free `client_caps` bit.
pub const CLIENT_CAP_EXT: u8 = 0x40;

/// [`Welcome::host_caps`]: the session is on the lossless audio plane
/// ([`AUDIO_PCM_MAGIC`](super::datagram::AUDIO_PCM_MAGIC), `0xD3`). A wire statement, not
/// an offer: the client must open from
/// [`Welcome::audio_rate_hz`](super::handshake::Welcome::audio_rate_hz) /
/// [`audio_bits`](super::handshake::Welcome::audio_bits) /
/// [`audio_frame_us`](super::handshake::Welcome::audio_frame_us), never from what it asked.
/// Unlike `0xD2`, the host does not drop back mid-session (the device is open at a fixed
/// format). Last free `host_caps` bit; the next cap needs a second byte (already
/// [`Welcome::host_caps2`]).
pub const HOST_CAP_AUDIO_HIRES: u8 = 0x80;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): idle-keepalive re-encodes
/// carry [`USER_FLAG_REPEAT`](crate::packet::USER_FLAG_REPEAT). Against a host that
/// advertises this, an unflagged AU is new content; against an older host the client must
/// treat activity as unknown.
pub const HOST_CAP2_REPEAT_MARK: u8 = 0x01;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): the injector puts wire touch
/// contacts on the desktop. Linux libei / gamescope-EIS / KWin set it; wlroots
/// virtual-pointer has no touch protocol, and Windows below build 1809 cannot create
/// `PT_TOUCH`. Without the bit a passthrough client falls back to trackpad — otherwise
/// contacts vanish with no error (`design/touch-client-overlay.md`).
pub const HOST_CAP2_TOUCH: u8 = 0x02;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): the host parses the tagged
/// extension block after `Start`'s 6 bytes. The client appends one only after seeing this
/// bit — Hello is first contact, with no host capability known yet, and stays frozen.
pub const HOST_CAP2_EXT: u8 = 0x04;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): the injector consumes
/// [`InputKind::Scroll`](crate::input::InputKind::Scroll) — source/phase-aware
/// normalized scroll. Without the bit the client's outbound seam converts each
/// event to the legacy `MouseScroll` vocabulary instead; the host never sees both.
pub const HOST_CAP2_SCROLL: u8 = 0x08;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): the host serves
/// [`ProbeRequest`](crate::quic::ProbeRequest)s from the moment the data plane is punched,
/// before its pipeline exists and without the one-per-10 s spacing, until the first video
/// frame leaves. That window is what the client's bring-up ramp measures the link in; a
/// client that does not see the bit bursts beside live video as before.
pub const HOST_CAP2_RAMP: u8 = 0x10;

/// [`Welcome::host_caps2`](crate::quic::Welcome::host_caps2): the host reads a
/// [`DeliveryReport`](super::control::DeliveryReport) every report window and divides a path
/// two sessions share by them (`abr::governor`). Toward this bit the client sends one per
/// window — 13 bytes against 750 ms; toward every other host it sends one while nothing is
/// arriving and one when the first packets land, because an older host logs each unknown
/// message. A host that leaves the bit clear therefore learns nothing about a session's air
/// after its first window, and its groups are left alone.
pub const HOST_CAP2_DELIVERY: u8 = 0x20;

/// [`Hello::video_codecs`]: H.264 / AVC. The software encode path emits H.264, so a client
/// that wants to stream from a GPU-less host must advertise this.
pub const CODEC_H264: u8 = 0x01;
/// [`Hello::video_codecs`]: H.265 / HEVC. A peer that omits [`Hello::video_codecs`] is
/// treated as HEVC-only.
pub const CODEC_HEVC: u8 = 0x02;
pub const CODEC_AV1: u8 = 0x04;
/// [`Hello::video_codecs`]: PyroWave (opt-in wired-LAN intra-only wavelet,
/// `design/pyrowave-codec-plan.md`). Deliberately absent from [`resolve_codec`]'s ladder:
/// selected only when the client also names it [`Hello::preferred_codec`] (or the operator
/// forces the mask). The bit means the bitstream of the vendored pin
/// (`crates/pyrowave-sys/vendor/pyrowave/PUNKTFUNK-VENDOR.txt`); upstream has no version
/// field, so a bitstream-changing vendor bump bumps the punktfunk protocol instead.
pub const CODEC_PYROWAVE: u8 = 0x08;

/// Pick the one codec the host will emit: client's [`Hello::video_codecs`] (`0` = older
/// client, HEVC-only) intersected with `host_capable`. `preferred` (`0` = none) wins when
/// it is in the shared set; else HEVC > AV1 > H.264. [`CODEC_PYROWAVE`] is not on that
/// ladder — only the preferred path can return it. `None` when nothing the ladder may pick
/// is shared; the caller refuses the session rather than emit an undecodable stream.
pub fn resolve_codec(client_codecs: u8, host_capable: u8, preferred: u8) -> Option<u8> {
    let shared = advertised(client_codecs) & host_capable;
    if shared == 0 {
        return None;
    }
    // `preferred` is a single-bit field by contract but a raw wire byte. Keep the lowest
    // set bit of the intersection so a multi-bit value cannot escape as a codec id
    // (`from_wire` would fold unknowns to HEVC, which may not even be shared).
    if preferred != 0 && shared & preferred != 0 {
        let want = shared & preferred;
        return Some(want & want.wrapping_neg());
    }
    [CODEC_HEVC, CODEC_AV1, CODEC_H264]
        .into_iter()
        .find(|&c| shared & c != 0)
}

/// What the client can decode. `0` is a missing codec byte: every pre-negotiation build
/// decoded HEVC. Spelled once so the pick and [`codec_preference_miss`] cannot disagree.
fn advertised(client_codecs: u8) -> u8 {
    if client_codecs == 0 {
        CODEC_HEVC
    } else {
        client_codecs
    }
}

/// Which side lacks the codec the client asked for, when [`resolve_codec`] could not honour
/// [`Hello::preferred_codec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecMiss {
    /// The client named a codec it never advertised — no decoder on that device.
    Client,
    /// The client decodes it; this host's encoders cannot emit it.
    Host,
    /// Neither side has it.
    Both,
}

/// Why the preference lost, for the operator line beside the negotiated codec. `None` when
/// there is no preference or it won — including [`CODEC_PYROWAVE`], which only a preference
/// can select. The pick stands either way; this only names the missing side.
pub fn codec_preference_miss(
    client_codecs: u8,
    host_capable: u8,
    preferred: u8,
) -> Option<CodecMiss> {
    let client = advertised(client_codecs);
    if preferred == 0 || client & host_capable & preferred != 0 {
        return None;
    }
    Some(
        match (client & preferred != 0, host_capable & preferred != 0) {
            (false, true) => CodecMiss::Client,
            (true, false) => CodecMiss::Host,
            _ => CodecMiss::Both,
        },
    )
}

/// HEVC `chroma_format_idc` 4:2:0. Default when a peer omits [`Welcome::chroma_format`].
pub const CHROMA_IDC_420: u8 = 1;
/// HEVC `chroma_format_idc` 4:4:4 (Range Extensions).
pub const CHROMA_IDC_444: u8 = 3;

/// Per-session CICP (ITU-T H.273) the host resolved, on [`Welcome`]. Configure the
/// decoder/presenter from these; do not infer from bitstream VUI. An older host omits the
/// bytes → [`ColorInfo::SDR_BT709`]. ST.2086 + CLL can change mid-stream, so they ride
/// [`HDR_META_MAGIC`] rather than this fixed struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    /// CICP colour primaries: 1 = BT.709, 9 = BT.2020.
    pub primaries: u8,
    /// CICP transfer characteristics: 1 = BT.709, 16 = PQ (SMPTE ST.2084), 18 = HLG.
    pub transfer: u8,
    /// CICP matrix coefficients: 1 = BT.709, 9 = BT.2020 non-constant-luminance.
    pub matrix: u8,
    /// `video_full_range_flag`: 0 = limited/studio range, 1 = full range.
    pub full_range: u8,
}

impl ColorInfo {
    pub const CP_BT709: u8 = 1;
    pub const CP_BT2020: u8 = 9;
    pub const TRC_BT709: u8 = 1;
    pub const TRC_PQ: u8 = 16;
    pub const TRC_HLG: u8 = 18;
    pub const MC_BT709: u8 = 1;
    /// CICP matrix 9: BT.2020 NCL. Never emit 10 (constant-luminance) — no client decodes it.
    pub const MC_BT2020_NCL: u8 = 9;

    /// Default when a peer omits the colour bytes.
    pub const SDR_BT709: ColorInfo = ColorInfo {
        primaries: Self::CP_BT709,
        transfer: Self::TRC_BT709,
        matrix: Self::MC_BT709,
        full_range: 0,
    };

    pub const HDR10_BT2020_PQ: ColorInfo = ColorInfo {
        primaries: Self::CP_BT2020,
        transfer: Self::TRC_PQ,
        matrix: Self::MC_BT2020_NCL,
        full_range: 0,
    };

    /// PQ also needs a [`HdrMeta`] datagram; HLG does not.
    pub fn is_hdr(&self) -> bool {
        self.transfer == Self::TRC_PQ || self.transfer == Self::TRC_HLG
    }
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self::SDR_BT709
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::pcm::BITS_16;
    use crate::audio::SAMPLE_RATE_HZ;
    use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode};
    use crate::quic::*;

    /// Every capability constant, grouped by the wire byte it is a bit of. Two features on
    /// one bit is a host advertising one and being read as the other, which no peer can
    /// report — so the tables are the wire's own record of what each byte spends.
    const CAP_TABLES: &[(&str, &[(&str, u8)])] = &[
        (
            "video_caps",
            &[
                ("VIDEO_CAP_10BIT", VIDEO_CAP_10BIT),
                ("VIDEO_CAP_HDR", VIDEO_CAP_HDR),
                ("VIDEO_CAP_444", VIDEO_CAP_444),
                ("VIDEO_CAP_HOST_TIMING", VIDEO_CAP_HOST_TIMING),
                ("VIDEO_CAP_PROBE_SEQ", VIDEO_CAP_PROBE_SEQ),
                ("VIDEO_CAP_STREAMED_AU", VIDEO_CAP_STREAMED_AU),
                ("VIDEO_CAP_CHACHA20", VIDEO_CAP_CHACHA20),
                ("VIDEO_CAP_MULTI_SLICE", VIDEO_CAP_MULTI_SLICE),
            ],
        ),
        (
            "client_caps",
            &[
                ("CLIENT_CAP_CURSOR", CLIENT_CAP_CURSOR),
                ("CLIENT_CAP_PHASE_LOCK", CLIENT_CAP_PHASE_LOCK),
                ("CLIENT_CAP_AUDIO_RED", CLIENT_CAP_AUDIO_RED),
                ("CLIENT_CAP_PAD_AUDIO", CLIENT_CAP_PAD_AUDIO),
                ("CLIENT_CAP_AUDIO_HIRES", CLIENT_CAP_AUDIO_HIRES),
                ("CLIENT_CAP_KEEP_HOST_AUDIO", CLIENT_CAP_KEEP_HOST_AUDIO),
                ("CLIENT_CAP_EXT", CLIENT_CAP_EXT),
            ],
        ),
        (
            "host_caps",
            &[
                ("HOST_CAP_GAMEPAD_STATE", HOST_CAP_GAMEPAD_STATE),
                ("HOST_CAP_CLIPBOARD", HOST_CAP_CLIPBOARD),
                ("HOST_CAP_TEXT_INPUT", HOST_CAP_TEXT_INPUT),
                ("HOST_CAP_CURSOR", HOST_CAP_CURSOR),
                ("HOST_CAP_PEN", HOST_CAP_PEN),
                ("HOST_CAP_AUDIO_RED", HOST_CAP_AUDIO_RED),
                ("HOST_CAP_PAD_AUDIO", HOST_CAP_PAD_AUDIO),
                ("HOST_CAP_AUDIO_HIRES", HOST_CAP_AUDIO_HIRES),
            ],
        ),
        (
            "host_caps2",
            &[
                ("HOST_CAP2_REPEAT_MARK", HOST_CAP2_REPEAT_MARK),
                ("HOST_CAP2_TOUCH", HOST_CAP2_TOUCH),
                ("HOST_CAP2_EXT", HOST_CAP2_EXT),
                ("HOST_CAP2_SCROLL", HOST_CAP2_SCROLL),
                ("HOST_CAP2_RAMP", HOST_CAP2_RAMP),
                ("HOST_CAP2_DELIVERY", HOST_CAP2_DELIVERY),
            ],
        ),
        (
            "video_codecs",
            &[
                ("CODEC_H264", CODEC_H264),
                ("CODEC_HEVC", CODEC_HEVC),
                ("CODEC_AV1", CODEC_AV1),
                ("CODEC_PYROWAVE", CODEC_PYROWAVE),
            ],
        ),
    ];

    /// The `Start` extension block's tag space: ids, not bits, so they only have to differ.
    const EXT_TAGS: &[(&str, u16)] = &[
        ("EXT_TAG_PADDING", EXT_TAG_PADDING),
        ("EXT_TAG_CLIENT", EXT_TAG_CLIENT),
        ("EXT_TAG_ABR", EXT_TAG_ABR),
        ("EXT_TAG_PRESET", EXT_TAG_PRESET),
    ];

    /// Within a byte, each constant is one bit and no bit is spent twice; tags are distinct
    /// ids. A peer reads a byte it did not write, so a collision is silent on both ends.
    #[test]
    fn cap_bytes_are_distinct_single_bits() {
        for (byte, consts) in CAP_TABLES {
            let mut taken = 0u8;
            for (name, bit) in *consts {
                assert_eq!(bit.count_ones(), 1, "{byte}: {name} is not a single bit");
                assert_eq!(
                    taken & bit,
                    0,
                    "{byte}: {name} = {bit:#04x} is already taken"
                );
                taken |= bit;
            }
        }
        for (i, (name, id)) in EXT_TAGS.iter().enumerate() {
            let earlier = &EXT_TAGS[..i];
            assert!(
                !earlier.iter().any(|(_, seen)| seen == id),
                "{name} = {id} is already taken"
            );
        }
    }

    /// The tables above cover every constant declared under these prefixes. Reads the two
    /// sources at compile time, so a bit added without a table entry fails here by name.
    /// A prefix nobody tables is a whole new byte and wants its own table and its own line.
    #[test]
    fn every_cap_constant_is_tabled() {
        let sources: [(&str, &[&str]); 2] = [
            (
                include_str!("caps.rs"),
                &[
                    "VIDEO_CAP_",
                    "CLIENT_CAP_",
                    "HOST_CAP_",
                    "HOST_CAP2_",
                    "CODEC_",
                ],
            ),
            (include_str!("handshake.rs"), &["EXT_TAG_"]),
        ];
        for (src, prefixes) in sources {
            for line in src.lines() {
                let Some(name) = line
                    .strip_prefix("pub const ")
                    .and_then(|rest| rest.split(':').next())
                else {
                    continue;
                };
                if !prefixes.iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                let tabled = CAP_TABLES
                    .iter()
                    .any(|(_, cs)| cs.iter().any(|(n, _)| *n == name))
                    || EXT_TAGS.iter().any(|(n, _)| *n == name);
                assert!(tabled, "{name} is missing from the capability tables");
            }
        }
    }

    #[test]
    fn host_cap_clipboard_bit_is_distinct_and_survives_welcome() {
        assert_ne!(HOST_CAP_CLIPBOARD, HOST_CAP_GAMEPAD_STATE);
        let mut w = Welcome {
            abi_version: 1,
            udp_port: 1,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0,
                max_data_per_block: 1024,
            },
            shard_payload: 1024,
            encrypt: false,
            key: [0; 16],
            salt: [0; 4],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: HOST_CAP_GAMEPAD_STATE | HOST_CAP_CLIPBOARD,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
            audio_layout: 0,
        };
        let got = Welcome::decode(&w.encode()).unwrap();
        assert_eq!(got.host_caps & HOST_CAP_CLIPBOARD, HOST_CAP_CLIPBOARD);
        assert_eq!(
            got.host_caps & HOST_CAP_GAMEPAD_STATE,
            HOST_CAP_GAMEPAD_STATE
        );
        w.host_caps = HOST_CAP_GAMEPAD_STATE;
        assert_eq!(
            Welcome::decode(&w.encode()).unwrap().host_caps & HOST_CAP_CLIPBOARD,
            0
        );
    }

    #[test]
    fn pad_audio_cap_bits_are_distinct() {
        assert_eq!(
            CLIENT_CAP_PAD_AUDIO & (CLIENT_CAP_CURSOR | CLIENT_CAP_PHASE_LOCK),
            0
        );
        assert_eq!(
            HOST_CAP_PAD_AUDIO
                & (HOST_CAP_GAMEPAD_STATE
                    | HOST_CAP_CLIPBOARD
                    | HOST_CAP_TEXT_INPUT
                    | HOST_CAP_CURSOR
                    | HOST_CAP_PEN),
            0
        );
        assert_eq!(CLIENT_CAP_PAD_AUDIO.count_ones(), 1);
        assert_eq!(HOST_CAP_PAD_AUDIO.count_ones(), 1);
    }

    #[test]
    fn keep_host_audio_cap_bit_is_distinct() {
        assert_eq!(
            CLIENT_CAP_KEEP_HOST_AUDIO
                & (CLIENT_CAP_CURSOR
                    | CLIENT_CAP_PHASE_LOCK
                    | CLIENT_CAP_AUDIO_RED
                    | CLIENT_CAP_PAD_AUDIO
                    | CLIENT_CAP_AUDIO_HIRES),
            0
        );
        assert_eq!(CLIENT_CAP_KEEP_HOST_AUDIO.count_ones(), 1);
    }

    #[test]
    fn resolve_codec_canonicalizes_a_multi_bit_preference() {
        // A peer may stuff its capability mask into `preferred`. The result must still be
        // one bit of the shared set; echoing the mask folds to HEVC and can pick a codec
        // the client cannot decode.
        assert_eq!(
            resolve_codec(CODEC_H264, CODEC_H264 | CODEC_AV1, CODEC_H264 | CODEC_AV1),
            Some(CODEC_H264)
        );
        let got = resolve_codec(
            CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            CODEC_AV1 | CODEC_HEVC,
        )
        .unwrap();
        assert_eq!(got.count_ones(), 1);
        assert_ne!(got & (CODEC_AV1 | CODEC_HEVC), 0);
    }
}
