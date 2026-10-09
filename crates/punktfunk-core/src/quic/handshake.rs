//! The handshake's values: what a client asks for ([`Hello`]), what the host answers
//! ([`Welcome`]), and what a [`ClientHello`] adds beside them ([`SessionPreset`], [`LinkFacts`],
//! [`client_label`]). Their wire form is [`super::v2::hello`]; a field a peer leaves out reads
//! as its default.

#[cfg(doc)]
use super::v2::hello::ClientHello;
use super::*;
use crate::config::{CompositorPref, Config, FecConfig, GamepadPref, Mode, Role};

/// `client → host`: open the session. The host creates its virtual output at exactly `mode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub mode: Mode,
    /// Preferred compositor (`Auto` = host decides). Honored only if that backend is available;
    /// the resolved choice is [`Welcome::compositor`]. Omitted by older clients → `Auto`.
    pub compositor: CompositorPref,
    /// Preferred virtual gamepad (`Auto` = host `PUNKTFUNK_GAMEPAD`, else X-Box 360). Echoed in
    /// [`Welcome::gamepad`]. Omitted by older clients → `Auto`.
    pub gamepad: GamepadPref,
    /// Requested encoder bitrate, kbps. `0` = host default. Clamped and echoed in
    /// [`Welcome::bitrate_kbps`]. Omitted by older clients → `0`.
    pub bitrate_kbps: u32,
    /// Device label for pairing approval, UTF-8, at most [`HELLO_NAME_MAX`] bytes.
    /// Omitted → `None` (host uses a fingerprint-derived label).
    pub name: Option<String>,
    /// Store-qualified library id (`steam:570`) the host resolves against its own library.
    /// `None` = default session. At most [`HELLO_LAUNCH_MAX`] bytes. Omitted → `None`.
    pub launch: Option<String>,
    /// [`VIDEO_CAP_10BIT`] / [`VIDEO_CAP_HDR`]. Host enables 10-bit/HDR only when the bit is
    /// set, so `0` stays 8-bit BT.709. Omitted → `0`.
    pub video_caps: u8,
    /// Requested channels: `2` / `6` / `8`. Host echoes the capture count in
    /// [`Welcome::audio_channels`]. Omitted → stereo.
    pub audio_channels: u8,
    /// Decode bitfield: [`CODEC_H264`] / [`CODEC_HEVC`] / [`CODEC_AV1`]. Host reports the pick
    /// in [`Welcome::codec`]. A GPU-less host needs [`CODEC_H264`]. Omitted → `0`, which
    /// [`resolve_codec`] treats as HEVC-only.
    pub video_codecs: u8,
    /// Soft hint: one codec bit, or `0` = host precedence. Honored only if shared; else
    /// [`resolve_codec`] falls back. Omitted by older clients → `0`.
    pub preferred_codec: u8,
    /// Client-panel ST.2086 volume ([`HdrMeta`]) when [`VIDEO_CAP_HDR`] is set. Copied into
    /// the virtual-display EDID so the host tone-maps to this panel, and echoed as `0xCE`.
    /// Omitted / no HDR display → `None`.
    pub display_hdr: Option<HdrMeta>,
    /// Non-video bits ([`CLIENT_CAP_CURSOR`]). Omitted → `0`.
    pub client_caps: u8,
    /// Largest sealed video-shard payload this client accepts. Non-zero ⇒ mid-session
    /// `shard_payload` changes are safe, and the value is the jumbo ceiling. `0`: the host
    /// must not change sealed geometry mid-session.
    pub max_shard_payload: u16,
    /// Requested capture rate (`48_000`, `96_000`, or the 44.1 kHz family). A request, never
    /// a fact — the client opens its device from [`Welcome::audio_rate_hz`]. Requires
    /// [`CLIENT_CAP_AUDIO_HIRES`]. `0` and absence both decode to
    /// [`SAMPLE_RATE_HZ`](crate::audio::SAMPLE_RATE_HZ).
    pub audio_rate_hz: u32,
    /// Requested depth: [`BITS_16`](crate::audio::pcm::BITS_16) or
    /// [`BITS_24`](crate::audio::pcm::BITS_24). Host answers in [`Welcome::audio_bits`].
    /// `0`/absence → 16-bit; only `audio_layout` can force this byte — a 96 kHz/16-bit
    /// request emits the rate and stops.
    pub audio_bits: u8,
    /// Requested surround coupling, an [`AudioLayout`](crate::audio::AudioLayout) wire id.
    /// Host answers in [`Welcome::audio_layout`] with what it encodes. `0`/absence is the legacy
    /// coupling.
    pub audio_layout: u8,
    /// How this client fills its view when the frame's shape differs
    /// ([`VideoFit::wire`](crate::video_fit::VideoFit::wire)). A host that sizes the frame for
    /// another device (a join, a mirrored head) reframes to it. Last field: `0`/absence is Fit.
    pub video_fit: u8,
}

/// QUIC application close: client deliberate quit. Host tears the virtual display down
/// immediately (no keep-alive linger). Any other close still lingers for reconnect.
pub const QUIT_CLOSE_CODE: u32 = 0x51;

/// QUIC application close: dedicated-session game process exited. Sibling of
/// [`QUIT_CLOSE_CODE`]; clients that ignore it still end the session.
pub const APP_EXITED_CLOSE_CODE: u32 = 0x52;

/// Longest [`Hello`] device name (UTF-8 bytes). Truncated on encode, rejected on decode.
pub const HELLO_NAME_MAX: usize = 64;

/// Longest [`Hello::launch`] id (UTF-8 bytes). Ids are short; 128 bounds the length prefix.
pub const HELLO_LAUNCH_MAX: usize = 128;

/// [`Welcome::audio_codec`]: Opus on `0xC9` (48 kHz). `0` so absence and older hosts both
/// read as Opus; a declined hi-res session resolves here — silence is the unacceptable outcome.
pub const AUDIO_CODEC_OPUS: u8 = 0;
/// [`Welcome::audio_codec`] id `1`, reserved and unimplemented. The design numbers Opus=0,
/// FLAC=1, PCM=2; this id is burned so [`AUDIO_CODEC_PCM`] stays `2`. Why FLAC lost lives in
/// `crate::audio::pcm`. No host emits this; no client should accept it.
pub const AUDIO_CODEC_FLAC_RESERVED: u8 = 1;
/// [`Welcome::audio_codec`]: raw interleaved LE PCM on `0xD3` (`crate::audio::pcm`).
/// `2` because [`AUDIO_CODEC_FLAC_RESERVED`] holds `1`.
pub const AUDIO_CODEC_PCM: u8 = 2;

/// Longest [`ClientHello::client_label`] in UTF-8 bytes. A log field, so short.
pub const EXT_CLIENT_MAX: usize = 96;

/// `s` as a [`ClientHello::client_label`]: trimmed, control characters dropped, truncated to
/// [`EXT_CLIENT_MAX`] on a char boundary. Empty in means empty out, which is "say nothing".
pub fn client_label(s: &str) -> String {
    let mut out: String = s.trim().chars().filter(|c| !c.is_control()).collect();
    while out.len() > EXT_CLIENT_MAX {
        out.pop();
    }
    out
}

/// [`ClientHello::abr_features`] bit 0: the client reads the reason byte on
/// [`BitrateChanged`](super::control::BitrateChanged). The host sends that tenth byte only
/// toward this bit, because every client without it rejects an ack of any other length.
/// Core sets it for every embedder that links the controller reading it, not the embedder.
pub const EXT_ABR_ACK_REASON: u8 = 0x01;

/// The connect-options bit that dials a diagnostic session ([`ClientHello::probe_only`]): the
/// FFI `delivery_flags` and the JNI dial carry it.
pub const EXT_DELIVERY_PROBE_ONLY: u8 = 0x02;

pub use crate::transport::LinkFacts;

impl LinkFacts {
    /// [`ClientHello::link`] on the wire: `kind ‖ mbps u32`.
    pub fn encode(&self) -> [u8; 5] {
        let m = self.mbps.to_le_bytes();
        [self.kind, m[0], m[1], m[2], m[3]]
    }

    /// What [`encode`](Self::encode) wrote. A short value reads the missing fields as zero.
    pub fn decode(v: &[u8]) -> LinkFacts {
        let mut mbps = [0u8; 4];
        for (d, s) in mbps.iter_mut().zip(v.iter().skip(1)) {
            *d = *s;
        }
        LinkFacts {
            kind: v.first().copied().unwrap_or(0),
            mbps: u32::from_le_bytes(mbps),
        }
    }
}

/// Longest [`SessionPreset::id`], printable ASCII.
pub const PRESET_ID_MAX: usize = 32;
/// Longest [`SessionPreset::name`] in UTF-8 bytes.
pub const PRESET_NAME_MAX: usize = 64;

/// The preset a session was dialled with ([`ClientHello::preset`]). The id is the client's own
/// and stable across a rename; the name is for people. The host shows it and hands it to hooks
/// and plugins; it changes nothing about the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPreset {
    pub id: String,
    pub name: String,
}

impl SessionPreset {
    /// Bounded and stripped: id to printable ASCII, name to [`client_label`]'s rules, each
    /// truncated. `None` when the id is empty after that, since the id is what a host keys on.
    pub fn new(id: &str, name: &str) -> Option<SessionPreset> {
        let id: String = id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .take(PRESET_ID_MAX)
            .collect();
        let mut name: String = name.trim().chars().filter(|c| !c.is_control()).collect();
        while name.len() > PRESET_NAME_MAX {
            name.pop();
        }
        (!id.is_empty()).then_some(SessionPreset { id, name })
    }

    /// `id_len u8, id, name_len u8, name`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.id.len() + self.name.len());
        out.push(self.id.len() as u8);
        out.extend_from_slice(self.id.as_bytes());
        out.push(self.name.len() as u8);
        out.extend_from_slice(self.name.as_bytes());
        out
    }

    /// What [`encode`](Self::encode) wrote, re-bounded as [`new`](Self::new) does. A malformed
    /// value is no preset, never a failed handshake: the preset informs, it does not gate.
    pub fn decode(v: &[u8]) -> Option<SessionPreset> {
        let (&id_len, rest) = v.split_first()?;
        let id = rest.get(..id_len as usize)?;
        let (&name_len, rest) = rest.get(id_len as usize..)?.split_first()?;
        let name = rest.get(..name_len as usize)?;
        SessionPreset::new(
            std::str::from_utf8(id).ok()?,
            &String::from_utf8_lossy(name),
        )
    }
}

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    /// Seed/testing: frames the host will send (`0` = unbounded).
    pub frames: u32,
    /// Resolved compositor. [`Hello::compositor`] if available, else auto-detect.
    /// Older host omit → `Auto` (unknown).
    pub compositor: CompositorPref,
    /// Resolved virtual-gamepad backend. DualSense feedback (0xCD) only arrives if this is
    /// DualSense. Older host omit → `Auto` (assume X-Box 360).
    pub gamepad: GamepadPref,
    /// Encoder bitrate the host configured, kbps. Older host omit → `0` (unknown).
    pub bitrate_kbps: u32,
    /// Encode bit depth: `8` or `10` (Main10, only if [`VIDEO_CAP_10BIT`]). Omit → `8`.
    pub bit_depth: u8,
    /// CICP colour the host encodes with. Omit → [`ColorInfo::SDR_BT709`]. Mastering metadata
    /// arrives separately on [`HDR_META_MAGIC`].
    pub color: ColorInfo,
    /// HEVC `chroma_format_idc`: [`CHROMA_IDC_420`] or [`CHROMA_IDC_444`] (only if
    /// [`VIDEO_CAP_444`] and the GPU opened 4:4:4). Hint; SPS is authoritative. Omit → 4:2:0.
    pub chroma_format: u8,
    /// Channels the host will send on `0xC9`. Build the Opus decoder from this via
    /// [`crate::audio::layout_for`], never from the Hello request. Omit → `2`.
    pub audio_channels: u8,
    /// Codec the host will emit ([`resolve_codec`]). Build the decoder from this.
    /// Omit → [`CODEC_HEVC`].
    pub codec: u8,
    /// Host input bits ([`HOST_CAP_GAMEPAD_STATE`]): snapshots vs legacy per-transition
    /// events. Omit → `0`.
    pub host_caps: u8,
    /// Management-API port (game library). Distinct from the QUIC port.
    /// `0` = not advertised; the client uses 47990.
    pub mgmt_port: u16,
    /// [`GRANT_GAMEPAD`](super::GRANT_GAMEPAD)-family mask. The client uses this to skip
    /// capture that cannot land. Omit → [`GRANT_ALL`](super::GRANT_ALL).
    pub grants: u32,
    /// Seconds until access expires, measured when this Welcome is built. `0` = permanent
    /// (also older-host omit). Mid-session changes: [`AccessUpdate`](super::AccessUpdate).
    pub expires_in_secs: u32,
    /// Audio plane: [`AUDIO_CODEC_OPUS`] (`0xC9`/`0xD2`) or [`AUDIO_CODEC_PCM`] (`0xD3`).
    /// Never both, never switched mid-session. Omit → Opus.
    pub audio_codec: u8,
    /// Resolved capture rate — open the client device from this, never from Hello.
    /// WASAPI `AUTOCONVERTPCM` accepts 96 kHz against a 48 kHz engine and returns interpolated
    /// samples with no error; the host must decline rather than pad. Absent / `0` →
    /// [`SAMPLE_RATE_HZ`](crate::audio::SAMPLE_RATE_HZ).
    pub audio_rate_hz: u32,
    /// Resolved depth. Wrong stride desynchronises every sample (24-bit read at 2 bytes is
    /// noise). Absent / `0` / unsupported
    /// ([`depth_is_supported`](crate::audio::pcm::depth_is_supported)) → `BITS_16`.
    pub audio_bits: u8,
    /// Resolved `0xD3` frame duration, microseconds. `0` for Opus (fixed 5 ms on `0xC9`).
    /// Do not hardcode: [`frame_us_for`](crate::audio::pcm::frame_us_for) sizes against the
    /// path MTU (this plane is never fragmented; 96 kHz/24-bit at 1472 B only fits 2 ms).
    pub audio_frame_us: u16,
    /// Second host-capability byte ([`HOST_CAP2_REPEAT_MARK`](super::HOST_CAP2_REPEAT_MARK),
    /// [`HOST_CAP2_TOUCH`](super::HOST_CAP2_TOUCH)). Omit → `0`.
    pub host_caps2: u8,
    /// The surround coupling the host encodes, an [`AudioLayout`](crate::audio::AudioLayout)
    /// wire id: what [`Hello::audio_layout`] asked for when the host knows it, else `0`. Build
    /// the decoder from THIS. Omit → `0`, legacy.
    pub audio_layout: u8,
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary (a straddling char is
/// dropped whole). Shared by Hello name/launch and [`PairRequest`](super::PairRequest).
pub(super) fn truncate_to(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

impl Welcome {
    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::defaults(role);
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        // Client reassembler ceiling from the negotiated rate: 4× average frame at
        // bitrate_kbps (IDR headroom), floor 8 MiB, cap 64 MiB. Host never reassembles
        // video. bitrate 0 (pre-negotiation) keeps the 64 MiB default bound.
        if role == Role::Client && self.bitrate_kbps > 0 {
            let per_frame = (self.bitrate_kbps as usize).saturating_mul(125)
                / self.mode.refresh_hz.max(1) as usize;
            c.max_frame_bytes = per_frame.saturating_mul(4).clamp(8 << 20, 64 << 20);
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn codec_negotiation() {
        // Precedence HEVC > AV1 > H.264; preference 0.
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_HEVC | CODEC_AV1, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_AV1, CODEC_AV1 | CODEC_H264, 0),
            Some(CODEC_AV1)
        );
        assert_eq!(resolve_codec(CODEC_H264, CODEC_H264, 0), Some(CODEC_H264));
        // Software host (H.264 only) + HEVC-only client share nothing → refuse.
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, 0), None);
        // A client that names no codec is HEVC-only.
        assert_eq!(
            resolve_codec(0, CODEC_HEVC | CODEC_H264, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(resolve_codec(0, CODEC_H264, 0), None);
        // Soft preference overrides precedence when the host can emit it.
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_H264 | CODEC_HEVC, CODEC_H264),
            Some(CODEC_H264)
        );
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_AV1, CODEC_HEVC | CODEC_AV1, CODEC_AV1),
            Some(CODEC_AV1)
        );
        // Preferred codec not in the shared set → precedence.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_H264, CODEC_HEVC | CODEC_H264, CODEC_AV1),
            Some(CODEC_HEVC)
        );
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, CODEC_HEVC), None);
        // PyroWave is opt-in only: mutual support never auto-selects it.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC | CODEC_PYROWAVE, 0),
            Some(CODEC_HEVC)
        );
        // Only shared codec, still refused — an all-intra 200 Mbps stream must not be a fallback.
        assert_eq!(resolve_codec(CODEC_PYROWAVE, CODEC_PYROWAVE, 0), None);
        assert_eq!(
            resolve_codec(
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_PYROWAVE
            ),
            Some(CODEC_PYROWAVE)
        );
        // Preference against a host without the backend falls back to the ladder.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC, CODEC_PYROWAVE),
            Some(CODEC_HEVC)
        );
    }

    /// Codec ids are a registry, not a compact enum. `1` is burned: compacting PCM to `1`
    /// would make every shipped `2` read the wrong plane (silence, not an error).
    #[test]
    fn audio_codec_ids_match_the_design_doc_numbering() {
        assert_eq!(AUDIO_CODEC_OPUS, 0, "absence decodes to Opus");
        assert_eq!(AUDIO_CODEC_FLAC_RESERVED, 1);
        assert_eq!(AUDIO_CODEC_PCM, 2);
    }

    #[test]
    fn a_preset_reads_back_bounded() {
        let preset = SessionPreset::new("3f9a0c11e2b4", "Docked").unwrap();
        assert_eq!(SessionPreset::decode(&preset.encode()), Some(preset));
        // A hostile value is bounded, never a failed handshake.
        let long = SessionPreset::new(&"a".repeat(99), &"\u{7}n".repeat(99)).unwrap();
        assert_eq!(long.id.len(), PRESET_ID_MAX);
        assert!(long.name.len() <= PRESET_NAME_MAX && !long.name.contains('\u{7}'));
        assert_eq!(SessionPreset::decode(&[9, b'x']), None);
        assert_eq!(SessionPreset::decode(&[]), None);
        assert_eq!(SessionPreset::new(" ", "Docked"), None);
    }

    /// A short link value reads its missing fields as zero.
    #[test]
    fn link_facts_read_back_from_short_values() {
        let facts = LinkFacts {
            kind: 1,
            mbps: 1_000,
        };
        assert_eq!(LinkFacts::decode(&facts.encode()), facts);
        assert_eq!(
            LinkFacts::decode(&[2, 0xe8]),
            LinkFacts {
                kind: 2,
                mbps: 0xe8
            }
        );
        assert_eq!(LinkFacts::decode(&[]), LinkFacts::default());
    }

    #[test]
    fn client_label_is_bounded() {
        let label = client_label("  android 0.38.0 console/library\n ");
        assert_eq!(label, "android 0.38.0 console/library");
        // A multi-byte tail truncates on a char boundary, never mid-code-point.
        let long = client_label(&"é".repeat(80));
        assert!(long.len() <= EXT_CLIENT_MAX);
        assert_eq!(long.chars().count(), EXT_CLIENT_MAX / 2);
    }
}
