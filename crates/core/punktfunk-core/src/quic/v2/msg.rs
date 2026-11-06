//! The post-handshake messages on the v2 wire: the same structs as `punktfunk/1`, each a frame
//! of tagged fields ([`V2Message`]).
//!
//! A field the sender leaves out decodes to its type's zero, so a message gains a field by
//! taking a new tag and an older reader skips it. Each message keeps `punktfunk/1`'s bounds on
//! what it holds (name and reason lengths, cursor size, clipboard kinds), checked on decode.
//! Tags are per message and, like every number on this wire, never reused.

use super::field::*;
use super::registry as reg;
use crate::config::Mode;
use crate::error::{PunktfunkError, Result};
use crate::quic::*;
/// The buffer [`ProfileUnlock::secret`] lives in, for callers that build one.
pub use zeroize::Zeroizing;

/// One v2 control or pairing message.
pub trait V2Message: Sized {
    const TYPE: u64;
    fn fields(&self) -> Fields;
    fn from_body(body: &[u8]) -> Result<Self>;

    /// The whole frame: `type ‖ len ‖ fields`.
    fn encode_v2(&self) -> Vec<u8> {
        self.fields().frame(Self::TYPE)
    }
}

/// How one struct field crosses the v2 wire. Absent decodes to `Default`.
pub trait FieldValue: Sized + Default {
    fn put(&self, f: Fields, tag: u64) -> Fields;
    fn get(v: &[u8]) -> Result<Self>;
}

impl FieldValue for u8 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u8(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u8_of(v)
    }
}

impl FieldValue for u16 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u16(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u16_of(v)
    }
}

impl FieldValue for u32 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u32(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u32_of(v)
    }
}

impl FieldValue for u64 {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u64(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        u64_of(v)
    }
}

impl FieldValue for bool {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bool(tag, *self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        bool_of(v)
    }
}

impl FieldValue for String {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.str(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        str_of(v, v.len()).map(str::to_string)
    }
}

impl FieldValue for Vec<u8> {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        Ok(v.to_vec())
    }
}

impl FieldValue for [u8; 32] {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        v.try_into()
            .map_err(|_| PunktfunkError::InvalidArg("32-byte field of another length"))
    }
}

/// `width u32 ‖ height u32 ‖ refresh_hz u32` as one 12-byte value.
impl FieldValue for Mode {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        let mut b = [0u8; 12];
        b[..4].copy_from_slice(&self.width.to_le_bytes());
        b[4..8].copy_from_slice(&self.height.to_le_bytes());
        b[8..].copy_from_slice(&self.refresh_hz.to_le_bytes());
        f.bytes(tag, &b)
    }
    fn get(v: &[u8]) -> Result<Self> {
        let b: &[u8; 12] = v
            .try_into()
            .map_err(|_| PunktfunkError::InvalidArg("mode field of another length"))?;
        let at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Mode {
            width: at(0),
            height: at(4),
            refresh_hz: at(8),
        })
    }
}

impl FieldValue for LaunchOutcomeKind {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.u8(tag, *self as u8)
    }
    fn get(v: &[u8]) -> Result<Self> {
        Ok(LaunchOutcomeKind::from_u8(u8_of(v)?))
    }
}

/// Name-value pairs as one value: a nested `{1 => name, 2 => value}` field per pair.
impl FieldValue for Vec<(String, String)> {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        let pairs = self.iter().fold(Fields::new(), |all, (name, value)| {
            all.fields(1, Fields::new().str(1, name).str(2, value))
        });
        f.fields(tag, pairs)
    }
    fn get(v: &[u8]) -> Result<Self> {
        let mut out = Vec::new();
        let mut pairs = FieldReader::new(v);
        while let Some((_, pair)) = pairs.next_field()? {
            let (mut name, mut value) = (String::new(), String::new());
            let mut r = FieldReader::new(pair);
            while let Some((tag, b)) = r.next_field()? {
                match tag {
                    1 => name = String::get(b)?,
                    2 => value = String::get(b)?,
                    _ => {}
                }
            }
            out.push((name, value));
        }
        Ok(out)
    }
}

/// A [`V2Message`] for a plain struct: `tag => field` per member, then an optional check of
/// the decoded value. Every member's type is a [`FieldValue`].
macro_rules! v2_message {
    ($ty:ident = $id:expr, { $($tag:literal => $field:ident),* $(,)? } $(, check $check:expr)?) => {
        impl V2Message for $ty {
            const TYPE: u64 = $id;

            fn fields(&self) -> Fields {
                let f = Fields::new();
                $( let f = FieldValue::put(&self.$field, f, $tag); )*
                f
            }

            fn from_body(body: &[u8]) -> Result<Self> {
                $( let mut $field = None; )*
                let mut r = FieldReader::new(body);
                while let Some((tag, v)) = r.next_field()? {
                    match tag {
                        $( $tag => set_once(&mut $field, FieldValue::get(v)?)?, )*
                        _ => {}
                    }
                }
                let m = $ty { $( $field: $field.unwrap_or_default(), )* };
                $(
                    let check: fn(&$ty) -> bool = $check;
                    if !check(&m) {
                        return Err(PunktfunkError::InvalidArg(concat!("bad ", stringify!($ty))));
                    }
                )?
                Ok(m)
            }
        }
    };
}

v2_message!(PairRequest = reg::MSG_PAIR_REQUEST, { 1 => name, 2 => spake_a, 3 => device_key },
    check |m| m.name.len() <= HELLO_NAME_MAX);
v2_message!(PairChallenge = reg::MSG_PAIR_CHALLENGE, { 1 => spake_b, 2 => confirm });
v2_message!(PairProof = reg::MSG_PAIR_PROOF, { 1 => confirm });
v2_message!(PairResult = reg::MSG_PAIR_RESULT, { 1 => ok });
v2_message!(AuthChallenge = reg::MSG_AUTH_CHALLENGE, { 1 => nonce });
v2_message!(AuthResponse = reg::MSG_AUTH_RESPONSE, { 1 => device_key, 2 => signature });
v2_message!(Refused = reg::MSG_REFUSED,
    { 1 => code, 2 => reason, 3 => reason_id, 4 => reason_params },
    check |m| m.bounded());

v2_message!(Reconfigure = reg::MSG_RECONFIGURE, { 1 => mode, 2 => exclude_codecs });
v2_message!(Reconfigured = reg::MSG_RECONFIGURED,
    { 1 => accepted, 2 => mode, 3 => exclude_codecs });
v2_message!(SetBitrate = reg::MSG_SET_BITRATE, { 1 => bitrate_kbps });
v2_message!(PipelineGap = reg::MSG_PIPELINE_GAP, { 1 => gap_ms });
v2_message!(ProbeShaped = reg::MSG_PROBE_REQUEST,
    { 1 => target_kbps, 2 => duration_ms, 3 => burst_hz, 4 => group_bytes, 5 => group_rate_kbps });
v2_message!(ProbeResult = reg::MSG_PROBE_RESULT,
    { 1 => bytes_sent, 2 => packets_sent, 3 => duration_ms, 4 => wire_packets_sent,
      5 => send_dropped });
v2_message!(CursorShape = reg::MSG_CURSOR_SHAPE,
    { 1 => serial, 2 => w, 3 => h, 4 => hot_x, 5 => hot_y, 6 => rgba },
    check |m| m.w > 0 && m.h > 0 && m.w <= CURSOR_SHAPE_MAX_SIDE && m.h <= CURSOR_SHAPE_MAX_SIDE
        && m.rgba.len() == m.w as usize * m.h as usize * 4);
v2_message!(CursorRenderMode = reg::MSG_CURSOR_RENDER, { 1 => client_draws });
v2_message!(AccessUpdate = reg::MSG_ACCESS_UPDATE, { 1 => grants, 2 => remaining_secs });
v2_message!(AudioState = reg::MSG_AUDIO_STATE, { 1 => muted });
v2_message!(LaunchOutcome = reg::MSG_LAUNCH_OUTCOME,
    { 1 => kind, 2 => message, 3 => message_id, 4 => message_params },
    check |m| m.bounded());
v2_message!(PadSlots = reg::MSG_PAD_SLOTS, { 1 => slots });
v2_message!(ShardPayloadChanged = reg::MSG_SHARD_PAYLOAD_CHANGED, { 1 => shard_payload });
v2_message!(ShardPayloadAck = reg::MSG_SHARD_PAYLOAD_ACK, { 1 => shard_payload });
v2_message!(ClockProbe = reg::MSG_CLOCK_PROBE, { 1 => t1_ns });
v2_message!(ClockEcho = reg::MSG_CLOCK_ECHO, { 1 => t1_ns, 2 => t2_ns, 3 => t3_ns });
v2_message!(PadIdentity = reg::MSG_PAD_IDENTITY,
    { 1 => pad, 2 => serial, 3 => replies, 4 => slot },
    check |m| m.slot < 4
        && m.serial.len() <= PAD_IDENTITY_SERIAL_MAX
        && m.replies.len() <= PAD_IDENTITY_REPLIES_MAX
        && unpack_identity_replies(&m.replies).is_some());
v2_message!(PadFeature = reg::MSG_PAD_FEATURE, { 1 => pad, 2 => data },
    check |m| !m.data.is_empty() && m.data.len() <= crate::quic::HID_REPORT_MAX);

/// `host → client`, before `ServerHello`, repeating: the host is still deciding, such as a
/// console approval of this device. `punktfunk/2` only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {}

impl V2Message for Pending {
    const TYPE: u64 = reg::MSG_PENDING;

    fn fields(&self) -> Fields {
        Fields::new()
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut r = FieldReader::new(body);
        while r.next_field()?.is_some() {}
        Ok(Pending {})
    }
}

/// Longest [`Redirect::addr`]: a host name (RFC 1035) or an IP literal.
pub const REDIRECT_ADDR_MAX: usize = 253;

/// Longest profile id on any wire or ABI: `char id[65]` on the C side.
pub const PROFILE_ID_MAX: usize = 64;

/// [`Redirect::pin`]: a SHA-256 fingerprint in hex.
pub const REDIRECT_PIN_LEN: usize = 64;

/// `host → client`, instead of `ServerHello`: the session runs on another host of this box.
/// The client dials `addr:port` with the same `ClientHello`, once; an empty `addr` is the
/// address it dialed. `pin` is that host's certificate fingerprint, which the box vouches for
/// over the connection the client already pinned; empty keeps the pin it dialed with.
/// `profile` is the id the host resolved, `seat_no` and `seat_name` the seat, and `occupant`
/// the device already there when this one joins a live session. To a browser, `port` is the
/// seat's browser plane and `cert_hash` its certificate's SHA-256, the hash the browser pins and
/// binds its credential to; empty to a native client. `punktfunk/2` only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Redirect {
    pub addr: String,
    pub port: u16,
    pub profile: String,
    pub seat_no: u8,
    pub seat_name: String,
    pub occupant: String,
    pub pin: String,
    pub cert_hash: String,
}

/// An empty value, or one SHA-256 in hex: [`Redirect::pin`] and [`Redirect::cert_hash`].
fn hex_or_empty(v: &str) -> bool {
    v.is_empty() || (v.len() == REDIRECT_PIN_LEN && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

v2_message!(Redirect = reg::MSG_REDIRECT,
    { 1 => addr, 2 => port, 3 => profile, 4 => seat_no, 5 => seat_name, 6 => occupant,
      7 => pin, 8 => cert_hash },
    check |m| m.port != 0 && m.addr.len() <= REDIRECT_ADDR_MAX
        && m.profile.len() <= PROFILE_ID_MAX && m.seat_name.len() <= HELLO_NAME_MAX
        && m.occupant.len() <= HELLO_NAME_MAX
        && hex_or_empty(&m.pin) && hex_or_empty(&m.cert_hash));

/// [`ProfileUnlock::kind`]: the account password as typed.
pub const UNLOCK_PASSWORD: u8 = 1;
/// [`ProfileUnlock::kind`]: a signature over [`unlock_signed_message`] from the device's unlock key.
pub const UNLOCK_KEY_PROOF: u8 = 2;
/// [`ProfileUnlock::enroll_alg`]: no key to enrol.
pub const ENROLL_NONE: u8 = 0;
/// [`ProfileUnlock::enroll_alg`]: ECDSA P-256 with SHA-256; the key is the 65-byte SEC1 point.
pub const ENROLL_P256: u8 = 1;
/// [`ProfileUnlock::enroll_alg`]: RSA 2048 PKCS#1 v1.5 with SHA-256; the key is a DER
/// `RSAPublicKey`.
pub const ENROLL_RSA2048: u8 = 2;
/// Longest password, in bytes.
pub const UNLOCK_PASSWORD_MAX: usize = 512;
/// Longest key-proof signature, in bytes.
pub const UNLOCK_SIGNATURE_MAX: usize = 1024;
/// Length of an [`ENROLL_P256`] key: `04 ‖ X ‖ Y`.
pub const ENROLL_P256_KEY_LEN: usize = 65;
/// Longest [`ENROLL_RSA2048`] key.
pub const ENROLL_RSA_KEY_MAX: usize = 600;

/// `client → host`, right after `ClientHello` on the first dial: what opens a profile on a
/// person's own account. A password, with the choice to stay signed in and optionally a key to
/// enrol, or a signature from an enrolled key. `secret` wipes on drop and `Debug` prints
/// lengths. [`Self::new`] and the decoder refuse one that fails [`Self::is_valid`].
pub struct ProfileUnlock {
    /// [`UNLOCK_PASSWORD`] or [`UNLOCK_KEY_PROOF`].
    pub kind: u8,
    /// The password as typed (UTF-8, never trimmed), or the signature.
    pub secret: Zeroizing<Vec<u8>>,
    /// Password only: remember this device for the profile.
    pub stay_signed_in: bool,
    /// Password only: `ENROLL_*` of `enroll_key`.
    pub enroll_alg: u8,
    /// Password only: the public key to enrol, empty with [`ENROLL_NONE`].
    pub enroll_key: Vec<u8>,
}

impl ProfileUnlock {
    /// An unlock, or `InvalidArg` when it breaks a bound of the wire. `secret` wipes either way.
    pub fn new(
        kind: u8,
        secret: Zeroizing<Vec<u8>>,
        stay_signed_in: bool,
        enroll_alg: u8,
        enroll_key: Vec<u8>,
    ) -> Result<Self> {
        let u = ProfileUnlock {
            kind,
            secret,
            stay_signed_in,
            enroll_alg,
            enroll_key,
        };
        if u.is_valid() {
            Ok(u)
        } else {
            Err(PunktfunkError::InvalidArg("bad ProfileUnlock"))
        }
    }

    /// Every bound the wire sets: the kind, the secret's length, a password's UTF-8 without
    /// NUL, and a key that matches its algorithm. A key proof carries nothing else.
    pub fn is_valid(&self) -> bool {
        let s = &self.secret;
        match self.kind {
            UNLOCK_PASSWORD => {
                (1..=UNLOCK_PASSWORD_MAX).contains(&s.len())
                    && !s.contains(&0)
                    && std::str::from_utf8(s).is_ok()
                    && match self.enroll_alg {
                        ENROLL_NONE => self.enroll_key.is_empty(),
                        ENROLL_P256 => {
                            self.enroll_key.len() == ENROLL_P256_KEY_LEN
                                && self.enroll_key[0] == 0x04
                        }
                        // A DER `RSAPublicKey` is a SEQUENCE.
                        ENROLL_RSA2048 => {
                            self.enroll_key.len() <= ENROLL_RSA_KEY_MAX
                                && self.enroll_key.first() == Some(&0x30)
                        }
                        _ => false,
                    }
            }
            UNLOCK_KEY_PROOF => {
                (1..=UNLOCK_SIGNATURE_MAX).contains(&s.len())
                    && !self.stay_signed_in
                    && self.enroll_alg == ENROLL_NONE
                    && self.enroll_key.is_empty()
            }
            _ => false,
        }
    }

    /// The whole frame, in a buffer that wipes on drop. Send this, never
    /// [`V2Message::encode_v2`], whose body copy is freed unwiped.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let body = Zeroizing::new(self.fields().into_body());
        Zeroizing::new(super::field::frame(Self::TYPE, &body))
    }
}

impl std::fmt::Debug for ProfileUnlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileUnlock")
            .field("kind", &self.kind)
            .field("secret_len", &self.secret.len())
            .field("stay_signed_in", &self.stay_signed_in)
            .field("enroll_alg", &self.enroll_alg)
            .field("enroll_key_len", &self.enroll_key.len())
            .finish()
    }
}

impl FieldValue for Zeroizing<Vec<u8>> {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        Ok(Zeroizing::new(v.to_vec()))
    }
}

// `secret` is written last: a growing body then never reallocates away a copy of it.
v2_message!(ProfileUnlock = reg::MSG_PROFILE_UNLOCK,
    { 1 => kind, 3 => stay_signed_in, 4 => enroll_alg, 5 => enroll_key, 2 => secret },
    check ProfileUnlock::is_valid);

/// Length of [`unlock_signed_message`].
pub const UNLOCK_SIGNED_LEN: usize = 152;
/// First bytes of [`unlock_signed_message`].
pub const UNLOCK_SIGNED_LABEL: &[u8; 24] = b"punktfunk-seat-unlock-v1";

/// What a profile's unlock key signs, with SHA-256 as the digest:
/// `"punktfunk-seat-unlock-v1" ‖ challenge ‖ seat id ‖ device fingerprint ‖ box fingerprint`.
/// `seat_id` is the `SeatId`'s 32 lowercase hex characters; the fingerprints are SHA-256 of the
/// device's certificate and of the box's as this client pinned it. `InvalidArg` for any other
/// seat id.
pub fn unlock_signed_message(
    challenge: &[u8; 32],
    seat_id: &str,
    device_fp: &[u8; 32],
    box_fp: &[u8; 32],
) -> Result<[u8; UNLOCK_SIGNED_LEN]> {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if seat_id.len() != 32 || !seat_id.bytes().all(hex) {
        return Err(PunktfunkError::InvalidArg(
            "seat id is not 32 lowercase hex",
        ));
    }
    let mut m = [0u8; UNLOCK_SIGNED_LEN];
    for (at, part) in [
        (0, &UNLOCK_SIGNED_LABEL[..]),
        (24, &challenge[..]),
        (56, seat_id.as_bytes()),
        (88, &device_fp[..]),
        (120, &box_fp[..]),
    ] {
        m[at..at + part.len()].copy_from_slice(part);
    }
    Ok(m)
}

v2_message!(ClipControl = reg::MSG_CLIP_CONTROL, { 1 => enabled, 2 => flags });
v2_message!(ClipState = reg::MSG_CLIP_STATE, { 1 => enabled, 2 => policy, 3 => reason });
v2_message!(ClipFetch = reg::MSG_CLIP_FETCH, { 1 => seq, 2 => file_index, 3 => mime },
    check |m| m.mime.len() <= CLIP_MAX_MIME);
v2_message!(ClipFetchHdr = reg::MSG_CLIP_FETCH_HDR, { 1 => status, 2 => total_size });

/// The rate in force (1) and, when short of the ask, why (2). A reason this build does not
/// know reads as none: the ack still stands.
impl V2Message for BitrateChanged {
    const TYPE: u64 = reg::MSG_BITRATE_CHANGED;

    fn fields(&self) -> Fields {
        Fields::new()
            .u32(1, self.bitrate_kbps)
            .when(self.reason.is_some(), |f| {
                f.u8(2, self.reason.map_or(0, AckReason::to_wire))
            })
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let (mut rate, mut reason) = (None, None);
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            match tag {
                1 => set_once(&mut rate, u32_of(v)?)?,
                2 => set_once(&mut reason, u8_of(v)?)?,
                _ => {}
            }
        }
        Ok(BitrateChanged {
            bitrate_kbps: rate.unwrap_or(0),
            reason: reason.and_then(AckReason::from_wire),
        })
    }
}

/// The offer's sequence (1) and one field per format (2, repeated): `mime ‖ size_hint u64`,
/// the mime's length implied by the field's.
impl V2Message for ClipOffer {
    const TYPE: u64 = reg::MSG_CLIP_OFFER;

    fn fields(&self) -> Fields {
        self.kinds
            .iter()
            .take(CLIP_MAX_KINDS)
            .fold(Fields::new().u32(1, self.seq), |f, k| {
                let mime = &k.mime.as_bytes()[..k.mime.len().min(CLIP_MAX_MIME)];
                let mut v = mime.to_vec();
                v.extend_from_slice(&k.size_hint.to_le_bytes());
                f.bytes(2, &v)
            })
    }

    fn from_body(body: &[u8]) -> Result<Self> {
        let mut seq = None;
        let mut kinds = Vec::new();
        let mut r = FieldReader::new(body);
        while let Some((tag, v)) = r.next_field()? {
            match tag {
                1 => set_once(&mut seq, u32_of(v)?)?,
                2 => {
                    if kinds.len() == CLIP_MAX_KINDS || v.len() < 8 || v.len() - 8 > CLIP_MAX_MIME {
                        return Err(PunktfunkError::InvalidArg("bad ClipOffer kind"));
                    }
                    let (mime, size) = v.split_at(v.len() - 8);
                    kinds.push(ClipKind {
                        mime: String::from_utf8_lossy(mime).into_owned(),
                        size_hint: u64::from_le_bytes(size.try_into().unwrap()),
                    });
                }
                _ => {}
            }
        }
        Ok(ClipOffer {
            seq: seq.unwrap_or(0),
            kinds,
        })
    }
}

/// `host → client`, at the session's start and before the first packet of epoch `epoch`:
/// what the video stream is from there on, and the host's end of the path. The receiver
/// holds frames of an epoch it has no config for. A host fact of `0` means its OS did not
/// say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamConfig {
    /// The video stream this config is for: 0 the first screen, then the one each further
    /// screen rides ([`ScreenLayout`]). Epochs count per stream.
    pub stream: u8,
    pub epoch: u8,
    pub mode: Mode,
    /// One `CODEC_*` bit.
    pub codec: u8,
    pub bit_depth: u8,
    /// CICP `[primaries, transfer, matrix, full_range]`.
    pub color: [u8; 4],
    pub chroma_format: u8,
    /// `IFACE_KIND_*` of the host's interface.
    pub host_iface_kind: u8,
    pub host_link_mbps: u32,
    /// The host data socket's granted send buffer.
    pub host_sndbuf_kb: u32,
    /// What the host's operator pinned with `PUNKTFUNK_DELIVERY`: `0` nothing, then auto,
    /// wake, burst, smooth, capped.
    pub host_forced_shape: u8,
}

v2_message!(StreamConfig = reg::MSG_STREAM_CONFIG,
    { 1 => epoch, 2 => mode, 3 => codec, 4 => bit_depth, 5 => color, 6 => chroma_format,
      7 => host_iface_kind, 8 => host_link_mbps, 9 => host_sndbuf_kb, 10 => host_forced_shape,
      11 => stream });

/// Screens one session carries at most, the first included.
pub const SCREENS_MAX: usize = 4;
/// Bytes of one [`Screen`] on the wire.
const SCREEN_LEN: usize = 20;

/// One screen of a session. In a `ClientHello` or [`Screens`] it is a monitor the client shows,
/// placed in the client's pixels beside screen 0; in a [`ScreenLayout`], a display the host
/// built, placed on the host's desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    /// 0 is the screen `Hello.mode` describes.
    pub index: u8,
    /// The video stream it rides ([`crate::packet::screen_stream`]); 0 in an ask.
    pub stream: u8,
    pub mode: Mode,
    /// Top-left of the screen.
    pub x: i32,
    pub y: i32,
    /// The client's primary monitor.
    pub primary: bool,
}

/// `index u8 ‖ stream u8 ‖ flags u8 ‖ 0 ‖ width u16 ‖ height u16 ‖ refresh u16 ‖ 0 u16 ‖ x i32 ‖
/// y i32` per screen, at most [`SCREENS_MAX`], as one value.
impl FieldValue for Vec<Screen> {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        let mut b = Vec::with_capacity(self.len() * SCREEN_LEN);
        for s in self.iter().take(SCREENS_MAX) {
            let dim = |v: u32| (v.min(u32::from(u16::MAX)) as u16).to_le_bytes();
            b.extend_from_slice(&[s.index, s.stream, u8::from(s.primary), 0]);
            b.extend_from_slice(&dim(s.mode.width));
            b.extend_from_slice(&dim(s.mode.height));
            b.extend_from_slice(&dim(s.mode.refresh_hz));
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(&s.x.to_le_bytes());
            b.extend_from_slice(&s.y.to_le_bytes());
        }
        f.bytes(tag, &b)
    }
    fn get(v: &[u8]) -> Result<Self> {
        if v.len() % SCREEN_LEN != 0 || v.len() / SCREEN_LEN > SCREENS_MAX {
            return Err(PunktfunkError::InvalidArg("bad screen list"));
        }
        Ok(v.chunks_exact(SCREEN_LEN)
            .map(|r| {
                let u16_at = |o: usize| u32::from(u16::from_le_bytes([r[o], r[o + 1]]));
                let i32_at = |o: usize| i32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
                Screen {
                    index: r[0],
                    stream: r[1],
                    primary: r[2] & 1 != 0,
                    mode: Mode {
                        width: u16_at(4),
                        height: u16_at(6),
                        refresh_hz: u16_at(8),
                    },
                    x: i32_at(12),
                    y: i32_at(16),
                }
            })
            .collect())
    }
}

/// `host → client`, before the first packet of any stream past the first and after every
/// change: each screen the host built, screen 0 included. A screen the host could not build
/// is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenLayout {
    pub screens: Vec<Screen>,
}

v2_message!(ScreenLayout = reg::MSG_SCREEN_LAYOUT, { 1 => screens });

/// `client → host`: every screen the client shows now, screen 0 first. Screen 0's mode still
/// moves by `Reconfigure`; this adds, drops and resizes the others.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screens {
    pub screens: Vec<Screen>,
}

v2_message!(Screens = reg::MSG_SCREENS, { 1 => screens });

impl StreamConfig {
    /// What a decoder for this epoch is built for.
    pub fn format(&self) -> VideoFormat {
        VideoFormat {
            codec: self.codec,
            bit_depth: self.bit_depth,
            color: ColorInfo::from_cicp(self.color),
            chroma_format: self.chroma_format,
        }
    }

    pub fn set_format(&mut self, f: VideoFormat) {
        self.codec = f.codec;
        self.bit_depth = f.bit_depth;
        self.color = f.color.cicp();
        self.chroma_format = f.chroma_format;
    }
}

/// `host → client`, before the first datagram of epoch `epoch` on audio stream `stream`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioConfig {
    pub stream: u8,
    pub epoch: u8,
    /// `AUDIO_CODEC_*`.
    pub codec: u8,
    pub rate_hz: u32,
    pub bits: u8,
    pub channels: u8,
    pub layout: u8,
    pub frame_us: u16,
    /// Each datagram carries the previous frame too.
    pub redundant: bool,
    /// The first audio `seq` under this config. The datagram carries no epoch, so this anchors it.
    pub first_seq: u32,
}

v2_message!(AudioConfig = reg::MSG_AUDIO_CONFIG,
    { 1 => stream, 2 => epoch, 3 => codec, 4 => rate_hz, 5 => bits, 6 => channels, 7 => layout,
      8 => frame_us, 9 => redundant, 10 => first_seq });

impl FieldValue for [u8; 4] {
    fn put(&self, f: Fields, tag: u64) -> Fields {
        f.bytes(tag, self)
    }
    fn get(v: &[u8]) -> Result<Self> {
        v.try_into()
            .map_err(|_| PunktfunkError::InvalidArg("4-byte field of another length"))
    }
}

/// One input edge for the input stream: the event's own encoding is the body.
pub fn encode_input_event(ev: &crate::input::InputEvent) -> Vec<u8> {
    super::field::frame(reg::MSG_INPUT_EVENT, &ev.encode())
}

/// The event an input-stream frame carries; `None` for another type or a bad event.
pub fn decode_input_event(ty: u64, body: &[u8]) -> Option<crate::input::InputEvent> {
    (ty == reg::MSG_INPUT_EVENT)
        .then(|| crate::input::InputEvent::decode(body))
        .flatten()
}

/// Decode a body of message type `M`, checking the frame type first.
pub fn decode<M: V2Message>(ty: u64, body: &[u8]) -> Result<M> {
    if ty != M::TYPE {
        return Err(PunktfunkError::InvalidArg("frame of another type"));
    }
    M::from_body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<M: V2Message + PartialEq + std::fmt::Debug>(m: M) {
        let wire = m.encode_v2();
        let (ty, body, n) = split_frame(&wire, reg::max_body).unwrap().unwrap();
        assert_eq!(n, wire.len());
        assert_eq!(decode::<M>(ty, body).unwrap(), m);
    }

    #[test]
    fn a_pad_identity_out_of_bounds_does_not_decode() {
        let long = PadIdentity {
            serial: "S".repeat(PAD_IDENTITY_SERIAL_MAX + 1),
            ..Default::default()
        };
        let torn = PadIdentity {
            replies: vec![4, 1, 2],
            ..Default::default()
        };
        for bad in [long, torn] {
            let wire = bad.encode_v2();
            let (ty, body, _) = split_frame(&wire, reg::max_body).unwrap().unwrap();
            assert!(decode::<PadIdentity>(ty, body).is_err());
        }
    }

    #[test]
    fn every_message_round_trips() {
        let mode = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        round_trip(PairRequest {
            name: "Living room TV".into(),
            spake_a: vec![1; 33],
            device_key: vec![2; 91],
        });
        round_trip(PairChallenge {
            spake_b: vec![3; 33],
            confirm: [4; 32],
        });
        round_trip(PairProof { confirm: [5; 32] });
        round_trip(PadIdentity {
            pad: 3,
            serial: "FXA0000000001".into(),
            replies: pack_identity_replies([(&[0x01, 0x83, 0x00][..], &[0x01, 0x83, 0x1E][..])]),
            slot: 2,
        });
        round_trip(PadFeature {
            pad: 1,
            data: vec![0x01, 0x87, 0x03, 0x08, 0x07, 0x00],
        });
        round_trip(PairResult { ok: true });
        round_trip(AuthChallenge { nonce: [6; 32] });
        round_trip(AuthResponse {
            device_key: vec![7; 91],
            signature: vec![8; 72],
        });
        round_trip(Refused {
            code: 0x67,
            reason: "update both".into(),
            ..Refused::default()
        });
        let worded = Refused {
            code: 0x68,
            reason: "x".repeat(REFUSED_REASON_MAX),
            ..Refused::default()
        }
        .worded(
            "close_monitor_missing",
            &[
                ("monitor", &"é".repeat(400)),
                ("monitors", &"é".repeat(400)),
                ("x", "y"),
            ],
        );
        assert_eq!(worded.reason_params.len(), REFUSED_PARAMS_MAX);
        assert!(worded.encode_v2().len() <= 1024);
        round_trip(worded);
        round_trip(Redirect {
            addr: "couch-pc.local".into(),
            port: 9778,
            profile: "9a3f1c2b7e40".into(),
            seat_no: 1,
            seat_name: "Seat 1".into(),
            occupant: "Ben's Apple TV".into(),
            pin: "ab".repeat(32),
            cert_hash: "cd".repeat(32),
        });
        round_trip(Reconfigure {
            mode,
            exclude_codecs: 0,
        });
        round_trip(Reconfigure {
            mode,
            exclude_codecs: CODEC_HEVC | CODEC_AV1,
        });
        round_trip(Reconfigured {
            accepted: true,
            mode,
            exclude_codecs: 0,
        });
        round_trip(Reconfigured {
            accepted: false,
            mode,
            exclude_codecs: CODEC_HEVC,
        });
        round_trip(SetBitrate {
            bitrate_kbps: 80_000,
        });
        for reason in [None, Some(AckReason::Cadence), Some(AckReason::Pinned)] {
            round_trip(BitrateChanged {
                bitrate_kbps: 50_000,
                reason,
            });
        }
        round_trip(PipelineGap { gap_ms: 401 });
        round_trip(ProbeShaped {
            target_kbps: 100_000,
            duration_ms: 500,
            burst_hz: 60,
            group_bytes: 65_536,
            group_rate_kbps: 0,
        });
        round_trip(ProbeResult {
            bytes_sent: 1 << 33,
            packets_sent: 9,
            duration_ms: 500,
            wire_packets_sent: 9,
            send_dropped: 0,
        });
        round_trip(CursorShape {
            serial: 3,
            w: 2,
            h: 3,
            hot_x: 1,
            hot_y: 1,
            rgba: vec![0xAB; 24],
        });
        round_trip(CursorRenderMode { client_draws: true });
        round_trip(AccessUpdate {
            grants: GRANT_ALL,
            remaining_secs: 60,
        });
        round_trip(AudioState { muted: true });
        round_trip(LaunchOutcome::new(
            LaunchOutcomeKind::Failed,
            "Gone in a second.",
        ));
        round_trip(PadSlots { slots: 0b101 });
        round_trip(ClipControl {
            enabled: true,
            flags: CLIP_FLAG_FILES,
        });
        round_trip(ClipState {
            enabled: true,
            policy: CLIP_POLICY_TEXT,
            reason: CLIP_REASON_OK,
        });
        round_trip(ClipOffer {
            seq: 7,
            kinds: vec![
                ClipKind {
                    mime: "text/plain;charset=utf-8".into(),
                    size_hint: 12,
                },
                ClipKind {
                    mime: String::new(),
                    size_hint: 0,
                },
            ],
        });
        round_trip(ClipFetch {
            seq: 7,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "image/png".into(),
        });
        round_trip(ClipFetchHdr {
            status: CLIP_FETCH_OK,
            total_size: 1 << 40,
        });
        round_trip(StreamConfig {
            stream: 0,
            epoch: 4,
            mode,
            codec: CODEC_AV1,
            bit_depth: 10,
            color: [9, 16, 9, 0],
            chroma_format: CHROMA_IDC_420,
            host_iface_kind: IFACE_KIND_ETHERNET,
            host_link_mbps: 2_500,
            host_sndbuf_kb: 4_096,
            host_forced_shape: 2,
        });
        round_trip(StreamConfig {
            stream: 2,
            epoch: 1,
            mode,
            ..Default::default()
        });
        let screens = vec![
            Screen {
                index: 0,
                stream: 0,
                mode,
                x: 0,
                y: 0,
                primary: true,
            },
            Screen {
                index: 1,
                stream: 2,
                mode: Mode {
                    width: 2560,
                    height: 1440,
                    refresh_hz: 60,
                },
                x: -2560,
                y: -300,
                primary: false,
            },
        ];
        round_trip(ScreenLayout {
            screens: screens.clone(),
        });
        round_trip(Screens { screens });
        round_trip(AudioConfig {
            stream: 0,
            epoch: 1,
            codec: AUDIO_CODEC_PCM,
            rate_hz: 96_000,
            bits: 24,
            channels: 2,
            layout: 0,
            frame_us: 2000,
            redundant: false,
            first_seq: 7_000,
        });
    }

    /// The bytes on the wire are pinned: a change here is a wire change.
    #[test]
    fn golden_frames() {
        assert_eq!(
            SetBitrate {
                bitrate_kbps: 80_000
            }
            .encode_v2(),
            [0x24, 0x06, 0x01, 0x04, 0x80, 0x38, 0x01, 0x00]
        );
        assert_eq!(
            BitrateChanged {
                bitrate_kbps: 1,
                reason: Some(AckReason::Cadence)
            }
            .encode_v2(),
            [0x25, 0x09, 0x01, 0x04, 0x01, 0x00, 0x00, 0x00, 0x02, 0x01, 0x02]
        );
        assert_eq!(
            PairResult { ok: true }.encode_v2(),
            [0x13, 0x03, 0x01, 0x01, 0x01]
        );
        assert_eq!(
            Redirect {
                addr: String::new(),
                port: 9778,
                profile: "ab".into(),
                seat_no: 2,
                seat_name: String::new(),
                occupant: String::new(),
                pin: String::new(),
                cert_hash: String::new(),
            }
            .encode_v2(),
            [
                0x05, 0x15, 0x01, 0x00, 0x02, 0x02, 0x32, 0x26, 0x03, 0x02, 0x61, 0x62, 0x04, 0x01,
                0x02, 0x05, 0x00, 0x06, 0x00, 0x07, 0x00, 0x08, 0x00
            ]
        );
        assert_eq!(
            ScreenLayout {
                screens: vec![Screen {
                    index: 1,
                    stream: 2,
                    mode: Mode {
                        width: 1920,
                        height: 1080,
                        refresh_hz: 60,
                    },
                    x: 1920,
                    y: 0,
                    primary: false,
                }],
            }
            .encode_v2(),
            [
                0x28, 0x16, 0x01, 0x14, 0x01, 0x02, 0x00, 0x00, 0x80, 0x07, 0x38, 0x04, 0x3C, 0x00,
                0x00, 0x00, 0x80, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ]
        );
    }

    /// A screen list past [`SCREENS_MAX`] or with a torn record does not decode.
    #[test]
    fn a_bad_screen_list_does_not_decode() {
        let list = |n: usize| Fields::new().bytes(1, &vec![0u8; n]).into_body();
        let at_bound = Screens::from_body(&list(SCREEN_LEN * SCREENS_MAX)).unwrap();
        assert_eq!(at_bound.screens.len(), SCREENS_MAX);
        assert!(Screens::from_body(&list(SCREEN_LEN * (SCREENS_MAX + 1))).is_err());
        assert!(Screens::from_body(&list(SCREEN_LEN - 1)).is_err());
    }

    #[test]
    fn bounds_hold_on_decode() {
        let long = |n: usize| "x".repeat(n);
        let bad_name = PairRequest {
            name: long(HELLO_NAME_MAX + 1),
            spake_a: vec![],
            device_key: vec![],
        };
        assert!(PairRequest::from_body(&bad_name.fields().into_body()).is_err());
        let bad_reason = Refused {
            code: 1,
            reason: long(REFUSED_REASON_MAX + 1),
            ..Refused::default()
        };
        assert!(Refused::from_body(&bad_reason.fields().into_body()).is_err());
        let nowhere = Redirect {
            port: 0,
            ..Redirect::default()
        };
        assert!(Redirect::from_body(&nowhere.fields().into_body()).is_err());
        let far = Redirect {
            addr: long(REDIRECT_ADDR_MAX + 1),
            port: 9778,
            ..Redirect::default()
        };
        assert!(Redirect::from_body(&far.fields().into_body()).is_err());
        let nobody = Redirect {
            port: 9778,
            profile: long(PROFILE_ID_MAX + 1),
            ..Redirect::default()
        };
        assert!(Redirect::from_body(&nobody.fields().into_body()).is_err());
        for pin in ["ab".repeat(31), "zz".repeat(32), "ab".repeat(33)] {
            let unpinnable = Redirect {
                port: 9778,
                pin: pin.clone(),
                ..Redirect::default()
            };
            assert!(Redirect::from_body(&unpinnable.fields().into_body()).is_err());
            let unhashable = Redirect {
                port: 9778,
                cert_hash: pin,
                ..Redirect::default()
            };
            assert!(Redirect::from_body(&unhashable.fields().into_body()).is_err());
        }
        let bad_cursor = CursorShape {
            serial: 1,
            w: CURSOR_SHAPE_MAX_SIDE + 1,
            h: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0; (CURSOR_SHAPE_MAX_SIDE as usize + 1) * 4],
        };
        assert!(CursorShape::from_body(&bad_cursor.fields().into_body()).is_err());
        let short_rgba = CursorShape {
            rgba: vec![0; 3],
            w: 1,
            ..bad_cursor
        };
        assert!(CursorShape::from_body(&short_rgba.fields().into_body()).is_err());
        let many = ClipOffer {
            seq: 1,
            kinds: vec![],
        }
        .fields();
        let many = (0..=CLIP_MAX_KINDS).fold(many, |f, _| f.bytes(2, &[0; 8]));
        assert!(ClipOffer::from_body(&many.into_body()).is_err());
        assert!(decode::<SetBitrate>(reg::MSG_PIPELINE_GAP, &[]).is_err());
    }

    fn unlock(kind: u8, secret: &[u8], stay: bool, alg: u8, key: Vec<u8>) -> ProfileUnlock {
        ProfileUnlock {
            kind,
            secret: Zeroizing::new(secret.to_vec()),
            stay_signed_in: stay,
            enroll_alg: alg,
            enroll_key: key,
        }
    }

    fn decoded(u: &ProfileUnlock) -> Result<ProfileUnlock> {
        let wire = u.encode();
        let (ty, body, n) = split_frame(&wire, reg::max_body)?.expect("one whole frame");
        assert_eq!(n, wire.len());
        decode::<ProfileUnlock>(ty, body)
    }

    /// Both kinds cross the wire whole, a password untrimmed, and `Debug` names no secret byte.
    #[test]
    fn a_profile_unlock_round_trips_and_prints_lengths() {
        let mut p256 = vec![0x04];
        p256.extend_from_slice(&[7; 64]);
        for u in [
            unlock(UNLOCK_PASSWORD, b" pass word\t", true, ENROLL_P256, p256),
            unlock(
                UNLOCK_PASSWORD,
                "Grüße".as_bytes(),
                false,
                ENROLL_NONE,
                vec![],
            ),
            unlock(
                UNLOCK_PASSWORD,
                b"x",
                false,
                ENROLL_RSA2048,
                vec![0x30, 0x82],
            ),
            unlock(UNLOCK_KEY_PROOF, &[0x30; 72], false, ENROLL_NONE, vec![]),
        ] {
            let back = decoded(&u).unwrap();
            assert_eq!(back.kind, u.kind);
            assert_eq!(*back.secret, *u.secret);
            assert_eq!(back.stay_signed_in, u.stay_signed_in);
            assert_eq!(back.enroll_alg, u.enroll_alg);
            assert_eq!(back.enroll_key, u.enroll_key);
        }
        let shown = format!(
            "{:?}",
            unlock(UNLOCK_PASSWORD, b"sesame", false, ENROLL_NONE, vec![])
        );
        assert!(
            !shown.contains("sesame") && !shown.contains("115"),
            "{shown}"
        );
        assert!(shown.contains("secret_len: 6"), "{shown}");
    }

    /// Every kind, algorithm and length the contract does not name is refused, at each bound.
    #[test]
    fn a_profile_unlock_out_of_bounds_does_not_decode() {
        let p256 = |first: u8, len: usize| {
            let mut k = vec![first];
            k.resize(len, 7);
            k
        };
        let rsa = |len: usize| {
            let mut k = vec![0x30];
            k.resize(len, 1);
            k
        };
        let pw = |s: &[u8]| unlock(UNLOCK_PASSWORD, s, false, ENROLL_NONE, vec![]);
        let proof =
            |len: usize| unlock(UNLOCK_KEY_PROOF, &vec![1; len], false, ENROLL_NONE, vec![]);
        let enrol = |alg: u8, key: Vec<u8>| unlock(UNLOCK_PASSWORD, b"pw", false, alg, key);
        let fits = [
            pw(&[b'a'; UNLOCK_PASSWORD_MAX]),
            proof(1),
            proof(UNLOCK_SIGNATURE_MAX),
            enrol(ENROLL_P256, p256(0x04, ENROLL_P256_KEY_LEN)),
            enrol(ENROLL_RSA2048, rsa(ENROLL_RSA_KEY_MAX)),
        ];
        for u in &fits {
            assert!(decoded(u).is_ok(), "{u:?}");
        }
        let refused = [
            unlock(0, b"pw", false, ENROLL_NONE, vec![]),
            unlock(3, b"pw", false, ENROLL_NONE, vec![]),
            pw(b""),
            pw(&[b'a'; UNLOCK_PASSWORD_MAX + 1]),
            pw(&[0xff, 0xfe]),
            pw(b"p\0w"),
            proof(0),
            proof(UNLOCK_SIGNATURE_MAX + 1),
            unlock(UNLOCK_KEY_PROOF, b"sig", true, ENROLL_NONE, vec![]),
            unlock(UNLOCK_KEY_PROOF, b"sig", false, ENROLL_P256, p256(4, 65)),
            unlock(UNLOCK_KEY_PROOF, b"sig", false, ENROLL_NONE, vec![1]),
            enrol(ENROLL_NONE, vec![1]),
            enrol(3, rsa(10)),
            enrol(ENROLL_P256, p256(0x04, ENROLL_P256_KEY_LEN - 1)),
            enrol(ENROLL_P256, p256(0x04, ENROLL_P256_KEY_LEN + 1)),
            enrol(ENROLL_P256, p256(0x02, ENROLL_P256_KEY_LEN)),
            enrol(ENROLL_RSA2048, vec![]),
            enrol(ENROLL_RSA2048, rsa(ENROLL_RSA_KEY_MAX + 1)),
            enrol(ENROLL_RSA2048, vec![0x31; 270]),
        ];
        for u in &refused {
            assert!(decoded(u).is_err(), "{u:?}");
            let secret = Zeroizing::new(u.secret.to_vec());
            let key = u.enroll_key.clone();
            let made = ProfileUnlock::new(u.kind, secret, u.stay_signed_in, u.enroll_alg, key);
            assert!(made.is_err(), "{u:?}");
        }
        // A second secret, a field running past the body, and a body past the frame bound.
        let twice = pw(b"one").fields().bytes(2, b"two").into_body();
        assert!(ProfileUnlock::from_body(&twice).is_err());
        let mut torn = pw(b"pw").fields().into_body();
        torn.pop();
        assert!(ProfileUnlock::from_body(&torn).is_err());
        let huge = Fields::new()
            .bytes(9, &[0; 2048])
            .frame(reg::MSG_PROFILE_UNLOCK);
        assert!(split_frame(&huge, reg::max_body).is_err());
    }

    /// The contract's vector: SHA-256 of the 152 bytes.
    #[test]
    fn the_unlock_message_matches_the_contract_vector() {
        use sha2::Digest;
        let challenge: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
        let m = unlock_signed_message(
            &challenge,
            "0123456789abcdef0123456789abcdef",
            &[0xaa; 32],
            &[0xbb; 32],
        )
        .unwrap();
        assert_eq!(
            crate::fp::hex(&sha2::Sha256::digest(m).into()),
            "31de46821f3058729b14c3585ad13159085ecb0d18c87194a34b1bddc8ddc31a"
        );
        for bad in [
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(unlock_signed_message(&challenge, bad, &[0; 32], &[0; 32]).is_err());
        }
    }

    #[test]
    fn input_edges_carry_the_event_encoding() {
        let ev = crate::input::InputEvent {
            kind: crate::input::InputKind::KeyUp,
            _pad: [0; 3],
            code: 30,
            x: 0,
            y: 0,
            flags: 2,
        };
        let wire = encode_input_event(&ev);
        let (ty, body, _) = split_frame(&wire, reg::max_body).unwrap().unwrap();
        assert_eq!(decode_input_event(ty, body), Some(ev));
        assert_eq!(decode_input_event(reg::MSG_SET_BITRATE, body), None);
    }

    /// An unknown tag is skipped, an unknown ack reason reads as none, a repeat is refused.
    #[test]
    fn unknown_tags_skip_and_repeats_refuse() {
        let body = Fields::new()
            .u32(1, 9)
            .str(77, "from a newer peer")
            .u8(2, 200)
            .into_body();
        assert_eq!(
            BitrateChanged::from_body(&body).unwrap(),
            BitrateChanged {
                bitrate_kbps: 9,
                reason: None
            }
        );
        let twice = Fields::new().u32(1, 1).u32(1, 2).into_body();
        assert!(SetBitrate::from_body(&twice).is_err());
    }
}
