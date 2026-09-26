//! The `punktfunk/1` positional handshake — Hello / Welcome / Start — and their wire codecs.
//!
//! Trailing fields append. Older peers stop early; absence decodes to the documented default,
//! so a legacy Hello/Welcome stays byte-identical until a later field is non-default.
//! Emitting a later field forces every earlier placeholder, each encoding exactly what its
//! absence meant. The exception is [`Hello::display_hdr`]: a fixed-length block with no
//! placeholder, disambiguated by remaining length, which caps the post-HDR tail at
//! `HDR_META_BODY_LEN − 1` bytes.
//!
//! `Welcome`'s tail after offset 68 is conditional: ChaCha inserts 32 key bytes, so
//! `mgmt_port` and everything after it sit at 69 or 101. A field that lands at 68 is read
//! as `cipher` by shipped clients (fail-closed). Evidence: `design/hi-res-audio.md`,
//! `design/shard-payload-reneg.md`, tests in this module.
//!
//! That layout is frozen. A new field goes in the tagged extension block after it
//! ([`encode_ext_block`]), gated by [`CLIENT_CAP_EXT`] on Welcome and [`HOST_CAP2_EXT`] on
//! Start, so a peer that did not ask never sees a byte past the layout it knows. Hello is
//! first contact — the client does not know the host yet — and carries no block.

use super::*;
use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::crypto::SessionKey;
use crate::error::{PunktfunkError, Result};

/// `client → host`: open the session. The host creates its virtual output at exactly `mode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_version: u32,
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
    /// Device label for pairing approval, `len u8 || UTF-8` (≤ [`HELLO_NAME_MAX`]).
    /// Omitted by older clients → `None` (host uses a fingerprint-derived label).
    pub name: Option<String>,
    /// Store-qualified library id (`steam:570`) the host resolves against its own library.
    /// `None` = default session. After `name` as `len u8 || UTF-8` (≤ [`HELLO_LAUNCH_MAX`]);
    /// a zero-length name placeholder precedes it when `name` is absent. Omitted → `None`.
    pub launch: Option<String>,
    /// [`VIDEO_CAP_10BIT`] / [`VIDEO_CAP_HDR`]. Host enables 10-bit/HDR only when the bit is
    /// set, so `0` (older clients) stays 8-bit BT.709. After `launch`; forces name/launch
    /// placeholders. Omitted → `0`.
    pub video_caps: u8,
    /// Requested channels: `2` / `6` / `8`. Host echoes the capture count in
    /// [`Welcome::audio_channels`]. Non-stereo forces name/launch/video_caps placeholders.
    /// Omitted or `2` → stereo, so the stereo Hello stays byte-identical.
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
    /// Fixed [`super::datagram::HDR_META_BODY_LEN`]-byte body, no placeholder — presence is
    /// remaining length after `preferred_codec`. Omitted / no HDR display → `None`.
    pub display_hdr: Option<HdrMeta>,
    /// Non-video bits ([`CLIENT_CAP_CURSOR`]). After `display_hdr`; that block has no
    /// placeholder, so remaining length < `HDR_META_BODY_LEN` means no HDR and these bytes
    /// *are* the post-HDR tail. Budget: 1+2+4+1+1+1 = 10 of 27. Omitted / zero → `0`.
    pub client_caps: u8,
    /// Largest sealed video-shard payload this client accepts. Non-zero ⇒ mid-session
    /// `shard_payload` changes are safe, and the value is the jumbo ceiling. `0` = legacy:
    /// host must not change sealed geometry mid-session. 2 LE bytes after `client_caps`.
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
    /// coupling, so a stereo or legacy Hello never carries this byte unless `video_fit` forces it.
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

/// [`Welcome::cipher`]: AES-128-GCM. Default; the only id pre-cipher builds know.
pub const CIPHER_AES_128_GCM: u8 = 0;
/// [`Welcome::cipher`]: ChaCha20-Poly1305 (RFC 8439), via [`VIDEO_CAP_CHACHA20`].
pub const CIPHER_CHACHA20_POLY1305: u8 = 1;

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

/// Extension tag `1`: no-op filler. Carries nothing, so a peer skips it like any tag it
/// does not know. Tag `0` is reserved. Every tag is allocated here with a doc line, as
/// `quic/caps.rs` does for bits, and an id is never reused for a second meaning: a peer
/// that skips an unknown id cannot tell two meanings apart.
pub const EXT_TAG_PADDING: u16 = 1;

/// Extension tag `2` on `Start`: what the client calls itself, UTF-8, no NUL — its build and
/// the shell that dialled (`"android 0.38.0 console/library"`). A label for the host's log, never
/// a fact it acts on: two sessions from one device are told apart here instead of by capture.
/// Bounded by [`EXT_CLIENT_MAX`]; a longer value is truncated on a char boundary by
/// [`client_label`].
pub const EXT_TAG_CLIENT: u16 = 2;

/// Longest [`EXT_TAG_CLIENT`] value in UTF-8 bytes. A log field, so short.
pub const EXT_CLIENT_MAX: usize = 96;

/// `s` as an [`EXT_TAG_CLIENT`] value: trimmed, control characters dropped, truncated to
/// [`EXT_CLIENT_MAX`] on a char boundary. Empty in means empty out, which is "say nothing".
pub fn client_label(s: &str) -> String {
    let mut out: String = s.trim().chars().filter(|c| !c.is_control()).collect();
    while out.len() > EXT_CLIENT_MAX {
        out.pop();
    }
    out
}

/// Extension tag `3` on `Start`: one byte of ABR protocol features the client understands,
/// as a bitfield ([`EXT_ABR_ACK_REASON`] is bit 0). A later feature takes another bit here
/// rather than a tag of its own, so the host reads one byte and answers what it recognises.
/// An absent tag, an empty value or a zero byte is a client that understands none of them —
/// which is every client shipped so far.
pub const EXT_TAG_ABR: u16 = 3;

/// [`EXT_TAG_ABR`] bit 0: the client reads the reason byte on
/// [`BitrateChanged`](super::control::BitrateChanged). The host sends that tenth byte only
/// toward this bit, because every client without it rejects an ack of any other length.
/// Core sets it for every embedder that links the controller reading it, not the embedder.
pub const EXT_ABR_ACK_REASON: u8 = 0x01;

/// Extension tag `4` on `Start`: the settings preset this session was dialled with, as
/// [`SessionPreset::encode`] writes it. The id is the client's own and stable across a rename;
/// the name is for people. The host shows it and hands it to hooks and plugins; it changes
/// nothing about the stream. Absent when the client streams with its plain settings.
pub const EXT_TAG_PRESET: u16 = 4;

/// Longest [`SessionPreset::id`], printable ASCII.
pub const PRESET_ID_MAX: usize = 32;
/// Longest [`SessionPreset::name`] in UTF-8 bytes.
pub const PRESET_NAME_MAX: usize = 64;

/// The preset a session was dialled with ([`EXT_TAG_PRESET`]).
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

    /// The preset in a decoded block, re-bounded as [`new`](Self::new) does. A malformed
    /// value is no preset, never a failed handshake: the tag informs, it does not gate.
    pub fn from_ext(entries: &[(u16, &[u8])]) -> Option<SessionPreset> {
        let (_, v) = entries.iter().find(|(tag, _)| *tag == EXT_TAG_PRESET)?;
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

/// The extension entries a client appends to its `Start`: label, features, then the preset.
///
/// Empty toward a host without [`HOST_CAP2_EXT`](super::HOST_CAP2_EXT): that host reads
/// `Start`'s six frozen bytes and nothing else, so it never learns the ABR features and
/// never lengthens an ack — today's behaviour, reached by never being told. An empty label
/// or preset says nothing rather than saying nothing at length.
#[cfg(any(feature = "quic", test))]
pub(crate) fn start_ext<'a>(
    host_caps2: u8,
    label: &'a str,
    abr: &'a [u8],
    preset: &'a [u8],
) -> Vec<(u16, &'a [u8])> {
    if host_caps2 & super::HOST_CAP2_EXT == 0 {
        return Vec::new();
    }
    let mut out: Vec<(u16, &[u8])> = Vec::with_capacity(3);
    if !label.is_empty() {
        out.push((EXT_TAG_CLIENT, label.as_bytes()));
    }
    out.push((EXT_TAG_ABR, abr));
    if !preset.is_empty() {
        out.push((EXT_TAG_PRESET, preset));
    }
    out
}

/// The ABR feature byte of a decoded extension block; `0` when the tag is absent or empty.
/// Both ends read the byte through this so a missing tag and a zero byte cannot diverge.
pub fn ext_abr_features(entries: &[(u16, &[u8])]) -> u8 {
    entries
        .iter()
        .find(|(tag, _)| *tag == EXT_TAG_ABR)
        .and_then(|(_, v)| v.first().copied())
        .unwrap_or(0)
}

/// Largest extension block on the wire, its `ext_len` header included. The block is read
/// before the peer is trusted, so this bounds what one message makes the other side hold.
pub const EXT_MAX_BYTES: usize = 4096;

/// Most entries in one block. Tags are unique, so this only bounds a flood of zero-length
/// entries inside [`EXT_MAX_BYTES`].
pub const EXT_MAX_ENTRIES: usize = 64;

/// Encode `ext_len u16 || (tag u16 || len u16 || value)*`, the block that follows the
/// frozen positional layout. `Err` on a repeated tag, a value past `u16::MAX`, or a block
/// past [`EXT_MAX_BYTES`] / [`EXT_MAX_ENTRIES`] — the rules [`decode_ext_block`] enforces,
/// so a peer never emits what the other side would reject.
pub fn encode_ext_block(entries: &[(u16, &[u8])]) -> Result<Vec<u8>> {
    let bad = || PunktfunkError::InvalidArg("bad handshake extension");
    let dup = || PunktfunkError::InvalidArg("duplicate handshake extension tag");
    if entries.len() > EXT_MAX_ENTRIES {
        return Err(bad());
    }
    let mut b = vec![0u8, 0u8];
    for (i, (tag, value)) in entries.iter().enumerate() {
        if entries[..i].iter().any(|(seen, _)| seen == tag) {
            return Err(dup());
        }
        let len = u16::try_from(value.len()).map_err(|_| bad())?;
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(value);
    }
    if b.len() > EXT_MAX_BYTES {
        return Err(bad());
    }
    let len = (b.len() - 2) as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    Ok(b)
}

/// Parse a block written by [`encode_ext_block`]. An unknown tag comes back untouched for
/// the caller to skip, never an error — that is what lets a newer peer talk to an older
/// one. `Err` on a truncated or oversized block or a repeated tag: the handshake fails
/// closed rather than act on half of what the peer meant.
pub fn decode_ext_block(b: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let bad = || PunktfunkError::InvalidArg("bad handshake extension");
    let dup = || PunktfunkError::InvalidArg("duplicate handshake extension tag");
    let len = b
        .get(0..2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()) as usize)
        .ok_or_else(bad)?;
    if len + 2 > EXT_MAX_BYTES {
        return Err(bad());
    }
    let mut rest = b.get(2..2 + len).ok_or_else(bad)?;
    let mut out: Vec<(u16, &[u8])> = Vec::new();
    while !rest.is_empty() {
        let head = rest.get(0..4).ok_or_else(bad)?;
        let tag = u16::from_le_bytes([head[0], head[1]]);
        let value_len = u16::from_le_bytes([head[2], head[3]]) as usize;
        // Bounds-checked before the split: a length a peer made up must not slice.
        let value = rest.get(4..4 + value_len).ok_or_else(bad)?;
        if out.len() == EXT_MAX_ENTRIES {
            return Err(bad());
        }
        if out.iter().any(|&(seen, _)| seen == tag) {
            return Err(dup());
        }
        out.push((tag, value));
        rest = &rest[4 + value_len..];
    }
    Ok(out)
}

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub abi_version: u32,
    pub udp_port: u16,
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    pub encrypt: bool,
    pub key: [u8; 16],
    pub salt: [u8; 4],
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
    /// Session AEAD: [`CIPHER_AES_128_GCM`] or [`CIPHER_CHACHA20_POLY1305`]. Emitted only when
    /// non-zero, so AES Welcome stays byte-identical. Decode is fail-closed: unknown id is
    /// `Err`, never a silent AES fallback (that session would not decrypt).
    pub cipher: u8,
    /// Management-API port (game library). Distinct from `udp_port` and the QUIC control port.
    /// `0` = not advertised (older host; client uses 47990). After the cipher block (69, or 101
    /// with ChaCha); emitting it forces the `cipher` placeholder — see [`Welcome::encode`].
    pub mgmt_port: u16,
    /// [`GRANT_GAMEPAD`](super::GRANT_GAMEPAD)-family mask. The client uses this to skip
    /// capture that cannot land. Omit → [`GRANT_ALL`](super::GRANT_ALL).
    pub grants: u32,
    /// Seconds until access expires, measured when this Welcome is built. `0` = permanent
    /// (also older-host omit). Mid-session changes: [`AccessUpdate`](super::AccessUpdate).
    pub expires_in_secs: u32,
    /// 32-byte ChaCha20-Poly1305 key, present iff `cipher == 1`, at 69..101. The 16-byte `key`
    /// keeps its offset and stays independently random. Decode rejects `cipher == 1` with
    /// fewer than 32 key bytes.
    pub key_chacha: Option<[u8; 32]>,
    /// Audio plane: [`AUDIO_CODEC_OPUS`] (`0xC9`/`0xD2`) or [`AUDIO_CODEC_PCM`] (`0xD3`).
    /// Never both, never switched mid-session. Non-Opus forces the four audio fields and every
    /// earlier placeholder so Opus Welcome stays 68 bytes; a slip onto offset 68 is `cipher`.
    /// Offset 79 (AES) / 111 (ChaCha). Omit → Opus.
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
    /// [`HOST_CAP2_TOUCH`](super::HOST_CAP2_TOUCH)). Nonzero forces the audio-block placeholders.
    /// Offset 87 (AES) / 119 (ChaCha). Omit → `0`.
    pub host_caps2: u8,
    /// The surround coupling the host encodes, an [`AudioLayout`](crate::audio::AudioLayout)
    /// wire id: what [`Hello::audio_layout`] asked for when the host knows it, else `0`. Build
    /// the decoder from THIS. Last field: offset 88 (AES) / 120 (ChaCha). Omit → `0`, legacy.
    pub audio_layout: u8,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
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

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(22);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(self.compositor.to_u8()); // offset 20; older hosts read [0..20]
        b.push(self.gamepad.to_u8()); // offset 21
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // offset 22..26
                                                               // name at 26: `len u8 || UTF-8`. Omitted when None and no later field, so a Hello
                                                               // with neither name nor launch stays 26 bytes. A later non-default field forces
                                                               // every earlier placeholder (0-length name/launch, default trailing bytes) so
                                                               // that field lands at a deterministic offset.
        let ac_present = self.audio_channels != 2;
        let vcodecs_present = self.video_codecs != 0;
        let pref_present = self.preferred_codec != 0;
        let hdr_present = self.display_hdr.is_some();
        let ccaps_present = self.client_caps != 0;
        let msp_present = self.max_shard_payload != 0;
        // Wire `0` and the legacy 48 kHz / 16-bit both count as default: decode maps
        // absence to those values, so a struct that spells them must encode identical
        // bytes to one left at zero.
        let arate_present =
            self.audio_rate_hz != 0 && self.audio_rate_hz != crate::audio::SAMPLE_RATE_HZ;
        let abits_present = self.audio_bits != 0 && self.audio_bits != crate::audio::pcm::BITS_16;
        let alayout_present = self.audio_layout != 0;
        let vfit_present = self.video_fit != 0;
        let audio_present = arate_present || abits_present || alayout_present || vfit_present;
        let need_placeholders = self.video_caps != 0
            || ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present;
        match (&self.name, &self.launch) {
            (None, None) if !need_placeholders => {}
            (name, _) => {
                let n = truncate_to(name.as_deref().unwrap_or(""), HELLO_NAME_MAX);
                b.push(n.len() as u8);
                b.extend_from_slice(n.as_bytes());
            }
        }
        if self.launch.is_some() || need_placeholders {
            let l = truncate_to(self.launch.as_deref().unwrap_or(""), HELLO_LAUNCH_MAX);
            b.push(l.len() as u8);
            b.extend_from_slice(l.as_bytes());
        }
        if need_placeholders {
            b.push(self.video_caps);
        }
        if ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present
        {
            b.push(self.audio_channels);
        }
        if vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present
        {
            b.push(self.video_codecs);
        }
        if pref_present || hdr_present || ccaps_present || msp_present || audio_present {
            b.push(self.preferred_codec);
        }
        // No placeholder. Decoder uses remaining length, which caps the post-HDR tail at
        // HDR_META_BODY_LEN − 1 bytes.
        if let Some(m) = &self.display_hdr {
            super::datagram::write_hdr_meta_body(m, &mut b);
        }
        if ccaps_present || msp_present || audio_present {
            b.push(self.client_caps);
        }
        if msp_present || audio_present {
            b.extend_from_slice(&self.max_shard_payload.to_le_bytes());
        }
        // Emitted as 48 000 when only `audio_bits` is non-default: struct `0` still means
        // 48 kHz, and the bytes a decoder reads must match the struct.
        if audio_present {
            let rate = if arate_present {
                self.audio_rate_hz
            } else {
                crate::audio::SAMPLE_RATE_HZ
            };
            b.extend_from_slice(&rate.to_le_bytes());
        }
        // A 96 kHz/16-bit request stops after the rate; only a later field forces the depth
        // out, as 16 when it was left at zero.
        if abits_present || alayout_present || vfit_present {
            b.push(if abits_present {
                self.audio_bits
            } else {
                crate::audio::pcm::BITS_16
            });
        }
        if alayout_present || vfit_present {
            b.push(self.audio_layout);
        }
        // Last field: nothing can force it.
        if vfit_present {
            b.push(self.video_fit);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Hello> {
        if b.len() < 20 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Hello"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        // name/launch raw length bytes (including 0 placeholders and oversized garbage)
        // locate the tail, so a corrupt name never panics — later fields just miss and
        // decode to defaults.
        let name_len = b.get(26).copied().unwrap_or(0) as usize;
        let launch_off = 27 + name_len;
        let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
        let tail = launch_off + 1 + launch_len;

        // display_hdr presence is remaining length after preferred_codec: ≥ HDR_META_BODY_LEN
        // ⇒ the block is there. Sound only while the post-HDR tail stays under that many
        // bytes (budget on [`Hello::client_caps`]). Computed once; every later field
        // reads off `post_hdr`.
        let has_hdr = b.len().saturating_sub(tail + 4) >= super::datagram::HDR_META_BODY_LEN;
        let post_hdr = if has_hdr {
            tail + 4 + super::datagram::HDR_META_BODY_LEN
        } else {
            tail + 4
        };
        Ok(Hello {
            abi_version: u32at(4),
            mode: Mode {
                width: u32at(8),
                height: u32at(12),
                refresh_hz: u32at(16),
            },
            compositor: b
                .get(20)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(21)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            bitrate_kbps: b
                .get(22..26)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent / oversized / non-UTF-8 → None. Never fail the handshake over a label.
            name: (name_len > 0 && name_len <= HELLO_NAME_MAX)
                .then(|| {
                    b.get(27..27 + name_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            launch: (launch_len > 0 && launch_len <= HELLO_LAUNCH_MAX)
                .then(|| {
                    b.get(launch_off + 1..launch_off + 1 + launch_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            video_caps: b.get(tail).copied().unwrap_or(0),
            // Unsupported channel count must not build a decoder.
            audio_channels: crate::audio::normalize_channels(b.get(tail + 1).copied().unwrap_or(2)),
            // 0 = older client; resolve_codec treats it as HEVC-only.
            video_codecs: b.get(tail + 2).copied().unwrap_or(0),
            preferred_codec: b.get(tail + 3).copied().unwrap_or(0),
            // Presence is remaining length (`has_hdr`), not a flag.
            display_hdr: has_hdr
                .then(|| {
                    b.get(tail + 4..tail + 4 + super::datagram::HDR_META_BODY_LEN)
                        .map(super::datagram::read_hdr_meta_body)
                })
                .flatten(),
            client_caps: b.get(post_hdr).copied().unwrap_or(0),
            // 0 = no mid-session renegotiation, no jumbo.
            max_shard_payload: b
                .get(post_hdr + 1..post_hdr + 3)
                .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent or 0 → 48 kHz, as a real rate, so nothing downstream treats 0 as
            // 48 000. Unknown non-zero rates pass through: rewriting them would mislabel
            // the stream. The host answers in Welcome::audio_rate_hz.
            audio_rate_hz: b
                .get(post_hdr + 3..post_hdr + 7)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .filter(|&hz| hz != 0)
                .unwrap_or(crate::audio::SAMPLE_RATE_HZ),
            // Depth is a byte stride: unsupported values would unpack 0xD3 wrongly.
            // Fallback to 16 costs a hi-res session, never correctness.
            audio_bits: match b.get(post_hdr + 7).copied() {
                Some(d) if crate::audio::pcm::depth_is_supported(d) => d,
                _ => crate::audio::pcm::BITS_16,
            },
            // Verbatim: the host answers a layout it knows or `0`, so an id from a newer
            // client costs nothing here and must not be folded onto one it did not ask for.
            audio_layout: b.get(post_hdr + 8).copied().unwrap_or(0),
            // Verbatim for the same reason; the host reads it through `VideoFit::from_wire`.
            video_fit: b.get(post_hdr + 9).copied().unwrap_or(0),
        })
    }
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        self.encode_positional(false)
    }

    /// The positional layout plus a tagged extension block. The host calls this only
    /// toward a client that set [`CLIENT_CAP_EXT`]; every other client gets [`encode`]
    /// byte for byte. Forcing every placeholder puts the block at a fixed offset.
    ///
    /// [`encode`]: Welcome::encode
    pub fn encode_ext(&self, entries: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        let block = encode_ext_block(entries)?;
        let mut b = self.encode_positional(true);
        debug_assert_eq!(b.len(), Self::ext_off(self.cipher));
        b.extend_from_slice(&block);
        Ok(b)
    }

    /// Entries a host appended to this Welcome; empty when it sent none (an older host, or
    /// one that saw no [`CLIENT_CAP_EXT`]). Unknown tags come back for the caller to skip.
    pub fn decode_ext(b: &[u8]) -> Result<Vec<(u16, &[u8])>> {
        let off = Self::ext_off(b.get(68).copied().unwrap_or(CIPHER_AES_128_GCM));
        if b.len() <= off {
            return Ok(Vec::new());
        }
        decode_ext_block(&b[off..])
    }

    /// Where the block sits: the positional layout at its full 89 bytes, 121 with the
    /// ChaCha key. A block forces every placeholder, so `cipher` alone locates it.
    fn ext_off(cipher: u8) -> usize {
        if cipher == CIPHER_CHACHA20_POLY1305 {
            121
        } else {
            89
        }
    }

    /// `force_tail` emits every optional field at its placeholder value so an extension
    /// block lands at a fixed offset. `false` leaves the chain alone, which is what keeps
    /// a default AES Welcome at 68 bytes.
    fn encode_positional(&self, force_tail: bool) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.udp_port.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(match self.fec.scheme {
            FecScheme::Gf8 => 0,
            FecScheme::Gf16 => 1,
        });
        b.push(self.fec.fec_percent);
        b.extend_from_slice(&self.fec.max_data_per_block.to_le_bytes());
        b.extend_from_slice(&self.shard_payload.to_le_bytes());
        b.push(self.encrypt as u8);
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.salt);
        b.extend_from_slice(&self.frames.to_le_bytes());
        b.push(self.compositor.to_u8()); // offset 53; older clients read [0..53]
        b.push(self.gamepad.to_u8()); // offset 54
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // offset 55..59
        b.push(self.bit_depth); // offset 59
        b.push(self.color.primaries); // 60..64; older clients → SDR BT.709
        b.push(self.color.transfer);
        b.push(self.color.matrix);
        b.push(self.color.full_range);
        b.push(self.chroma_format); // offset 64; omit → 4:2:0
        b.push(self.audio_channels); // offset 65; omit → stereo
        b.push(self.codec); // offset 66; omit → HEVC
        b.push(self.host_caps); // offset 67; omit → 0
                                // Cipher at 68 + ChaCha key at 69..101, emitted only when non-AES so an AES
                                // Welcome stays byte-identical. Host only sets cipher toward VIDEO_CAP_CHACHA20.
        debug_assert_eq!(
            self.cipher == CIPHER_CHACHA20_POLY1305,
            self.key_chacha.is_some(),
            "key_chacha present iff cipher == 1"
        );
        // Frozen: a new field goes in the extension block, never here. A later tail field
        // forces every earlier placeholder (cipher=0, mgmt=0, …) — without that an AES
        // Welcome carrying mgmt_port puts the port's low byte at 68, where shipped clients
        // fail-close on unknown cipher. Audio presence is codec ≠ Opus, never silent Opus.
        let mgmt_present = self.mgmt_port != 0 || force_tail;
        let access_present =
            self.grants != super::access::GRANT_ALL || self.expires_in_secs != 0 || force_tail;
        let audio_present = self.audio_codec != AUDIO_CODEC_OPUS || force_tail;
        let caps2_present = self.host_caps2 != 0 || force_tail;
        let layout_present = self.audio_layout != 0 || force_tail;
        if self.cipher != CIPHER_AES_128_GCM
            || mgmt_present
            || access_present
            || audio_present
            || caps2_present
            || layout_present
        {
            b.push(self.cipher);
            if let Some(k) = &self.key_chacha {
                b.extend_from_slice(k);
            }
            if mgmt_present || access_present || audio_present || caps2_present || layout_present {
                b.extend_from_slice(&self.mgmt_port.to_le_bytes());
            }
            if access_present || audio_present || caps2_present || layout_present {
                b.extend_from_slice(&self.grants.to_le_bytes());
                b.extend_from_slice(&self.expires_in_secs.to_le_bytes());
            }
            if audio_present || caps2_present || layout_present {
                b.push(self.audio_codec);
                b.extend_from_slice(&self.audio_rate_hz.to_le_bytes());
                b.push(self.audio_bits);
                b.extend_from_slice(&self.audio_frame_us.to_le_bytes());
            }
            if caps2_present || layout_present {
                b.push(self.host_caps2);
            }
            // Last field: nothing can force it.
            if layout_present {
                b.push(self.audio_layout);
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Trailing from compositor (53) on is optional. mgmt_port, grants, audio, and
        // host_caps2 and audio_layout follow the cipher block — shifted 32 when a ChaCha
        // key precedes them — so they are read from `mgmt_off`, not a constant. Emitting a
        // later field forces every earlier one; audio_layout, the last, sits at 88 AES / 120 ChaCha.
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        // Absent → AES. Fail-closed: cipher==1 with a short key, or id ≥ 2, is Err.
        // A silent AES fallback would not decrypt.
        let cipher = b.get(68).copied().unwrap_or(CIPHER_AES_128_GCM);
        let key_chacha = match cipher {
            CIPHER_AES_128_GCM => None,
            CIPHER_CHACHA20_POLY1305 => {
                let bytes = b
                    .get(69..101)
                    .ok_or(PunktfunkError::InvalidArg("bad Welcome"))?;
                let mut k = [0u8; 32];
                k.copy_from_slice(bytes);
                Some(k)
            }
            _ => return Err(PunktfunkError::InvalidArg("bad Welcome")),
        };
        // After the cipher block. Absent → 0; client uses the compiled-in default.
        let mgmt_off = if cipher == CIPHER_CHACHA20_POLY1305 {
            101
        } else {
            69
        };
        let mgmt_port = b
            .get(mgmt_off..mgmt_off + 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // Absent (older host, or encode omitted GRANT_ALL/permanent) → GRANT_ALL / 0.
        let grants_off = mgmt_off + 2;
        let grants = b
            .get(grants_off..grants_off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(super::access::GRANT_ALL);
        let expires_in_secs = b
            .get(grants_off + 4..grants_off + 8)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // Absent → Opus at 48 kHz / 16-bit. Same bytes as a declined hi-res session.
        let audio_off = grants_off + 8;
        // Codec is verbatim: folding an unknown id onto Opus would play the wrong
        // plane as silence. Client refuses a plane it cannot play.
        let audio_codec = b.get(audio_off).copied().unwrap_or(AUDIO_CODEC_OPUS);
        // A rate off the supported set is folded, not carried: it sizes decode buffers, and
        // `u32::MAX` asks for hundreds of gigabytes. Supported rates pass through unchanged,
        // so 44100 still arrives as 44100 rather than mislabelled as 48000.
        let audio_rate_hz = b
            .get(audio_off + 1..audio_off + 5)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .filter(|&hz| crate::audio::pcm::rate_is_supported(hz))
            .unwrap_or(crate::audio::SAMPLE_RATE_HZ);
        // Depth feeds unpack stride: unsupported → 16, matching audio_channels.
        // Only a corrupt or future wire reaches this (pinned-TLS control stream).
        let audio_bits = match b.get(audio_off + 5).copied() {
            Some(d) if crate::audio::pcm::depth_is_supported(d) => d,
            _ => crate::audio::pcm::BITS_16,
        };
        // 0 stays 0: it means the host stated no duration, and each consumer has its own
        // default for that. Any other value floors at the ladder's shortest rung, because
        // conceal counts divide 50 ms by it — `frame_us = 1` asks for 50_000 packets of scratch.
        let audio_frame_us = b
            .get(audio_off + 6..audio_off + 8)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .map(|us| if us == 0 { 0 } else { us.max(1_000) })
            .unwrap_or(0);
        // Trails the audio block (87 AES / 119 ChaCha). Absent → 0.
        let host_caps2 = b.get(audio_off + 8).copied().unwrap_or(0);
        // Verbatim, like `audio_codec`: an id this build does not know must reach the client
        // as itself, so it can refuse the decoder rather than build the wrong one.
        let audio_layout = b.get(audio_off + 9).copied().unwrap_or(0);
        Ok(Welcome {
            abi_version: u32at(4),
            udp_port: u16at(8),
            mode: Mode {
                width: u32at(10),
                height: u32at(14),
                refresh_hz: u32at(18),
            },
            fec: FecConfig {
                scheme: if b[22] == 1 {
                    FecScheme::Gf16
                } else {
                    FecScheme::Gf8
                },
                fec_percent: b[23],
                max_data_per_block: u16at(24),
            },
            shard_payload: u16at(26),
            encrypt: b[28] != 0,
            key,
            salt,
            frames: u32at(49),
            compositor: b
                .get(53)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(54)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            bitrate_kbps: b
                .get(55..59)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent → 8 (the only depth those hosts encode).
            bit_depth: b.get(59).copied().unwrap_or(8),
            // Absent → SDR BT.709 limited.
            color: ColorInfo {
                primaries: b.get(60).copied().unwrap_or(ColorInfo::CP_BT709),
                transfer: b.get(61).copied().unwrap_or(ColorInfo::TRC_BT709),
                matrix: b.get(62).copied().unwrap_or(ColorInfo::MC_BT709),
                full_range: b.get(63).copied().unwrap_or(0),
            },
            // Absent / 0 / unknown → 4:2:0. Only CHROMA_IDC_444 flips the client.
            chroma_format: match b.get(64).copied() {
                Some(CHROMA_IDC_444) => CHROMA_IDC_444,
                _ => CHROMA_IDC_420,
            },
            // Absent → stereo. Non-{6,8} normalizes so a corrupt byte cannot build a decoder.
            audio_channels: crate::audio::normalize_channels(b.get(65).copied().unwrap_or(2)),
            // Absent / unknown → HEVC.
            codec: match b.get(66).copied() {
                Some(CODEC_H264) => CODEC_H264,
                Some(CODEC_AV1) => CODEC_AV1,
                Some(CODEC_PYROWAVE) => CODEC_PYROWAVE,
                _ => CODEC_HEVC,
            },
            // Absent → 0 (legacy per-transition events).
            host_caps: b.get(67).copied().unwrap_or(0),
            mgmt_port,
            grants,
            expires_in_secs,
            cipher,
            key_chacha,
            audio_codec,
            audio_rate_hz,
            audio_bits,
            audio_frame_us,
            host_caps2,
            audio_layout,
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // P1GameStream until the P2 packet rev lands
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        // ChaCha key when cipher==1 (decode guarantees Some); AES key otherwise.
        c.key = match (self.cipher, self.key_chacha) {
            (CIPHER_CHACHA20_POLY1305, Some(k)) => SessionKey::ChaCha20Poly1305(k),
            _ => SessionKey::Aes128Gcm(self.key),
        };
        c.salt = self.salt;
        // Client reassembler ceiling from the negotiated rate: 4× average frame at
        // bitrate_kbps (IDR headroom), floor 8 MiB, cap 64 MiB. Host never reassembles
        // video. bitrate 0 (pre-negotiation) keeps the 64 MiB p1_defaults bound.
        if role == Role::Client && self.bitrate_kbps > 0 {
            let per_frame = (self.bitrate_kbps as usize).saturating_mul(125)
                / self.mode.refresh_hz.max(1) as usize;
            c.max_frame_bytes = per_frame.saturating_mul(4).clamp(8 << 20, 64 << 20);
        }
        c
    }
}

impl Start {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.client_udp_port.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Start> {
        if b.len() < 6 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Start"));
        }
        Ok(Start {
            client_udp_port: u16::from_le_bytes([b[4], b[5]]),
        })
    }

    /// Start plus a tagged extension block at its fixed 6 bytes. The client sends one only
    /// after a Welcome carrying [`HOST_CAP2_EXT`]; Hello is frozen, so this is where a
    /// client-side extension rides.
    pub fn encode_ext(&self, entries: &[(u16, &[u8])]) -> Result<Vec<u8>> {
        let mut b = self.encode();
        b.extend_from_slice(&encode_ext_block(entries)?);
        Ok(b)
    }

    /// Entries a client appended to this Start; empty when it sent none. Call after
    /// [`Start::decode`] has accepted the prefix.
    pub fn decode_ext(b: &[u8]) -> Result<Vec<(u16, &[u8])>> {
        if b.len() <= 6 {
            return Ok(Vec::new());
        }
        decode_ext_block(&b[6..])
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::pcm::{depth_is_supported, frame_us_for, BITS_16, BITS_24};
    use crate::audio::SAMPLE_RATE_HZ;
    use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode, Role};
    use crate::quic::*;

    #[test]
    fn welcome_roundtrip() {
        let w = Welcome {
            abi_version: 1,
            udp_port: 9999,
            mode: Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 240,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [1, 2, 3, 4],
            frames: 600,
            compositor: CompositorPref::Gamescope,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 50_000,
            bit_depth: 10,
            color: ColorInfo::HDR10_BT2020_PQ,
            chroma_format: CHROMA_IDC_444,
            audio_channels: 2,
            codec: CODEC_H264,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);

        // 50 Mbps / 240 Hz → ~104 KB; the 8 MiB floor governs. Host never reassembles video.
        let cc = w.session_config(Role::Client);
        assert_eq!(cc.max_frame_bytes, 8 << 20);
        cc.validate().expect("derived client config validates");
        assert_eq!(w.session_config(Role::Host).max_frame_bytes, 64 << 20);
        let old_host = Welcome {
            bitrate_kbps: 0,
            ..w
        };
        assert_eq!(
            old_host.session_config(Role::Client).max_frame_bytes,
            64 << 20
        );
        // 1.5 Gbps at 60 Hz = 4 × 3.125 MB = 12.5 MB, between the 8 MiB floor and 64 MiB cap.
        let fat = Welcome {
            bitrate_kbps: 1_500_000,
            mode: Mode {
                width: 5120,
                height: 1440,
                refresh_hz: 60,
            },
            ..w
        };
        let derived = fat.session_config(Role::Client).max_frame_bytes;
        assert_eq!(derived, 4 * 1_500_000 * 125 / 60);
        assert!(derived > (8 << 20) && derived < (64 << 20));
    }

    #[test]
    fn welcome_cipher_negotiation_wire_and_back_compat() {
        use crate::crypto::SessionKey;
        let base = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 50_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: 0,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // AES Welcome is 68 bytes — the pre-cipher wire form.
        let enc = base.encode();
        assert_eq!(enc.len(), 68);
        assert_eq!(Welcome::decode(&enc).unwrap(), base);

        // Cipher byte at 68, 32-byte key at 69..101.
        let k32: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let cenc = cha.encode();
        assert_eq!(cenc.len(), 68 + 1 + 32);
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);

        let old_host = Welcome::decode(&cenc[..68]).unwrap();
        assert_eq!(old_host.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_host.key_chacha, None);

        // cipher==1 with a short key, or id ≥ 2, is Err. A silent AES fallback would not decrypt.
        assert!(Welcome::decode(&cenc[..69]).is_err());
        assert!(Welcome::decode(&cenc[..100]).is_err());
        let mut bad = cenc.clone();
        bad[68] = 2;
        assert!(Welcome::decode(&bad).is_err());

        let aes_cfg = base.session_config(Role::Client);
        assert_eq!(aes_cfg.key, SessionKey::Aes128Gcm([7u8; 16]));
        aes_cfg.validate().expect("AES config validates");
        let cha_cfg = cha.session_config(Role::Client);
        assert_eq!(cha_cfg.key, SessionKey::ChaCha20Poly1305(k32));
        cha_cfg.validate().expect("ChaCha config validates");

        // mgmt_port after cipher: without a cipher placeholder the port's low byte lands at
        // 68. 47991 is 0xBB57 → byte 68 = 0x57, an unknown id; shipped clients fail-close.
        let mgmt = Welcome {
            mgmt_port: 47991,
            ..base
        };
        let menc = mgmt.encode();
        assert_eq!(menc.len(), 68 + 1 + 2, "cipher placeholder + LE u16 port");
        assert_eq!(
            menc[68], CIPHER_AES_128_GCM,
            "the cipher byte MUST be present (as 0) so a current client still reads AES here"
        );
        assert_eq!(&menc[69..71], &47991u16.to_le_bytes());
        assert_eq!(Welcome::decode(&menc).unwrap(), mgmt);

        // ChaCha: port at 101..103, after the 32-byte key.
        let both = Welcome {
            mgmt_port: 47991,
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let benc = both.encode();
        assert_eq!(benc.len(), 68 + 1 + 32 + 2);
        assert_eq!(&benc[101..103], &47991u16.to_le_bytes());
        assert_eq!(Welcome::decode(&benc).unwrap(), both);

        // No advertised port: AES Welcome stays 68 bytes.
        assert_eq!(base.encode().len(), 68);
        assert_eq!(Welcome::decode(&enc).unwrap().mgmt_port, 0);
        assert_eq!(Welcome::decode(&cenc).unwrap().mgmt_port, 0);
        // Truncated tail is not half a port.
        assert_eq!(Welcome::decode(&menc[..70]).unwrap().mgmt_port, 0);

        // Access advert forces cipher=0 and mgmt=0 so the u32s land at 71..79 (AES)
        // or 103..111 (ChaCha).
        let guest = Welcome {
            grants: GRANT_PRESET_CONTROLLER_ONLY,
            expires_in_secs: 4 * 3600,
            ..base
        };
        let genc = guest.encode();
        assert_eq!(
            genc.len(),
            68 + 1 + 2 + 8,
            "cipher + mgmt placeholders + 2 u32s"
        );
        assert_eq!(genc[68], CIPHER_AES_128_GCM, "forced cipher placeholder");
        assert_eq!(
            &genc[69..71],
            &0u16.to_le_bytes(),
            "forced mgmt placeholder"
        );
        assert_eq!(&genc[71..75], &GRANT_PRESET_CONTROLLER_ONLY.to_le_bytes());
        assert_eq!(&genc[75..79], &(4u32 * 3600).to_le_bytes());
        assert_eq!(Welcome::decode(&genc).unwrap(), guest);
        assert_eq!(Welcome::decode(&genc).unwrap().mgmt_port, 0);

        // All three trailing features, behind a ChaCha key: 103..111.
        let full_chain = Welcome {
            mgmt_port: 47991,
            grants: GRANT_PRESET_VIEW_ONLY,
            expires_in_secs: 60,
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let fenc = full_chain.encode();
        assert_eq!(fenc.len(), 68 + 1 + 32 + 2 + 8);
        assert_eq!(&fenc[103..107], &GRANT_PRESET_VIEW_ONLY.to_le_bytes());
        assert_eq!(&fenc[107..111], &60u32.to_le_bytes());
        assert_eq!(Welcome::decode(&fenc).unwrap(), full_chain);

        // Shorter wire forms (pre-cipher, cipher-only, mgmt-port) → GRANT_ALL / permanent.
        for old in [&enc[..], &cenc[..], &menc[..]] {
            let w = Welcome::decode(old).unwrap();
            assert_eq!(w.grants, GRANT_ALL);
            assert_eq!(w.expires_in_secs, 0);
        }
        // Partial u32 is never half a mask; grants without expiry stay permanent.
        assert_eq!(Welcome::decode(&genc[..73]).unwrap().grants, GRANT_ALL);
        let g_only = Welcome::decode(&genc[..75]).unwrap();
        assert_eq!(g_only.grants, GRANT_PRESET_CONTROLLER_ONLY);
        assert_eq!(g_only.expires_in_secs, 0);

        // A reader that stops at 71 sees AES and unknown port; the advert does not perturb it.
        let old_view = Welcome::decode(&genc[..71]).unwrap();
        assert_eq!(old_view.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_view.mgmt_port, 0);
        assert_eq!(old_view, base);

        // GRANT_ALL / permanent emits no advert — still 68 bytes.
        assert_eq!(base.encode().len(), 68);
    }

    #[test]
    fn codec_negotiation_and_back_compat() {
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
        // Older client (0 = no codec byte) is HEVC-only.
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
        // Decode must not fold PyroWave to HEVC (that sent wavelet AUs into an HEVC decoder).
        let mut pw_w = Welcome::decode(
            &Welcome {
                abi_version: 2,
                udp_port: 1,
                mode: Mode {
                    width: 1280,
                    height: 720,
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
                codec: CODEC_PYROWAVE,
                host_caps: 0,
                mgmt_port: 0,
                grants: GRANT_ALL,
                expires_in_secs: 0,
                cipher: 0,
                key_chacha: None,
                audio_codec: AUDIO_CODEC_OPUS,
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
                audio_layout: 0,
                audio_frame_us: 0,
                host_caps2: 0,
            }
            .encode(),
        )
        .unwrap();
        assert_eq!(pw_w.codec, CODEC_PYROWAVE);
        // Unknown future bit still folds to HEVC.
        pw_w.codec = 0x40;
        assert_eq!(Welcome::decode(&pw_w.encode()).unwrap().codec, CODEC_HEVC);

        // Extra trailing codec bytes are skipped by a build that ignores them.
        let h = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: CODEC_H264 | CODEC_HEVC,
            preferred_codec: CODEC_H264,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let enc = h.encode();
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec.video_codecs, CODEC_H264 | CODEC_HEVC);
        assert_eq!(dec.preferred_codec, CODEC_H264);
        // Drop preferred_codec: video_codecs intact, preference 0.
        let no_pref = &enc[..enc.len() - 1];
        assert_eq!(
            Hello::decode(no_pref).unwrap().video_codecs,
            CODEC_H264 | CODEC_HEVC
        );
        assert_eq!(Hello::decode(no_pref).unwrap().preferred_codec, 0);
        // No video_codecs/preferred bytes → 0 (HEVC-only).
        let legacy = &enc[..enc.len() - 2];
        assert_eq!(Hello::decode(legacy).unwrap().video_codecs, 0);
        assert_eq!(Hello::decode(legacy).unwrap().preferred_codec, 0);

        // No codec byte → HEVC.
        let mut w = Welcome::decode(
            &Welcome {
                abi_version: 2,
                udp_port: 1,
                mode: h.mode,
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
                codec: CODEC_H264,
                host_caps: 0,
                mgmt_port: 0,
                grants: GRANT_ALL,
                expires_in_secs: 0,
                cipher: 0,
                key_chacha: None,
                audio_codec: AUDIO_CODEC_OPUS,
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
                audio_layout: 0,
                audio_frame_us: 0,
                host_caps2: 0,
            }
            .encode(),
        )
        .unwrap();
        assert_eq!(w.codec, CODEC_H264);
        w.codec = CODEC_HEVC;
        let wenc = w.encode();
        assert_eq!(
            Welcome::decode(&wenc[..wenc.len() - 1]).unwrap().codec,
            CODEC_HEVC
        );
    }

    #[test]
    fn hello_start_roundtrip() {
        let h = Hello {
            abi_version: 1,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 120,
            },
            compositor: CompositorPref::Kwin,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 25_000,
            name: Some("Test Device".into()),
            launch: Some("steam:570".into()),
            video_caps: VIDEO_CAP_10BIT,
            audio_channels: 2,
            video_codecs: CODEC_H264 | CODEC_HEVC,
            preferred_codec: CODEC_HEVC,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn hello_welcome_compositor_back_compat() {
        // Truncation both ways: missing trailing bytes → Auto; extra bytes ignored.
        let h = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Mutter,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 80_000,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let enc = h.encode();
        assert_eq!(enc.len(), 26);
        // 20-byte Hello → both Auto, no bitrate.
        let legacy = Hello::decode(&enc[..20]).unwrap();
        assert_eq!(legacy.compositor, CompositorPref::Auto);
        assert_eq!(legacy.gamepad, GamepadPref::Auto);
        assert_eq!(legacy.bitrate_kbps, 0);
        assert_eq!(legacy.mode, h.mode);
        // 21-byte Hello → compositor intact, gamepad Auto.
        let mid = Hello::decode(&enc[..21]).unwrap();
        assert_eq!(mid.compositor, CompositorPref::Mutter);
        assert_eq!(mid.gamepad, GamepadPref::Auto);
        // 22-byte Hello → gamepad intact, bitrate 0.
        let pre_bitrate = Hello::decode(&enc[..22]).unwrap();
        assert_eq!(pre_bitrate.gamepad, GamepadPref::DualSense);
        assert_eq!(pre_bitrate.bitrate_kbps, 0);
        // Full message carries bitrate.
        assert_eq!(Hello::decode(&enc).unwrap().bitrate_kbps, 80_000);

        let w = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: h.mode,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [3u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Kwin,
            gamepad: GamepadPref::Xbox360,
            bitrate_kbps: 120_000,
            bit_depth: 10,
            color: ColorInfo::HDR10_BT2020_PQ,
            chroma_format: CHROMA_IDC_444,
            audio_channels: 6,
            codec: CODEC_HEVC,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        let wenc = w.encode();
        assert_eq!(wenc.len(), 68); // 60 base + colour + chroma + audio + codec + host-caps
        let legacy_w = Welcome::decode(&wenc[..53]).unwrap();
        assert_eq!(legacy_w.compositor, CompositorPref::Auto);
        assert_eq!(legacy_w.gamepad, GamepadPref::Auto);
        assert_eq!(legacy_w.bitrate_kbps, 0);
        assert_eq!(legacy_w.frames, 0);
        assert_eq!(legacy_w.key, w.key);
        let mid_w = Welcome::decode(&wenc[..54]).unwrap();
        assert_eq!(mid_w.compositor, CompositorPref::Kwin);
        assert_eq!(mid_w.gamepad, GamepadPref::Auto);
        // 55-byte Welcome → gamepad intact, bitrate 0.
        let pre_bitrate_w = Welcome::decode(&wenc[..55]).unwrap();
        assert_eq!(pre_bitrate_w.gamepad, GamepadPref::Xbox360);
        assert_eq!(pre_bitrate_w.bitrate_kbps, 0);
        assert_eq!(pre_bitrate_w.bit_depth, 8); // no trailing byte → 8-bit
        assert_eq!(legacy_w.bit_depth, 8);
        // 60-byte Welcome → SDR BT.709.
        let pre_color_w = Welcome::decode(&wenc[..60]).unwrap();
        assert_eq!(pre_color_w.bit_depth, 10);
        assert_eq!(pre_color_w.color, ColorInfo::SDR_BT709);
        assert_eq!(pre_color_w.chroma_format, CHROMA_IDC_420); // no chroma byte → 4:2:0
        assert_eq!(legacy_w.color, ColorInfo::SDR_BT709);
        assert_eq!(legacy_w.chroma_format, CHROMA_IDC_420);
        // 64-byte Welcome: colour, no chroma/audio → 4:2:0 + stereo.
        let pre_chroma_w = Welcome::decode(&wenc[..64]).unwrap();
        assert_eq!(pre_chroma_w.color, ColorInfo::HDR10_BT2020_PQ);
        assert_eq!(pre_chroma_w.chroma_format, CHROMA_IDC_420);
        assert_eq!(pre_chroma_w.audio_channels, 2); // offset 65 absent → stereo
                                                    // 65-byte Welcome: chroma, no audio → 4:4:4 + stereo.
        let pre_audio_w = Welcome::decode(&wenc[..65]).unwrap();
        assert_eq!(pre_audio_w.chroma_format, CHROMA_IDC_444);
        assert_eq!(pre_audio_w.audio_channels, 2);
        assert_eq!(Welcome::decode(&wenc).unwrap().bitrate_kbps, 120_000);
        assert_eq!(Welcome::decode(&wenc).unwrap().bit_depth, 10);
        assert_eq!(
            Welcome::decode(&wenc).unwrap().color,
            ColorInfo::HDR10_BT2020_PQ
        );
        assert_eq!(
            Welcome::decode(&wenc).unwrap().chroma_format,
            CHROMA_IDC_444
        );
        assert_eq!(Welcome::decode(&wenc).unwrap().audio_channels, 6);
        // 67-byte Welcome → host_caps 0; full form carries the bit.
        assert_eq!(Welcome::decode(&wenc[..67]).unwrap().host_caps, 0);
        assert_eq!(
            Welcome::decode(&wenc).unwrap().host_caps,
            HOST_CAP_GAMEPAD_STATE
        );
    }

    #[test]
    fn hello_name_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: Some("Enrico's MacBook".into()),
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let enc = base.encode();
        assert_eq!(
            Hello::decode(&enc).unwrap().name.as_deref(),
            Some("Enrico's MacBook")
        );
        // 26-byte peer ignores the trailing name; named host reading 26 bytes → None.
        assert_eq!(Hello::decode(&enc[..26]).unwrap().name, None);
        // No name → 26 bytes, same as the bitrate-era form.
        let unnamed = Hello {
            name: None,
            ..base.clone()
        };
        assert_eq!(unnamed.encode().len(), 26);
        // Over-long names truncate on a char boundary within HELLO_NAME_MAX.
        let long = Hello {
            name: Some(format!("{}ü", "x".repeat(HELLO_NAME_MAX - 1))), // ü straddles HELLO_NAME_MAX
            ..base.clone()
        };
        let dec = Hello::decode(&long.encode()).unwrap();
        let n = dec.name.expect("truncated name still present");
        assert!(n.len() <= HELLO_NAME_MAX && n.starts_with('x'));
        // Corrupt length or bad UTF-8 → None, never Err.
        let mut bad_len = unnamed.encode();
        bad_len.push(40); // claims 40 name bytes, none follow
        assert_eq!(Hello::decode(&bad_len).unwrap().name, None);
        let mut bad_utf8 = unnamed.encode();
        bad_utf8.extend_from_slice(&[2, 0xFF, 0xFE]);
        assert_eq!(Hello::decode(&bad_utf8).unwrap().name, None);
    }

    #[test]
    fn hello_launch_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        // Launch alone: a zero-length name placeholder keeps the offset deterministic.
        let with_launch = Hello {
            launch: Some("steam:570".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&with_launch.encode()).unwrap(), with_launch);
        let both = Hello {
            name: Some("Enrico's Mac".into()),
            launch: Some("custom:abc123".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // Name, no launch → launch None.
        let name_only = Hello {
            name: Some("Enrico's Mac".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&name_only.encode()).unwrap().launch, None);
        // Neither field → 26 bytes, no launch placeholder.
        assert_eq!(base.encode().len(), 26);
        assert_eq!(Hello::decode(&base.encode()).unwrap().launch, None);
        // 26-byte peer ignores a trailing launch.
        assert_eq!(
            Hello::decode(&with_launch.encode()[..26]).unwrap().launch,
            None
        );
        // Over-long ids truncate on a char boundary within HELLO_LAUNCH_MAX.
        let long = Hello {
            launch: Some(format!("{}ü", "x".repeat(HELLO_LAUNCH_MAX - 1))),
            ..base.clone()
        };
        let dec = Hello::decode(&long.encode())
            .unwrap()
            .launch
            .expect("present");
        assert!(dec.len() <= HELLO_LAUNCH_MAX && dec.starts_with('x'));
    }

    #[test]
    fn hello_display_hdr_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 3840,
                height: 2160,
                refresh_hz: 120,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: VIDEO_CAP_10BIT | VIDEO_CAP_HDR,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], // G, B, R
            white_point: [15635, 16450],                                       // D65
            max_display_mastering_luminance: 8_000_000,                        // 800 nits
            min_display_mastering_luminance: 500,                              // 0.05 nits
            max_cll: 0,
            max_fall: 400,
        };
        let with_hdr = Hello {
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&with_hdr.encode()).unwrap(), with_hdr);
        // display_hdr alone still lands at a deterministic offset (placeholders through the tail).
        let hdr_only = Hello {
            video_caps: 0,
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        // Decode that stops at preferred_codec ignores the block; older Hello → None.
        let enc = with_hdr.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - HDR_META_BODY_LEN]).unwrap(),
            Hello {
                display_hdr: None,
                ..with_hdr.clone()
            }
        );
        assert_eq!(Hello::decode(&base.encode()).unwrap().display_hdr, None);
        // Truncated block → None, never a partial HdrMeta.
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 1]).unwrap().display_hdr,
            None
        );
        // 26 + 6 placeholders (name/launch/caps/channels/codecs/pref) + body.
        assert_eq!(hdr_only.encode().len(), 26 + 6 + HDR_META_BODY_LEN);
    }

    #[test]
    fn control_messages_disjoint_from_hello() {
        // Hello uses MAGIC (PKF1); control uses CTL_MAGIC (PKFc). No overlap at any abi.
        for abi in [1u32, 2, 16, 0x10, 0x0113, 0x1410] {
            let h = Hello {
                abi_version: abi,
                mode: Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                compositor: CompositorPref::Auto,
                gamepad: GamepadPref::Auto,
                bitrate_kbps: 0,
                name: None,
                launch: None,
                video_caps: 0,
                audio_channels: 2,
                video_codecs: 0,
                preferred_codec: 0,
                display_hdr: None,
                client_caps: 0,
                max_shard_payload: 0,
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
                audio_layout: 0,
                video_fit: 0,
            }
            .encode();
            assert!(PairRequest::decode(&h).is_err(), "abi {abi} parsed as pair");
            assert!(Reconfigure::decode(&h).is_err());
        }
        // PairRequest never parses as Hello.
        let pr = PairRequest {
            name: "x".into(),
            spake_a: vec![0u8; 33],
            device_key: Vec::new(),
        }
        .encode();
        assert!(Hello::decode(&pr).is_err());
    }
    #[test]
    fn hello_client_caps_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        // Caps without HDR: remaining < HDR_META_BODY_LEN, so not a truncated HdrMeta.
        let caps_only = Hello {
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&caps_only.encode()).unwrap(), caps_only);
        // Caps after the fixed HDR block.
        let both = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // HDR without caps is the pre-caps wire form (caps 0).
        let hdr_only = Hello {
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        assert_eq!(Hello::decode(&base.encode()).unwrap().client_caps, 0);
        // Truncating the caps byte: nothing before it moved.
        let enc = both.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 1]).unwrap(),
            Hello {
                client_caps: 0,
                max_shard_payload: 0,
                ..both.clone()
            }
        );
    }

    /// `max_shard_payload` forces earlier placeholders, composes with HDR, degrades to 0.
    #[test]
    fn hello_max_shard_payload_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        // Advertisement alone: earlier trailing fields are placeholders so the 2 LE bytes land.
        let adv = Hello {
            max_shard_payload: crate::config::max_shard_payload() as u16,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&adv.encode()).unwrap(), adv);
        // Remaining-length disambiguation must still find caps and payload after HDR.
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        let full = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 8908,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&full.encode()).unwrap(), full);
        // No trailing bytes → 0: host must not change sealed geometry mid-session.
        assert_eq!(Hello::decode(&base.encode()).unwrap().max_shard_payload, 0);
        // Truncating the 2 trailing bytes drops the advertisement only.
        let enc = full.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 2]).unwrap(),
            Hello {
                max_shard_payload: 0,
                ..full.clone()
            }
        );
    }

    /// Codec ids are a registry, not a compact enum. `1` is burned: compacting PCM to `1`
    /// would make every shipped `2` read the wrong plane (silence, not an error).
    #[test]
    fn audio_codec_ids_match_the_design_doc_numbering() {
        assert_eq!(AUDIO_CODEC_OPUS, 0);
        assert_eq!(AUDIO_CODEC_FLAC_RESERVED, 1);
        assert_eq!(AUDIO_CODEC_PCM, 2);
        // Opus is 0 so an absent field is the legacy wire.
        assert_eq!(AUDIO_CODEC_OPUS, 0, "absence decodes to Opus");
    }

    /// Welcome tail is conditional: cipher at 68, ChaCha key at 69..101, so later fields sit
    /// at 79 (AES) or 111 (ChaCha). A decoder that uses a fixed offset, or a test that only
    /// covers AES, breaks the ChaCha path. See `design/hi-res-audio.md`.
    #[test]
    fn welcome_hires_audio_wire_under_both_ciphers() {
        let base = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 50_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: 0,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // Opus session stays 68 bytes — the pre-cipher / pre-hi-res wire.
        assert_eq!(base.encode().len(), 68);
        assert_eq!(Welcome::decode(&base.encode()).unwrap(), base);

        // A layout answer trails host_caps2 and forces the whole tail out. Cut before it, the
        // Welcome reads as an older host's: legacy coupling.
        let coupled = Welcome {
            audio_layout: 1,
            ..base
        };
        let enc = coupled.encode();
        assert_eq!(enc.len(), 89);
        assert_eq!(enc[88], 1);
        assert_eq!(Welcome::decode(&enc).unwrap(), coupled);
        assert_eq!(Welcome::decode(&enc[..88]).unwrap().audio_layout, 0);

        // Presence is codec alone. Rate/depth must not put the block on an Opus Welcome.
        let opus_with_stray_format = Welcome {
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            audio_frame_us: 2000,
            ..base
        };
        assert_eq!(
            opus_with_stray_format.encode().len(),
            68,
            "only audio_codec puts the block on the wire"
        );

        // Frame duration from the ladder: 96 kHz/24-bit stereo in ~1400 B is 2 ms.
        // This plane is never fragmented; a hardcoded 2.5 ms would not send.
        let frame_us = frame_us_for(96_000, BITS_24, 2, 1400).expect("a rung fits the default MTU");
        assert_eq!(
            frame_us, 2000,
            "the documented rung at the default MTU ceiling"
        );
        let hires = Welcome {
            host_caps: HOST_CAP_AUDIO_HIRES,
            audio_codec: AUDIO_CODEC_PCM,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            audio_frame_us: frame_us as u16,
            ..base
        };

        // AES: audio block at 79..87 (cipher 1 + mgmt 2 + grants 4 + expiry 4 past 68).
        let enc = hires.encode();
        assert_eq!(enc.len(), 87, "68 + cipher 1 + mgmt 2 + access 8 + audio 8");
        assert_eq!(enc[68], CIPHER_AES_128_GCM, "forced cipher placeholder");
        assert_eq!(&enc[69..71], &0u16.to_le_bytes(), "forced mgmt placeholder");
        assert_eq!(&enc[71..75], &GRANT_ALL.to_le_bytes(), "forced grants");
        assert_eq!(&enc[75..79], &0u32.to_le_bytes(), "forced expiry");
        assert_eq!(enc[79], AUDIO_CODEC_PCM);
        assert_eq!(&enc[80..84], &96_000u32.to_le_bytes());
        assert_eq!(enc[84], BITS_24);
        assert_eq!(&enc[85..87], &(frame_us as u16).to_le_bytes());
        assert_eq!(Welcome::decode(&enc).unwrap(), hires);

        // ChaCha: same eight bytes at 111..119.
        let k32: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..hires
        };
        let cenc = cha.encode();
        assert_eq!(cenc.len(), 119, "87 + the 32-byte ChaCha key");
        assert_eq!(&cenc[101..103], &0u16.to_le_bytes(), "forced mgmt");
        assert_eq!(&cenc[103..107], &GRANT_ALL.to_le_bytes(), "forced grants");
        assert_eq!(&cenc[107..111], &0u32.to_le_bytes(), "forced expiry");
        assert_eq!(cenc[111], AUDIO_CODEC_PCM);
        assert_eq!(&cenc[112..116], &96_000u32.to_le_bytes());
        assert_eq!(cenc[116], BITS_24);
        assert_eq!(&cenc[117..119], &(frame_us as u16).to_le_bytes());
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);
        // Same eight bytes, 32-byte offset: decoder position comes from `cipher`, not a constant.
        assert_eq!(&enc[79..87], &cenc[111..119]);

        // Forced placeholders must decode as their own absence, or hi-res would invent a
        // mgmt port / access mask.
        for w in [
            Welcome::decode(&enc).unwrap(),
            Welcome::decode(&cenc).unwrap(),
        ] {
            assert_eq!(w.mgmt_port, 0, "forced placeholder, not an advertised port");
            assert_eq!(w.grants, GRANT_ALL, "forced placeholder, full control");
            assert_eq!(w.expires_in_secs, 0, "forced placeholder, permanent");
            assert!(depth_is_supported(w.audio_bits));
        }

        // Composes with real mgmt/grants values, not only zeros.
        let guest_hires = Welcome {
            mgmt_port: 47991,
            grants: GRANT_PRESET_CONTROLLER_ONLY,
            expires_in_secs: 4 * 3600,
            ..hires
        };
        let genc = guest_hires.encode();
        assert_eq!(
            genc.len(),
            87,
            "same length — the placeholders were already paid for"
        );
        assert_eq!(&genc[69..71], &47991u16.to_le_bytes());
        assert_eq!(&genc[71..75], &GRANT_PRESET_CONTROLLER_ONLY.to_le_bytes());
        assert_eq!(&genc[79..87], &enc[79..87], "the audio block is unmoved");
        assert_eq!(Welcome::decode(&genc).unwrap(), guest_hires);

        // Shorter wire forms are Opus at 48 kHz / 16-bit (a real rate, not raw 0).
        let mgmt_era = Welcome {
            mgmt_port: 47991,
            ..base
        }
        .encode();
        for old in [&base.encode()[..], &mgmt_era[..], &enc[..79], &cenc[..111]] {
            let w = Welcome::decode(old).unwrap();
            assert_eq!(w.audio_codec, AUDIO_CODEC_OPUS);
            assert_eq!(w.audio_rate_hz, SAMPLE_RATE_HZ);
            assert_eq!(w.audio_bits, BITS_16);
            assert_eq!(w.audio_frame_us, 0);
        }
        // Prefix through 79: HOST_CAP_AUDIO_HIRES rides host_caps at 67, not the audio block.
        assert_eq!(
            Welcome::decode(&enc[..79]).unwrap(),
            Welcome {
                host_caps: HOST_CAP_AUDIO_HIRES,
                ..base
            }
        );

        // Cut mid-u32: rate is the legacy value, not two bytes of 96 000.
        let torn = Welcome::decode(&enc[..82]).unwrap();
        assert_eq!(torn.audio_codec, AUDIO_CODEC_PCM, "the codec byte survived");
        assert_eq!(torn.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(torn.audio_frame_us, 0);

        // Unsupported depth → 16 so a 0xD3 unpack never walks the wrong stride.
        let mut bad_depth = enc.clone();
        bad_depth[84] = 32;
        assert_eq!(Welcome::decode(&bad_depth).unwrap().audio_bits, BITS_16);
        // A supported rate is verbatim. Clamping 44100 to 48000 would mislabel the stream.
        let mut odd_rate = enc.clone();
        odd_rate[80..84].copy_from_slice(&44_100u32.to_le_bytes());
        assert_eq!(Welcome::decode(&odd_rate).unwrap().audio_rate_hz, 44_100);

        // Off the supported set the rate folds: it sizes decode buffers, and both the ABI and
        // the client session size from it before libopus ever sees the value.
        let mut absurd_rate = enc.clone();
        absurd_rate[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Welcome::decode(&absurd_rate).unwrap().audio_rate_hz,
            SAMPLE_RATE_HZ
        );

        // `frame_us` divides 50 ms into a conceal-packet count, so a 1 µs frame asks for
        // 50_000 packets of scratch. Floor at the ladder's shortest rung; 0 stays absent.
        let mut tiny_frame = enc.clone();
        tiny_frame[85..87].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(Welcome::decode(&tiny_frame).unwrap().audio_frame_us, 1_000);
        let mut absent_frame = enc.clone();
        absent_frame[85..87].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(Welcome::decode(&absent_frame).unwrap().audio_frame_us, 0);
    }

    /// `host_caps2` past the audio block: presence forces earlier placeholders to their
    /// absence-defaults; older host omit → 0.
    #[test]
    fn welcome_host_caps2_wire_under_both_ciphers() {
        let base = Welcome {
            abi_version: 1,
            udp_port: 1,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 10,
                max_data_per_block: 4096,
            },
            shard_payload: 1408,
            encrypt: true,
            key: [7; 16],
            salt: [3; 4],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 20_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: super::super::access::GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // Zero is not emitted: Welcome stays 68 bytes.
        assert_eq!(base.encode().len(), 68);

        // AES: placeholders forced, byte at 87.
        let marked = Welcome {
            host_caps2: HOST_CAP2_REPEAT_MARK,
            ..base
        };
        let enc = marked.encode();
        assert_eq!(enc.len(), 88);
        let got = Welcome::decode(&enc).unwrap();
        assert_eq!(got, marked);
        // Placeholders decode as absence: not hi-res, not empty grants, not a mgmt port.
        assert_eq!(got.audio_codec, AUDIO_CODEC_OPUS);
        assert_eq!(got.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(got.grants, super::super::access::GRANT_ALL);
        assert_eq!(got.mgmt_port, 0);

        // ChaCha: byte at 119, total 120.
        let chacha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some([9; 32]),
            host_caps2: HOST_CAP2_REPEAT_MARK,
            ..base
        };
        let enc = chacha.encode();
        assert_eq!(enc.len(), 120);
        assert_eq!(Welcome::decode(&enc).unwrap(), chacha);

        // Shorter wire → 0, never an error.
        assert_eq!(Welcome::decode(&base.encode()).unwrap().host_caps2, 0);
    }

    /// Hello trailing fields have placeholders except `display_hdr` (fixed 28-byte block,
    /// remaining-length). That caps the post-HDR tail at 27 bytes.
    #[test]
    fn hello_video_fit_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 3216,
                height: 1440,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        // Fit is absence: the legacy 26 bytes.
        assert_eq!(base.encode().len(), 26);

        // A fit forces every earlier placeholder, the audio triple as its legacy values, and
        // lands last; cut before it, the Hello reads as an older client's Fit.
        let crop = Hello {
            video_fit: crate::video_fit::VideoFit::Crop.wire(),
            ..base.clone()
        };
        let enc = crop.encode();
        assert_eq!(enc.len(), 26 + 6 + 1 + 2 + 4 + 1 + 1 + 1);
        assert_eq!(
            &enc[35..39],
            &SAMPLE_RATE_HZ.to_le_bytes(),
            "rate forced as 48 kHz"
        );
        assert_eq!(enc[39], BITS_16, "depth forced as 16");
        assert_eq!(enc[40], 0, "layout forced as legacy");
        assert_eq!(enc[41], 1);
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec, crop);
        assert_eq!(dec.audio_layout, 0);
        assert_eq!(Hello::decode(&enc[..41]).unwrap().video_fit, 0);

        // With the HDR block and every other tail field the post-HDR tail stays under
        // HDR_META_BODY_LEN, so a Hello without HDR is never read as one with.
        let full = Hello {
            display_hdr: Some(HdrMeta {
                display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
                white_point: [15635, 16450],
                max_display_mastering_luminance: 8_000_000,
                min_display_mastering_luminance: 500,
                max_cll: 0,
                max_fall: 400,
            }),
            client_caps: CLIENT_CAP_AUDIO_HIRES,
            max_shard_payload: 8908,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            audio_layout: 1,
            video_fit: crate::video_fit::VideoFit::Stretch.wire(),
            ..base.clone()
        };
        let fenc = full.encode();
        let post_hdr = fenc.len() - (26 + 6 + HDR_META_BODY_LEN);
        assert_eq!(post_hdr, 10);
        assert!(post_hdr < HDR_META_BODY_LEN);
        assert_eq!(Hello::decode(&fenc).unwrap(), full);
    }

    #[test]
    fn hello_hires_audio_request_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            video_fit: 0,
        };
        // Legacy request is still 26 bytes.
        assert_eq!(base.encode().len(), 26);
        assert_eq!(Hello::decode(&base.encode()).unwrap(), base);

        // A layout ask forces the depth out as the legacy 16 and lands after it; cut before
        // it, the Hello reads as an older client's: legacy coupling.
        let coupled = Hello {
            audio_layout: 1,
            ..base.clone()
        };
        let enc = coupled.encode();
        assert_eq!(enc.len(), 26 + 6 + 1 + 2 + 4 + 1 + 1);
        assert_eq!(enc[39], BITS_16, "depth forced out as 16");
        assert_eq!(enc[40], 1);
        assert_eq!(Hello::decode(&enc).unwrap(), coupled);
        assert_eq!(Hello::decode(&enc[..40]).unwrap().audio_layout, 0);

        // 26 + 6 placeholders + client_caps 1 + max_shard_payload 2 + rate 4.
        // No HDR, no depth byte (16-bit is default and last).
        let rate_only = Hello {
            audio_rate_hz: 96_000,
            ..base.clone()
        };
        let enc = rate_only.encode();
        assert_eq!(enc.len(), 26 + 6 + 1 + 2 + 4);
        assert_eq!(&enc[26..28], &[0, 0], "name + launch length placeholders");
        assert_eq!(enc[28], 0, "video_caps placeholder");
        assert_eq!(enc[29], 2, "audio_channels placeholder = stereo");
        assert_eq!(
            &enc[30..32],
            &[0, 0],
            "video_codecs + preferred placeholders"
        );
        assert_eq!(enc[32], 0, "client_caps placeholder");
        assert_eq!(
            &enc[33..35],
            &0u16.to_le_bytes(),
            "max_shard_payload placeholder"
        );
        assert_eq!(&enc[35..39], &96_000u32.to_le_bytes());
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec, rate_only);
        assert_eq!(
            dec.client_caps, 0,
            "the forced placeholder reads as absence"
        );
        assert_eq!(dec.max_shard_payload, 0, "…and so does this one");
        assert_eq!(dec.audio_bits, BITS_16, "absent depth → the legacy 16");

        // Last field forces the rate out as 48 000, not struct `0` — otherwise 16-bit and
        // 24-bit at the default rate would disagree about where the depth byte lives.
        let bits_only = Hello {
            audio_bits: BITS_24,
            ..base.clone()
        };
        let benc = bits_only.encode();
        assert_eq!(benc.len(), 26 + 6 + 1 + 2 + 4 + 1);
        assert_eq!(&benc[35..39], &SAMPLE_RATE_HZ.to_le_bytes(), "forced rate");
        assert_eq!(benc[39], BITS_24);
        assert_eq!(Hello::decode(&benc).unwrap(), bits_only);

        // Capable and opted-in, both parameters set.
        let req = Hello {
            client_caps: CLIENT_CAP_AUDIO_HIRES,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            ..base.clone()
        };
        let renc = req.encode();
        assert_eq!(Hello::decode(&renc).unwrap(), req);
        assert_eq!(renc[32], CLIENT_CAP_AUDIO_HIRES);
        // 8-byte tail < HDR_META_BODY_LEN, so remaining-length says no HDR.
        assert_eq!(Hello::decode(&renc).unwrap().display_hdr, None);

        // Post-HDR tail must stay under HDR_META_BODY_LEN (8 spent, 19 free) or a
        // Hello without HDR is read as one with.
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        let full = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_AUDIO_HIRES | CLIENT_CAP_CURSOR,
            max_shard_payload: 8908,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            ..base.clone()
        };
        let fenc = full.encode();
        let post_hdr = fenc.len() - (26 + 6 + HDR_META_BODY_LEN);
        assert_eq!(
            post_hdr, 8,
            "client_caps 1 + max_shard_payload 2 + audio_rate_hz 4 + audio_bits 1"
        );
        assert!(
            post_hdr < HDR_META_BODY_LEN,
            "the post-display_hdr tail must stay under {HDR_META_BODY_LEN} bytes — \
             at or past it, a Hello WITHOUT an HDR block is read as one WITH"
        );
        assert_eq!(Hello::decode(&fenc).unwrap(), full);

        // Omit or truncate before the audio fields → 48 kHz / 16-bit.
        assert_eq!(
            Hello::decode(&base.encode()).unwrap().audio_rate_hz,
            SAMPLE_RATE_HZ
        );
        assert_eq!(Hello::decode(&base.encode()).unwrap().audio_bits, BITS_16);
        let pre_audio = Hello::decode(&renc[..35]).unwrap();
        assert_eq!(pre_audio.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(pre_audio.audio_bits, BITS_16);
        assert_eq!(pre_audio.max_shard_payload, 0);
        // Torn rate is never half a rate.
        assert_eq!(
            Hello::decode(&renc[..37]).unwrap().audio_rate_hz,
            SAMPLE_RATE_HZ
        );
        // Unsupported depth → 16. A request is only a request.
        let mut bad_depth = renc.clone();
        bad_depth[39] = 32;
        assert_eq!(Hello::decode(&bad_depth).unwrap().audio_bits, BITS_16);
        assert!(depth_is_supported(Hello::decode(&renc).unwrap().audio_bits));
    }

    #[test]
    fn welcome_ext_block_wire_and_back_compat() {
        // The gate bits are each their own; a client that sets neither sees today's bytes.
        assert_eq!(CLIENT_CAP_EXT.count_ones(), 1);
        assert_eq!(
            CLIENT_CAP_EXT
                & (CLIENT_CAP_CURSOR
                    | CLIENT_CAP_PHASE_LOCK
                    | CLIENT_CAP_AUDIO_RED
                    | CLIENT_CAP_PAD_AUDIO
                    | CLIENT_CAP_AUDIO_HIRES
                    | CLIENT_CAP_KEEP_HOST_AUDIO),
            0
        );
        assert_eq!(HOST_CAP2_EXT & (HOST_CAP2_REPEAT_MARK | HOST_CAP2_TOUCH), 0);

        let base = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 50_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: 0,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_layout: 0,
            audio_frame_us: 0,
            host_caps2: 0,
        };

        // No CLIENT_CAP_EXT: the host calls plain encode and the wire is the 68-byte Welcome.
        let legacy = base.encode();
        assert_eq!(legacy.len(), 68);
        assert!(Welcome::decode_ext(&legacy).unwrap().is_empty());
        // A positional tail is not a block: mgmt_port alone still decodes to no entries.
        let mgmt = Welcome {
            mgmt_port: 47991,
            ..base
        };
        assert!(Welcome::decode_ext(&mgmt.encode()).unwrap().is_empty());

        // With the bit: the same 68 bytes, the forced placeholders, then ext_len + one entry.
        let enc = base.encode_ext(&[(EXT_TAG_PADDING, &[][..])]).unwrap();
        assert_eq!(&enc[..68], &legacy[..]);
        assert_eq!(enc[68], CIPHER_AES_128_GCM, "placeholder, not a block byte");
        assert_eq!(enc.len(), 89 + 2 + 4);
        assert_eq!(&enc[89..91], &4u16.to_le_bytes());
        assert_eq!(
            Welcome::decode(&enc).unwrap(),
            base,
            "every placeholder decodes to what its absence meant"
        );
        assert_eq!(
            Welcome::decode_ext(&enc).unwrap(),
            vec![(EXT_TAG_PADDING, &[][..])]
        );

        // An unknown tag is carried, not fatal: the rest of the block and every positional
        // field still decode. This is what lets a newer host talk to an older client.
        let enc = base
            .encode_ext(&[(0xBEEF, &[1, 2, 3][..]), (EXT_TAG_PADDING, &[][..])])
            .unwrap();
        assert_eq!(Welcome::decode(&enc).unwrap(), base);
        assert_eq!(
            Welcome::decode_ext(&enc).unwrap(),
            vec![(0xBEEF, &[1, 2, 3][..]), (EXT_TAG_PADDING, &[][..])]
        );

        // The ChaCha key shifts the block 32 bytes, like every other tail field.
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some([3u8; 32]),
            ..base
        };
        let cenc = cha.encode_ext(&[(EXT_TAG_PADDING, &[][..])]).unwrap();
        assert_eq!(cenc.len(), 121 + 2 + 4);
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);
        assert_eq!(
            Welcome::decode_ext(&cenc).unwrap(),
            vec![(EXT_TAG_PADDING, &[][..])]
        );

        // Client side: the block rides Start, after its fixed 6 bytes.
        let start = Start {
            client_udp_port: 41000,
        };
        let senc = start.encode_ext(&[(EXT_TAG_PADDING, &[][..])]).unwrap();
        assert_eq!(&senc[..6], &start.encode()[..]);
        assert_eq!(Start::decode(&senc).unwrap(), start);
        assert_eq!(
            Start::decode_ext(&senc).unwrap(),
            vec![(EXT_TAG_PADDING, &[][..])]
        );
        assert!(Start::decode_ext(&start.encode()).unwrap().is_empty());
    }

    #[test]
    fn ext_block_rejects_a_duplicate_tag_a_truncation_and_a_flood() {
        let good = encode_ext_block(&[(EXT_TAG_PADDING, &[7][..])]).unwrap();
        assert_eq!(good, vec![5, 0, 1, 0, 1, 0, 7]);
        assert_eq!(
            decode_ext_block(&good).unwrap(),
            vec![(EXT_TAG_PADDING, &[7][..])]
        );

        // A tag appears once. Fail closed on both sides, the stance cipher >= 2 takes.
        assert!(
            encode_ext_block(&[(EXT_TAG_PADDING, &[][..]), (EXT_TAG_PADDING, &[][..])]).is_err()
        );
        assert!(decode_ext_block(&[8, 0, 1, 0, 0, 0, 1, 0, 0, 0]).is_err());

        // Every truncation of a good block, plus a value length reaching past the block.
        for cut in 0..good.len() {
            assert!(decode_ext_block(&good[..cut]).is_err(), "cut {cut}");
        }
        assert!(decode_ext_block(&[4, 0, 1, 0, 9, 0]).is_err());

        // Caps: neither a made-up ext_len nor a flood of entries allocates.
        assert!(decode_ext_block(&[0xff, 0xff]).is_err());
        assert!(encode_ext_block(&[(EXT_TAG_PADDING, &[0u8; EXT_MAX_BYTES][..])]).is_err());
        let flood: Vec<(u16, &[u8])> = (0..=EXT_MAX_ENTRIES as u16).map(|t| (t, &[][..])).collect();
        assert!(encode_ext_block(&flood).is_err());
    }

    /// New client → old host: a host that does not parse the block is told
    /// nothing, so its acks stay nine bytes and its behaviour stays today's.
    #[test]
    fn an_old_host_is_told_nothing_and_answers_as_it_always_did() {
        let abr = [EXT_ABR_ACK_REASON];
        assert!(start_ext(0, "android 0.38.0", &abr, &[]).is_empty());
        assert!(start_ext(HOST_CAP2_REPEAT_MARK | HOST_CAP2_TOUCH, "x", &abr, &[]).is_empty());
        // A host that does parse it hears both, the log label first.
        let ext = start_ext(HOST_CAP2_EXT, "android 0.38.0", &abr, &[]);
        assert_eq!(ext.len(), 2);
        assert_eq!(ext[0].0, EXT_TAG_CLIENT);
        assert_eq!(ext_abr_features(&ext), EXT_ABR_ACK_REASON);
        // No label is still a tag: the feature byte does not ride on a log line.
        let ext = start_ext(HOST_CAP2_EXT, "", &abr, &[]);
        assert_eq!(ext, vec![(EXT_TAG_ABR, &abr[..])]);
    }

    #[test]
    fn a_preset_rides_start_and_reads_back_bounded() {
        let preset = SessionPreset::new("3f9a0c11e2b4", "Docked").unwrap();
        let bytes = preset.encode();
        let abr = [EXT_ABR_ACK_REASON];
        let block = encode_ext_block(&start_ext(HOST_CAP2_EXT, "deck", &abr, &bytes)).unwrap();
        let got = decode_ext_block(&block).unwrap();
        assert_eq!(SessionPreset::from_ext(&got), Some(preset));
        // Nothing set, nothing sent; an old host hears none of it.
        assert_eq!(
            SessionPreset::from_ext(&start_ext(HOST_CAP2_EXT, "", &abr, &[])),
            None
        );
        assert!(start_ext(0, "", &abr, &bytes).is_empty());
        // A hostile value is bounded, never a failed handshake.
        let long = SessionPreset::new(&"a".repeat(99), &"\u{7}n".repeat(99)).unwrap();
        assert_eq!(long.id.len(), PRESET_ID_MAX);
        assert!(long.name.len() <= PRESET_NAME_MAX && !long.name.contains('\u{7}'));
        assert_eq!(
            SessionPreset::from_ext(&[(EXT_TAG_PRESET, &[9, b'x'][..])]),
            None
        );
        assert_eq!(SessionPreset::from_ext(&[(EXT_TAG_PRESET, &[][..])]), None);
        assert_eq!(SessionPreset::new(" ", "Docked"), None);
    }

    /// New client → new host: the tag is one byte of features, and a bit this
    /// host does not know is a bit it ignores — bit 0 is still served.
    #[test]
    fn an_abr_tag_with_unknown_bits_still_asks_for_the_ack_reason() {
        let one = [EXT_ABR_ACK_REASON];
        let block = encode_ext_block(&[(EXT_TAG_ABR, &one[..])]).unwrap();
        let got = decode_ext_block(&block).unwrap();
        assert_eq!(ext_abr_features(&got) & EXT_ABR_ACK_REASON, 1);
        // A client from a later package setting bits this build never heard of.
        let future = [EXT_ABR_ACK_REASON | 0xF0];
        let block = encode_ext_block(&[(EXT_TAG_ABR, &future[..])]).unwrap();
        let got = decode_ext_block(&block).unwrap();
        assert_eq!(ext_abr_features(&got) & EXT_ABR_ACK_REASON, 1);
        // Absent, empty, or zero: a client that reads no ABR feature at all.
        assert_eq!(ext_abr_features(&[]), 0);
        assert_eq!(ext_abr_features(&[(EXT_TAG_ABR, &[][..])]), 0);
        assert_eq!(ext_abr_features(&[(EXT_TAG_ABR, &[0][..])]), 0);
        assert_eq!(ext_abr_features(&[(EXT_TAG_CLIENT, &[1][..])]), 0);
    }

    #[test]
    fn client_label_is_bounded_and_rides_start() {
        // A ≤ 96-byte label survives whole; the tag is skipped by a peer that does not know it.
        let label = client_label("  android 0.38.0 console/library\n ");
        assert_eq!(label, "android 0.38.0 console/library");
        let enc = Start {
            client_udp_port: 4770,
        }
        .encode_ext(&[(EXT_TAG_CLIENT, label.as_bytes())])
        .unwrap();
        assert_eq!(Start::decode(&enc).unwrap().client_udp_port, 4770);
        assert_eq!(
            Start::decode_ext(&enc).unwrap(),
            vec![(EXT_TAG_CLIENT, label.as_bytes())]
        );
        // A multi-byte tail truncates on a char boundary, never mid-code-point.
        let long = client_label(&"é".repeat(80));
        assert!(long.len() <= EXT_CLIENT_MAX);
        assert_eq!(long.chars().count(), EXT_CLIENT_MAX / 2);
    }
}
