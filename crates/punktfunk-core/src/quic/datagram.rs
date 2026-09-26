//! QUIC datagram side planes, tagged by the first byte.
//!
//! Video rides UDP. Everything else here is a QUIC datagram: 0xC9 audio, 0xCA rumble,
//! 0xCB mic, 0xCC rich input, 0xCD HID output, 0xCE HDR metadata, 0xCF host timing,
//! 0xD0 cursor, 0xD1 pad audio, 0xD2 redundant audio, 0xD3 PCM. Input 0xC8 lives in
//! [`crate::input`].
//!
//! Unknown tags and short buffers decode to `None`. Length-tolerant tails (rumble,
//! host timing, cursor) take the prefix the peer knows. Evidence:
//! `design/trigger-rumble-plane.md`, `design/phase-locked-capture.md`.

pub const AUDIO_MAGIC: u8 = 0xC9;
pub const RUMBLE_MAGIC: u8 = 0xCA;
/// Client → host Opus. The host feeds a virtual PipeWire source so apps can record it.
pub const MIC_MAGIC: u8 = 0xCB;
pub const RICH_INPUT_MAGIC: u8 = 0xCC;
pub const HIDOUT_MAGIC: u8 = 0xCD;

/// One Opus frame, 5 ms — under any MTU.
pub fn encode_audio_datagram(seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + opus.len());
    b.push(AUDIO_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

pub fn decode_audio_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < 13 || b[0] != AUDIO_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[13..]))
}

/// Previous-frame copy on the successor datagram — a lost 0xC9 is reconstructed, not concealed.
///
/// `[0xD2][u32 seq LE][u64 pts_ns LE][u16 primary_len LE][primary][previous]`.
/// Previous seq is `seq - 1`; empty tail = no predecessor.
///
/// Not Opus LBRR: `RESTRICTED_LOWDELAY` is CELT-only at 5 ms, below SILK's 10 ms, so
/// `set_inband_fec` is a no-op. Mic uplink (VoIP, 10 ms) does use in-band FEC.
/// Recovery sits in the client's 15–90 ms de-jitter buffer; the copy is not extra delay.
///
/// Sent only when both peers advertised [`CLIENT_CAP_AUDIO_RED`](super::caps::CLIENT_CAP_AUDIO_RED)
/// / [`HOST_CAP_AUDIO_RED`](super::caps::HOST_CAP_AUDIO_RED). `0xD1` is pad audio; this tag is `0xD2`.
pub const AUDIO_RED_MAGIC: u8 = 0xD2;

pub const AUDIO_RED_HEADER: usize = 1 + 4 + 8 + 2;

pub fn encode_audio_red_datagram(seq: u32, pts_ns: u64, opus: &[u8], prev: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(AUDIO_RED_HEADER + opus.len() + prev.len());
    b.push(AUDIO_RED_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    // 5 ms Opus is tens of bytes (encoder buffer is 4 KiB). A silent u16 truncate
    // would desync the split, so drop the previous-frame copy instead of the primary.
    let primary_len = u16::try_from(opus.len()).unwrap_or(u16::MAX);
    b.extend_from_slice(&primary_len.to_le_bytes());
    b.extend_from_slice(opus);
    if opus.len() == primary_len as usize {
        b.extend_from_slice(prev);
    }
    b
}

/// `previous` is `None` on an empty tail. Over-long `primary_len` is `None`, not a panic.
/// Tuple matches [`decode_audio_datagram`] plus the copy so cbindgen stays uniform.
#[allow(clippy::type_complexity)]
pub fn decode_audio_red_datagram(b: &[u8]) -> Option<(u32, u64, &[u8], Option<&[u8]>)> {
    if b.len() < AUDIO_RED_HEADER || b[0] != AUDIO_RED_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    let primary_len = u16::from_le_bytes(b[13..15].try_into().unwrap()) as usize;
    let rest = &b[AUDIO_RED_HEADER..];
    if primary_len > rest.len() {
        return None; // split point is outside the datagram
    }
    let (primary, prev) = rest.split_at(primary_len);
    Some((seq, pts_ns, primary, (!prev.is_empty()).then_some(prev)))
}

/// Lossless PCM. Header matches [`AUDIO_MAGIC`] so
/// [`AudioGapTracker`](crate::audio::AudioGapTracker) and pts / A-V sync stay unchanged.
///
/// `[0xD3][u32 seq LE][u64 pts_ns LE][interleaved LE samples]`.
/// A session runs `0xC9`/`0xD2` **or** `0xD3`, never both, never mid-session:
/// the output device is open at a fixed rate/depth. Capture format change ends
/// the plane rather than switching tags.
///
/// One frame per datagram, sized by [`crate::audio::pcm::frame_us_for`] so it
/// fits the path MTU — never fragmented. No [`AUDIO_RED_MAGIC`]: it would
/// double the largest bitrate on the connection.
pub const AUDIO_PCM_MAGIC: u8 = 0xD3;

/// Same length as the [`AUDIO_MAGIC`] header, so seq/pts sit at the same offsets.
pub const AUDIO_PCM_HEADER: usize = crate::audio::pcm::PCM_HEADER_LEN;

pub fn encode_audio_pcm_datagram(seq: u32, pts_ns: u64, pcm: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(AUDIO_PCM_HEADER + pcm.len());
    b.push(AUDIO_PCM_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(pcm);
    b
}

pub fn decode_audio_pcm_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < AUDIO_PCM_HEADER || b[0] != AUDIO_PCM_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[AUDIO_PCM_HEADER..]))
}

/// Legacy rumble v1. Level-triggered — persists until superseded; the host re-sends as loss heal.
/// New hosts emit [`encode_rumble_datagram_v3`]; this remains the old-host wire
/// (`PUNKTFUNK_RUMBLE_ENVELOPE=0` drops to it).
pub fn encode_rumble_datagram(pad: u16, low: u16, high: u16) -> [u8; 7] {
    let mut b = [0u8; 7];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b
}

pub const RUMBLE_V1_LEN: usize = 7;
/// v1 body + `[u8 seq][u16 ttl_ms LE]`. Length-tolerant: an old client reads the first 7 bytes
/// and ignores the tail — no wire-version bump.
pub const RUMBLE_V2_LEN: usize = 10;
/// v2 + `[u16 left_trigger LE][u16 right_trigger LE]`. Same `>=` prefix as v2: a 14-byte
/// datagram satisfies v1, v2, and v3 readers.
pub const RUMBLE_V3_LEN: usize = 14;

/// Self-terminating rumble: the level is authorized for at most `ttl_ms`, so a dead host
/// silences itself. `seq` is per-pad wrapping (every send) vs
/// [`GamepadSnapshot::seq_newer`](crate::input::GamepadSnapshot::seq_newer) so a reordered
/// stale start cannot re-light after a stop. Renewals replace the deadline; they never stack.
/// Stop is still `low == high == 0` sent immediately — expiry is the safety net, not the stop.
pub fn encode_rumble_datagram_v2(pad: u16, low: u16, high: u16, seq: u8, ttl_ms: u16) -> [u8; 10] {
    let mut b = [0u8; RUMBLE_V2_LEN];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b[7] = seq;
    b[8..10].copy_from_slice(&ttl_ms.to_le_bytes());
    b
}

/// v2 envelope plus Xbox trigger motors (`0..=0xFFFF`, same as `low`/`high`).
///
/// The four levels share one `seq` and one `ttl_ms`. A second sequence space would
/// let a reordered datagram apply handles from *t* and triggers from *t−1*. Sharing
/// also puts the trigger motors on the same lease/renewal/seq-gate as the handles.
///
/// Only the Windows HID Xbox pad (`0x03` output report) sources non-zero triggers.
/// Classic XInput and evdev `FF_RUMBLE` pass `lt = rt = 0` — a v2 datagram plus four
/// zero bytes, which length tolerance already accepts.
pub fn encode_rumble_datagram_v3(
    pad: u16,
    low: u16,
    high: u16,
    seq: u8,
    ttl_ms: u16,
    lt: u16,
    rt: u16,
) -> [u8; RUMBLE_V3_LEN] {
    let mut b = [0u8; RUMBLE_V3_LEN];
    b[..RUMBLE_V2_LEN].copy_from_slice(&encode_rumble_datagram_v2(pad, low, high, seq, ttl_ms));
    b[10..12].copy_from_slice(&lt.to_le_bytes());
    b[12..14].copy_from_slice(&rt.to_le_bytes());
    b
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleEnvelope {
    /// Per-pad wrapping send counter — the reorder gate.
    pub seq: u8,
    /// How long, in ms, this envelope authorizes the stated level before the client must silence.
    pub ttl_ms: u16,
}

/// `envelope` is `None` on a 7-byte (legacy) datagram — no seq/ttl; the client
/// applies its own staleness policy.
///
/// `left_trigger`/`right_trigger` are plain fields, not `Option`. A v1/v2 datagram
/// decodes them as 0. `Option` would invite "absent → keep previous", which on a
/// level-triggered plane is stuck rumble: `0xCA` means *these levels now*.
/// (`envelope` is optional because its absence selects a different policy.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleUpdate {
    pub pad: u16,
    pub low: u16,
    pub high: u16,
    pub left_trigger: u16,
    pub right_trigger: u16,
    pub envelope: Option<RumbleEnvelope>,
}

/// Handle levels only; ignores v2/v3 tails. Clients that honor TTL use [`decode_rumble_envelope`].
pub fn decode_rumble_datagram(b: &[u8]) -> Option<(u16, u16, u16)> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    Some((u16at(1), u16at(3), u16at(5)))
}

/// Length-detects each tail: `>= v2` has seq/ttl, `>= v3` has trigger levels,
/// 7..v2 is legacy (`envelope: None`). A torn tail degrades to a level, not a drop.
/// Absent trigger bytes are 0, not "unchanged" — see [`RumbleUpdate`].
pub fn decode_rumble_envelope(b: &[u8]) -> Option<RumbleUpdate> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let envelope = (b.len() >= RUMBLE_V2_LEN).then(|| RumbleEnvelope {
        seq: b[7],
        ttl_ms: u16::from_le_bytes([b[8], b[9]]),
    });
    let triggers = b.len() >= RUMBLE_V3_LEN;
    Some(RumbleUpdate {
        pad: u16at(1),
        low: u16at(3),
        high: u16at(5),
        left_trigger: if triggers { u16at(10) } else { 0 },
        right_trigger: if triggers { u16at(12) } else { 0 },
        envelope,
    })
}

pub fn encode_mic_datagram(seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + opus.len());
    b.push(MIC_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

pub fn decode_mic_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < 13 || b[0] != MIC_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[13..]))
}

pub(super) const RICH_TOUCHPAD: u8 = 0x01;
pub(super) const RICH_MOTION: u8 = 0x02;
pub(super) const RICH_TOUCHPAD_EX: u8 = 0x03;
pub(super) const RICH_HID_REPORT: u8 = 0x04;
/// Stylus plane ([`PenBatch`](super::pen::PenBatch)), not a [`RichInput`] variant:
/// pens route to the tablet injector, pads to gamepad backends. [`RichInput::decode`]
/// returns `None` here so a pre-pen host drops it as unknown-kind. Listed so 0xCC
/// kind bytes stay unique.
pub(super) const RICH_PEN: u8 = 0x05;

/// Longest raw HID report on [`RichInput::HidReport`] / [`HidOutput::HidRaw`].
/// Valve interrupt/feature reports are 64 bytes.
pub const HID_REPORT_MAX: usize = 64;

/// Client→host controller input beyond the 18-byte [`InputEvent`](crate::input::InputEvent).
/// Wire: `[0xCC][kind][fields…]`. Unknown kind → `None` (dropped). Kind `0x05` is
/// [`PenBatch`](super::pen::PenBatch) with its own decoder — see [`RICH_PEN`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichInput {
    /// One contact. `x`/`y` are `0..=65535` SCREEN (origin top-left, +y down) — what
    /// SDL/Windows/Android produce; the host scales to DualSense resolution.
    /// `active = false` lifts.
    Touchpad {
        pad: u8,
        finger: u8,
        active: bool,
        x: u16,
        y: u16,
    },
    /// `gyro` (pitch/yaw/roll) + `accel`, sensor signed-16, passed through into the DualSense report.
    Motion {
        pad: u8,
        gyro: [i16; 3],
        accel: [i16; 3],
    },
    /// Trackpad contact with surface, click vs touch, and pressure.
    /// `surface`: 0 DualSense/single, 1 Steam left, 2 Steam right.
    /// `x`/`y` are signed (centred at 0), SCREEN (+x right, +y down). Device-raw
    /// quirks (Deck +y up) are the host applier's job — the wire is screen-y.
    /// `pressure` is 0 if the surface has no force sensor.
    TouchpadEx {
        pad: u8,
        surface: u8,
        finger: u8,
        touch: bool,
        click: bool,
        x: i16,
        y: i16,
        pressure: u16,
    },
    /// Raw HID input report, forwarded verbatim for as-is passthrough
    /// ([`GamepadPref::SteamController2`](crate::config::GamepadPref)).
    /// `data[..len]` is the interrupt/GATT payload, report-id first. Fixed-size
    /// body keeps the type `Copy` at the controller's report rate; a lost datagram
    /// heals on the next snapshot.
    HidReport {
        pad: u8,
        len: u8,
        data: [u8; HID_REPORT_MAX],
    },
}

impl RichInput {
    /// The pad this input addresses. Every variant carries a `u8`, so one pattern
    /// serves the read and [`Self::set_pad`].
    pub fn pad(&self) -> u8 {
        match self {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. }
            | RichInput::HidReport { pad, .. } => *pad,
        }
    }

    /// Re-address to the host's OS slot — the inverse of [`HidOutput::with_pad`].
    /// A client numbers its first pad 0 and `pf_inject::pad_pool` hands out slots in
    /// claim order, so the two spaces differ whenever a session's pads arrive out of
    /// wire order. Miss this and one pad's reports drive another pad's device.
    pub fn set_pad(&mut self, slot: u8) {
        match self {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. }
            | RichInput::HidReport { pad, .. } => *pad = slot,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![RICH_INPUT_MAGIC];
        match *self {
            RichInput::Touchpad {
                pad,
                finger,
                active,
                x,
                y,
            } => {
                out.extend_from_slice(&[RICH_TOUCHPAD, pad, finger, active as u8]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            RichInput::Motion { pad, gyro, accel } => {
                out.extend_from_slice(&[RICH_MOTION, pad]);
                for v in gyro.iter().chain(accel.iter()) {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            RichInput::TouchpadEx {
                pad,
                surface,
                finger,
                touch,
                click,
                x,
                y,
                pressure,
            } => {
                let state = (touch as u8) | ((click as u8) << 1);
                out.extend_from_slice(&[RICH_TOUCHPAD_EX, pad, surface, finger, state]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&pressure.to_le_bytes());
            }
            RichInput::HidReport { pad, len, ref data } => {
                let len = (len as usize).min(HID_REPORT_MAX);
                out.extend_from_slice(&[RICH_HID_REPORT, pad, len as u8]);
                out.extend_from_slice(&data[..len]);
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<RichInput> {
        if b.first() != Some(&RICH_INPUT_MAGIC) {
            return None;
        }
        match *b.get(1)? {
            RICH_TOUCHPAD if b.len() >= 9 => Some(RichInput::Touchpad {
                pad: b[2],
                finger: b[3],
                active: b[4] != 0,
                x: u16::from_le_bytes([b[5], b[6]]),
                y: u16::from_le_bytes([b[7], b[8]]),
            }),
            RICH_MOTION if b.len() >= 15 => {
                let i16at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
                Some(RichInput::Motion {
                    pad: b[2],
                    gyro: [i16at(3), i16at(5), i16at(7)],
                    accel: [i16at(9), i16at(11), i16at(13)],
                })
            }
            RICH_TOUCHPAD_EX if b.len() >= 12 => Some(RichInput::TouchpadEx {
                pad: b[2],
                surface: b[3],
                finger: b[4],
                touch: b[5] & 0x01 != 0,
                click: b[5] & 0x02 != 0,
                x: i16::from_le_bytes([b[6], b[7]]),
                y: i16::from_le_bytes([b[8], b[9]]),
                pressure: u16::from_le_bytes([b[10], b[11]]),
            }),
            RICH_HID_REPORT if b.len() >= 4 => {
                // `len` is clamped to the fixed body and to the buffer; a torn datagram
                // truncates, never over-reads.
                let len = (b[3] as usize).min(HID_REPORT_MAX).min(b.len() - 4);
                let mut data = [0u8; HID_REPORT_MAX];
                data[..len].copy_from_slice(&b[4..4 + len]);
                Some(RichInput::HidReport {
                    pad: b[2],
                    len: len as u8,
                    data,
                })
            }
            _ => None,
        }
    }
}

/// Longest [`HidOutput::Trigger`] `effect`: DualSense mode byte plus ten parameters.
/// Encode and decode both clamp here — the only variable-length HID-output variant.
pub const TRIGGER_EFFECT_MAX: usize = 11;

const HIDOUT_LED: u8 = 0x01;
const HIDOUT_PLAYER_LEDS: u8 = 0x02;
const HIDOUT_TRIGGER: u8 = 0x03;
const HIDOUT_TRACKPAD_HAPTIC: u8 = 0x04;
const HIDOUT_HID_RAW: u8 = 0x05;
const HIDOUT_AUDIO_CTL: u8 = 0x06;
const HIDOUT_MIC_LED: u8 = 0x07;

/// [`HidOutput::HidRaw`] `kind`: interrupt-OUT / GATT write (`write` / `SDL_hid_write`).
pub const HID_RAW_OUTPUT: u8 = 0;
/// [`HidOutput::HidRaw`] `kind`: SET_REPORT / GATT feature write (`SDL_hid_send_feature_report`).
pub const HID_RAW_FEATURE: u8 = 1;

/// DualSense feedback a game wrote to the host's virtual pad.
/// Wire: `[0xCD][kind][pad][fields…]`. Rumble stays on [`RUMBLE_MAGIC`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HidOutput {
    Led {
        pad: u8,
        r: u8,
        g: u8,
        b: u8,
    },
    /// Low 5 bits of the player-indicator LEDs.
    PlayerLeds {
        pad: u8,
        bits: u8,
    },
    /// Adaptive trigger. `which` 0 = L2, 1 = R2; `effect` is the DualSense parameter block.
    Trigger {
        pad: u8,
        which: u8,
        effect: Vec<u8>,
    },
    /// Steam Controller voice-coil pulse. `side` 0 = right, 1 = left; `period` is µs off-time.
    /// A client without coils drops it. The tag is allocated on a shipped wire — keep the
    /// decoder even while nothing produces or renders this variant.
    TrackpadHaptic {
        pad: u8,
        side: u8,
        amplitude: u16,
        period: u16,
        count: u16,
    },
    /// Raw report for as-is passthrough (reverse of [`RichInput::HidReport`]).
    /// `kind` is [`HID_RAW_OUTPUT`] or [`HID_RAW_FEATURE`]; `data` is the full report,
    /// id first, ≤ [`HID_REPORT_MAX`]. Triton rumble re-sends every ~40 ms against a
    /// ~50 ms hardware timeout; settings refresh every ~3 s — a lost datagram heals.
    HidRaw {
        pad: u8,
        kind: u8,
        data: Vec<u8>,
    },
    /// DS5 output-report `0x02` audio-control region (samples ride [`PAD_AUDIO_MAGIC`]).
    /// `raw` is bytes 5..=10 (volumes + routing). `flags`: bit0 = haptics-select
    /// (`valid_flag0` bit1), bits1..4 = `valid_flag0` bits 4..7. Wire:
    /// `[0xCD][0x06][u16 pad LE][u8 flags][6 raw]`. Change-only; a rumbling pad would
    /// otherwise re-send unchanged audio state on every output report.
    AudioCtl {
        pad: u16,
        flags: u8,
        raw: [u8; 6],
    },
    /// Microphone-mute LED: `mode` 0 off, 1 on, 2 pulse (report `0x02` byte 9, enabled by
    /// `valid_flag1` bit 0). Wire: `[0xCD][0x07][pad][mode]`. A client that predates the tag
    /// drops this one datagram.
    MicLed {
        pad: u8,
        mode: u8,
    },
}

impl HidOutput {
    /// `u16` because [`HidOutput::AudioCtl`] carries one; other variants' `pad` is `u8` and widens.
    pub fn pad(&self) -> u16 {
        match self {
            HidOutput::Led { pad, .. }
            | HidOutput::PlayerLeds { pad, .. }
            | HidOutput::Trigger { pad, .. }
            | HidOutput::TrackpadHaptic { pad, .. }
            | HidOutput::HidRaw { pad, .. }
            | HidOutput::MicLed { pad, .. } => u16::from(*pad),
            HidOutput::AudioCtl { pad, .. } => *pad,
        }
    }

    /// Re-address to the client's wire pad. Backends tag feedback with the OS slot
    /// they created; `pf_inject::pad_pool` numbers each session from zero.
    /// Pad indices fit `u8` (`input::MAX_PADS` is 16); the assert pins that.
    pub fn with_pad(self, pad: u16) -> Self {
        debug_assert!(
            pad <= u16::from(u8::MAX),
            "pad index {pad} does not fit the wire's u8 variants"
        );
        let narrow = pad as u8;
        match self {
            HidOutput::Led { r, g, b, .. } => HidOutput::Led {
                pad: narrow,
                r,
                g,
                b,
            },
            HidOutput::PlayerLeds { bits, .. } => HidOutput::PlayerLeds { pad: narrow, bits },
            HidOutput::Trigger { which, effect, .. } => HidOutput::Trigger {
                pad: narrow,
                which,
                effect,
            },
            HidOutput::TrackpadHaptic {
                side,
                amplitude,
                period,
                count,
                ..
            } => HidOutput::TrackpadHaptic {
                pad: narrow,
                side,
                amplitude,
                period,
                count,
            },
            HidOutput::HidRaw { kind, data, .. } => HidOutput::HidRaw {
                pad: narrow,
                kind,
                data,
            },
            HidOutput::AudioCtl { flags, raw, .. } => HidOutput::AudioCtl { pad, flags, raw },
            HidOutput::MicLed { mode, .. } => HidOutput::MicLed { pad: narrow, mode },
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![HIDOUT_MAGIC];
        match self {
            HidOutput::Led { pad, r, g, b } => {
                out.extend_from_slice(&[HIDOUT_LED, *pad, *r, *g, *b])
            }
            HidOutput::PlayerLeds { pad, bits } => {
                out.extend_from_slice(&[HIDOUT_PLAYER_LEDS, *pad, *bits])
            }
            HidOutput::Trigger { pad, which, effect } => {
                out.extend_from_slice(&[HIDOUT_TRIGGER, *pad, *which]);
                out.extend_from_slice(&effect[..effect.len().min(TRIGGER_EFFECT_MAX)]);
            }
            HidOutput::TrackpadHaptic {
                pad,
                side,
                amplitude,
                period,
                count,
            } => {
                out.extend_from_slice(&[HIDOUT_TRACKPAD_HAPTIC, *pad, *side]);
                out.extend_from_slice(&amplitude.to_le_bytes());
                out.extend_from_slice(&period.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
            HidOutput::HidRaw { pad, kind, data } => {
                out.extend_from_slice(&[HIDOUT_HID_RAW, *pad, *kind]);
                out.extend_from_slice(&data[..data.len().min(HID_REPORT_MAX)]);
            }
            HidOutput::AudioCtl { pad, flags, raw } => {
                out.push(HIDOUT_AUDIO_CTL);
                out.extend_from_slice(&pad.to_le_bytes());
                out.push(*flags);
                out.extend_from_slice(raw);
            }
            HidOutput::MicLed { pad, mode } => {
                out.extend_from_slice(&[HIDOUT_MIC_LED, *pad, *mode])
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<HidOutput> {
        if b.first() != Some(&HIDOUT_MAGIC) {
            return None;
        }
        match *b.get(1)? {
            HIDOUT_LED if b.len() >= 6 => Some(HidOutput::Led {
                pad: b[2],
                r: b[3],
                g: b[4],
                b: b[5],
            }),
            HIDOUT_PLAYER_LEDS if b.len() >= 4 => Some(HidOutput::PlayerLeds {
                pad: b[2],
                bits: b[3],
            }),
            // `> 4`, not `>= 4`: no effect bytes is malformed. An empty block becomes
            // an all-zero trigger report (mode 0x00), which RELEASES a held effect.
            // A genuine "no effect" is a full-length zero block.
            HIDOUT_TRIGGER if b.len() > 4 => Some(HidOutput::Trigger {
                pad: b[2],
                which: b[3],
                effect: b[4..b.len().min(4 + TRIGGER_EFFECT_MAX)].to_vec(),
            }),
            HIDOUT_TRACKPAD_HAPTIC if b.len() >= 10 => Some(HidOutput::TrackpadHaptic {
                pad: b[2],
                side: b[3],
                amplitude: u16::from_le_bytes([b[4], b[5]]),
                period: u16::from_le_bytes([b[6], b[7]]),
                count: u16::from_le_bytes([b[8], b[9]]),
            }),
            HIDOUT_HID_RAW if b.len() >= 5 => Some(HidOutput::HidRaw {
                pad: b[2],
                kind: b[3],
                data: b[4..b.len().min(4 + HID_REPORT_MAX)].to_vec(),
            }),
            // Pad is the only u16 index on this plane; consumers narrow with `as u8`.
            // Wire pad 256 would alias onto slot 0. Reject here so the narrowings
            // downstream are lossless (`input::MAX_PADS`).
            HIDOUT_AUDIO_CTL
                if b.len() >= 11
                    && u16::from_le_bytes([b[2], b[3]]) < crate::input::MAX_PADS as u16 =>
            {
                Some(HidOutput::AudioCtl {
                    pad: u16::from_le_bytes([b[2], b[3]]),
                    flags: b[4],
                    raw: b[5..11].try_into().unwrap(),
                })
            }
            HIDOUT_MIC_LED if b.len() >= 4 => Some(HidOutput::MicLed {
                pad: b[2],
                mode: b[3],
            }),
            _ => None,
        }
    }
}

/// SMPTE ST.2086 mastering volume + CEA-861.3 content light level.
/// Datagram not [`Welcome`]: larger, and can change mid-stream; the host re-sends
/// on keyframes so a dropped best-effort datagram converges. Omitted for HLG
/// (scene-referred — no mastering metadata).
///
/// HDR10 SEI fixed-point units: pass to `DXGI_HDR_METADATA_HDR10` /
/// Android `KEY_HDR_STATIC_INFO` / Apple `CAEDRMetadata`. libavcodec
/// `AVMasteringDisplayMetadata` needs an `AVRational` conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HdrMeta {
    /// Primaries G, B, R as (x, y) in 1/50000 units (ST.2086 order is G, B, R).
    pub display_primaries: [[u16; 2]; 3],
    /// White point (x, y) in 1/50000 units.
    pub white_point: [u16; 2],
    /// Max display mastering luminance, 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
    /// MaxCLL, nits. `0` = unknown.
    pub max_cll: u16,
    /// MaxFALL, nits. `0` = unknown.
    pub max_fall: u16,
}

pub const HDR_META_MAGIC: u8 = 0xCE;

/// [`HdrMeta`] body (no tag): 6×u16 primaries + 2×u16 white + 2×u32 luminance +
/// 2×u16 CLL/FALL = 28. Shared by the [`HDR_META_MAGIC`] datagram and `Hello::display_hdr`.
pub const HDR_META_BODY_LEN: usize = 12 + 4 + 8 + 4;

const HDR_META_LEN: usize = 1 + HDR_META_BODY_LEN;

pub fn write_hdr_meta_body(m: &HdrMeta, b: &mut Vec<u8>) {
    for p in m.display_primaries.iter() {
        b.extend_from_slice(&p[0].to_le_bytes());
        b.extend_from_slice(&p[1].to_le_bytes());
    }
    b.extend_from_slice(&m.white_point[0].to_le_bytes());
    b.extend_from_slice(&m.white_point[1].to_le_bytes());
    b.extend_from_slice(&m.max_display_mastering_luminance.to_le_bytes());
    b.extend_from_slice(&m.min_display_mastering_luminance.to_le_bytes());
    b.extend_from_slice(&m.max_cll.to_le_bytes());
    b.extend_from_slice(&m.max_fall.to_le_bytes());
}

/// Caller guarantees `b` is at least [`HDR_META_BODY_LEN`] (both callers `get` an exact slice).
pub fn read_hdr_meta_body(b: &[u8]) -> HdrMeta {
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    HdrMeta {
        display_primaries: [
            [u16at(0), u16at(2)],
            [u16at(4), u16at(6)],
            [u16at(8), u16at(10)],
        ],
        white_point: [u16at(12), u16at(14)],
        max_display_mastering_luminance: u32at(16),
        min_display_mastering_luminance: u32at(20),
        max_cll: u16at(24),
        max_fall: u16at(26),
    }
}

pub fn encode_hdr_meta_datagram(m: &HdrMeta) -> Vec<u8> {
    let mut b = Vec::with_capacity(HDR_META_LEN);
    b.push(HDR_META_MAGIC);
    write_hdr_meta_body(m, &mut b);
    b
}

pub fn decode_hdr_meta_datagram(b: &[u8]) -> Option<HdrMeta> {
    if b.len() < HDR_META_LEN || b[0] != HDR_META_MAGIC {
        return None;
    }
    Some(read_hdr_meta_body(&b[1..]))
}

/// Per-AU host timing. Once per access unit, after its last packet left the socket,
/// and only when the client advertised [`VIDEO_CAP_HOST_TIMING`].
pub const HOST_TIMING_MAGIC: u8 = 0xCF;

/// One AU's host-side time: capture → fully sent. The client correlates by `pts_ns`
/// and derives `network = (received + clock_offset − pts_ns) − host_us`. A lost
/// 0xCF means that frame contributes no host/network sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTiming {
    /// AU capture stamp — matches the AU's `pts_ns` exactly.
    pub pts_ns: u64,
    /// Capture→sent, µs. Saturates at `u32::MAX` ≈ 71 min — past the 10 s client sanity clamp.
    pub host_us: u32,
    /// Per-stage split of `host_us`. `None` from a host that predates the tail.
    /// Decode reads the 13-byte prefix and takes stages only when present — no
    /// capability bit: old client + new host reads the prefix.
    pub stages: Option<HostStages>,
    /// Capture-tick hold currently applied, ns. Rides after the stages block.
    /// `None` from a pre-phase-lock host or a shorter datagram.
    pub applied_phase_ns: Option<i32>,
}

/// Per-stage split of [`HostTiming::host_us`], all µs from the same capture anchor.
/// `host_us = queue + encode + residual(seal/FEC + channel-wait) + pace`; the
/// client derives residual as `host_us − queue_us − encode_us − pace_us`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStages {
    /// Capture delivery → encoder submit (ring / channel-queue age). 0 for re-encoded hold frames.
    pub queue_us: u32,
    /// Encoder submit → bitstream ready (scheduling wait + ASIC time).
    pub encode_us: u32,
    /// First byte to the socket → last packet sent (microburst spread).
    pub pace_us: u32,
}

const HOST_TIMING_LEN: usize = 1 + 8 + 4;
/// With [`HostStages`] tail: + 3 × u32 = 25.
const HOST_TIMING_STAGES_LEN: usize = HOST_TIMING_LEN + 12;
/// With phase-lock ACK after stages: + i32 = 29. Each form is a strict prefix of the next.
const HOST_TIMING_PHASE_LEN: usize = HOST_TIMING_STAGES_LEN + 4;

/// Extended form when `stages` is set — an older client parses the prefix and ignores the tail.
pub fn encode_host_timing_datagram(t: &HostTiming) -> Vec<u8> {
    let mut b = Vec::with_capacity(HOST_TIMING_PHASE_LEN);
    b.push(HOST_TIMING_MAGIC);
    b.extend_from_slice(&t.pts_ns.to_le_bytes());
    b.extend_from_slice(&t.host_us.to_le_bytes());
    if let Some(s) = &t.stages {
        b.extend_from_slice(&s.queue_us.to_le_bytes());
        b.extend_from_slice(&s.encode_us.to_le_bytes());
        b.extend_from_slice(&s.pace_us.to_le_bytes());
        // Phase ACK only after a stages tail: the prefix wire cannot express
        // "phase but no stages", and every host that phase-locks sends stages.
        if let Some(p) = t.applied_phase_ns {
            b.extend_from_slice(&p.to_le_bytes());
        }
    }
    b
}

/// A 13-byte (legacy) datagram yields `stages: None`.
pub fn decode_host_timing_datagram(b: &[u8]) -> Option<HostTiming> {
    if b.len() < HOST_TIMING_LEN || b[0] != HOST_TIMING_MAGIC {
        return None;
    }
    let stages = (b.len() >= HOST_TIMING_STAGES_LEN).then(|| HostStages {
        queue_us: u32::from_le_bytes(b[13..17].try_into().unwrap()),
        encode_us: u32::from_le_bytes(b[17..21].try_into().unwrap()),
        pace_us: u32::from_le_bytes(b[21..25].try_into().unwrap()),
    });
    let applied_phase_ns = (b.len() >= HOST_TIMING_PHASE_LEN)
        .then(|| i32::from_le_bytes(b[25..29].try_into().unwrap()));
    Some(HostTiming {
        pts_ns: u64::from_le_bytes(b[1..9].try_into().unwrap()),
        host_us: u32::from_le_bytes(b[9..13].try_into().unwrap()),
        stages,
        applied_phase_ns,
    })
}

/// Cursor state. Once per captured frame while
/// [`CLIENT_CAP_CURSOR`](super::caps::CLIENT_CAP_CURSOR) ∧
/// [`HOST_CAP_CURSOR`](super::caps::HOST_CAP_CURSOR) — per-frame resend is
/// latest-wins under loss. The bitmap rides the control stream
/// ([`CursorShape`](super::control::CursorShape)); this datagram only moves/hides.
pub const CURSOR_STATE_MAGIC: u8 = 0xD0;

pub const CURSOR_VISIBLE: u8 = 0x01;
/// Host app captured/hid the pointer — the client should run relative/captured.
/// Advisory; user override always wins.
pub const CURSOR_RELATIVE_HINT: u8 = 0x02;

/// Per-frame host cursor. `x`/`y` are the hotspot in the host OUTPUT pixel space —
/// the same space the video mode describes, so the client maps through letterbox
/// like touches, in reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorState {
    /// [`CursorShape`](super::control::CursorShape) serial. No cached shape: keep the
    /// previous cursor until the reliable shape message lands — stale shape, never a
    /// wrong position.
    pub serial: u32,
    pub flags: u8,
    pub x: i32,
    pub y: i32,
}

impl CursorState {
    pub fn visible(&self) -> bool {
        self.flags & CURSOR_VISIBLE != 0
    }
    pub fn relative_hint(&self) -> bool {
        self.flags & CURSOR_RELATIVE_HINT != 0
    }
}

const CURSOR_STATE_LEN: usize = 1 + 4 + 1 + 8;

pub fn encode_cursor_state_datagram(s: &CursorState) -> Vec<u8> {
    let mut b = Vec::with_capacity(CURSOR_STATE_LEN);
    b.push(CURSOR_STATE_MAGIC);
    b.extend_from_slice(&s.serial.to_le_bytes());
    b.push(s.flags);
    b.extend_from_slice(&s.x.to_le_bytes());
    b.extend_from_slice(&s.y.to_le_bytes());
    b
}

/// A longer buffer is tolerated (append-extension, like 0xCF).
pub fn decode_cursor_state_datagram(b: &[u8]) -> Option<CursorState> {
    if b.len() < CURSOR_STATE_LEN || b[0] != CURSOR_STATE_MAGIC {
        return None;
    }
    Some(CursorState {
        serial: u32::from_le_bytes(b[1..5].try_into().unwrap()),
        flags: b[5],
        x: i32::from_le_bytes(b[6..10].try_into().unwrap()),
        y: i32::from_le_bytes(b[10..14].try_into().unwrap()),
    })
}

/// Per-gamepad DualSense audio (voice-coil haptics and speaker) for the matching
/// real controller. Samples on this plane; routing/volume on [`HidOutput::AudioCtl`].
/// Emitted only when the session negotiated
/// [`CLIENT_CAP_PAD_AUDIO`](super::caps::CLIENT_CAP_PAD_AUDIO) ∧
/// [`HOST_CAP_PAD_AUDIO`](super::caps::HOST_CAP_PAD_AUDIO) and the pad arrival
/// declared a renderer
/// ([`crate::input::ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`]/`_SPEAKER`).
/// A lost frame is a concealed gap, never state.
pub const PAD_AUDIO_MAGIC: u8 = 0xD1;

/// DualSense voice-coil actuators. 5 ms Opus, same cadence as [`AUDIO_MAGIC`]: haptics are felt latency.
pub const PAD_AUDIO_KIND_HAPTICS: u8 = 0;
/// Controller speaker. 10 ms Opus — speaker content can buffer for coding efficiency.
pub const PAD_AUDIO_KIND_SPEAKER: u8 = 1;

const PAD_AUDIO_HEADER_LEN: usize = 1 + 1 + 1 + 4 + 8;

/// Owned — the client's plane queue stores it. `seq`/`pts_ns` are per-(pad, kind)
/// from the host capture clock, for gap concealment and lip-sync against main audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadAudioFrame {
    pub pad: u8,
    pub kind: u8,
    pub seq: u32,
    pub pts_ns: u64,
    /// Opus frame. Empty = DTX silence.
    pub opus: Vec<u8>,
}

/// One Opus frame (5/10 ms — under any MTU).
pub fn encode_pad_audio_datagram(pad: u8, kind: u8, seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(PAD_AUDIO_HEADER_LEN + opus.len());
    b.push(PAD_AUDIO_MAGIC);
    b.push(pad);
    b.push(kind);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

pub fn decode_pad_audio_datagram(buf: &[u8]) -> Option<PadAudioFrame> {
    if buf.len() < PAD_AUDIO_HEADER_LEN || buf[0] != PAD_AUDIO_MAGIC {
        return None;
    }
    Some(PadAudioFrame {
        pad: buf[1],
        kind: buf[2],
        seq: u32::from_le_bytes(buf[3..7].try_into().unwrap()),
        pts_ns: u64::from_le_bytes(buf[7..15].try_into().unwrap()),
        opus: buf[15..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn hdr_meta_datagram_roundtrip_and_truncation() {
        let m = HdrMeta {
            // BT.2020 primaries in 1/50000 units (DXGI/ST.2086 reference).
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],                 // D65
            max_display_mastering_luminance: 10_000_000, // 1000 nits in 0.0001 cd/m²
            min_display_mastering_luminance: 1,          // 0.0001 nits
            max_cll: 1000,
            max_fall: 400,
        };
        let d = encode_hdr_meta_datagram(&m);
        assert_eq!(d[0], HDR_META_MAGIC);
        assert_eq!(decode_hdr_meta_datagram(&d), Some(m));
        for n in 0..d.len() {
            assert_eq!(decode_hdr_meta_datagram(&d[..n]), None);
        }
        let mut bad = d.clone();
        bad[0] = HIDOUT_MAGIC;
        assert_eq!(decode_hdr_meta_datagram(&bad), None);
    }

    #[test]
    fn host_timing_datagram_roundtrip_and_truncation() {
        let t = HostTiming {
            pts_ns: 1_751_500_000_123_456_789, // CLOCK_REALTIME-scale capture stamp
            host_us: 4_321,
            stages: None,
            applied_phase_ns: None,
        };
        let d = encode_host_timing_datagram(&t);
        assert_eq!(d[0], HOST_TIMING_MAGIC);
        assert_eq!(d.len(), 13);
        assert_eq!(decode_host_timing_datagram(&d), Some(t));
        for n in 0..d.len() {
            assert_eq!(decode_host_timing_datagram(&d[..n]), None);
        }
        let mut bad = d.clone();
        bad[0] = HDR_META_MAGIC;
        assert_eq!(decode_host_timing_datagram(&bad), None);

        // Stage tail roundtrips. A truncated tail degrades to `stages: None`, never
        // a partial read; the prefix is identical in both forms.
        let ts = HostTiming {
            stages: Some(HostStages {
                queue_us: 900,
                encode_us: 3_100,
                pace_us: 2_500,
            }),
            ..t
        };
        let ds = encode_host_timing_datagram(&ts);
        assert_eq!(ds.len(), 25);
        assert_eq!(
            &ds[..13],
            &d[..13],
            "prefix is byte-identical to the legacy form"
        );
        assert_eq!(decode_host_timing_datagram(&ds), Some(ts));
        for n in 13..ds.len() {
            assert_eq!(
                decode_host_timing_datagram(&ds[..n]),
                Some(t),
                "partial stage tail ({n} B) must degrade to the legacy decode"
            );
        }

        // Phase-ACK: 29 B roundtrips; 25..28 degrade to stages; a phase without
        // stages is unencodable (prefix discipline).
        let tp = HostTiming {
            applied_phase_ns: Some(-2_750_000),
            ..ts
        };
        let dp = encode_host_timing_datagram(&tp);
        assert_eq!(dp.len(), 29);
        assert_eq!(&dp[..25], &ds[..25], "stages form is a strict prefix");
        assert_eq!(decode_host_timing_datagram(&dp), Some(tp));
        for n in 25..dp.len() {
            assert_eq!(
                decode_host_timing_datagram(&dp[..n]),
                Some(ts),
                "partial phase tail ({n} B) must degrade to the stages decode"
            );
        }
        let no_stages = HostTiming {
            stages: None,
            applied_phase_ns: Some(1),
            ..t
        };
        assert_eq!(
            encode_host_timing_datagram(&no_stages).len(),
            13,
            "phase without stages must encode as the legacy form (prefix discipline)"
        );
    }

    #[test]
    fn audio_datagram_roundtrip() {
        let opus = [0x42u8; 97];
        let d = encode_audio_red_datagram(7, 42, &opus, &[]);
        assert_eq!(d[0], AUDIO_RED_MAGIC);
        let d = encode_audio_datagram(7, 1_000_000_123, &opus);
        assert_eq!(d[0], AUDIO_MAGIC);
        let (seq, pts, payload) = decode_audio_datagram(&d).unwrap();
        assert_eq!((seq, pts), (7, 1_000_000_123));
        assert_eq!(payload, opus);
        assert!(decode_audio_datagram(&d[..12]).is_none());
        assert!(decode_audio_datagram(&[0u8; 13]).is_none());

        // Empty payload is legal (DTX) — header-only datagram.
        let header_only = encode_audio_datagram(0, 0, &[]);
        let (_, _, empty) = decode_audio_datagram(&header_only).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn audio_red_datagram_roundtrip() {
        let cur = [0x42u8; 97];
        let prev = [0x37u8; 88];
        let d = encode_audio_red_datagram(7, 1_000_000_123, &cur, &prev);
        assert_eq!(d[0], AUDIO_RED_MAGIC);
        let (seq, pts, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!((seq, pts), (7, 1_000_000_123));
        assert_eq!(primary, cur);
        assert_eq!(previous, Some(&prev[..]));

        // Empty tail = no predecessor (first frame, or after a capture reopen).
        let d = encode_audio_red_datagram(0, 5, &cur, &[]);
        let (_, _, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!(primary, cur);
        assert_eq!(
            previous, None,
            "an empty tail must decode as absent, not as a zero-length frame"
        );

        // Equal-length frames still split at the length prefix — nothing else tells them apart.
        let a = [1u8; 64];
        let b = [2u8; 64];
        let d = encode_audio_red_datagram(9, 0, &a, &b);
        let (_, _, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!(primary, a);
        assert_eq!(previous, Some(&b[..]));
    }

    /// Over-long `primary_len` must not panic on `split_at`.
    #[test]
    fn audio_red_datagram_rejects_bad_input() {
        let d = encode_audio_red_datagram(1, 2, &[0xAAu8; 30], &[0xBBu8; 20]);
        for n in 0..AUDIO_RED_HEADER {
            assert!(decode_audio_red_datagram(&d[..n]).is_none(), "len {n}");
        }
        // primary_len larger than the datagram: refuse, do not slice.
        let mut bad = d.clone();
        bad[13..15].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(decode_audio_red_datagram(&bad).is_none());
        let mut wrong = d.clone();
        wrong[0] = AUDIO_MAGIC;
        assert!(decode_audio_red_datagram(&wrong).is_none());
    }

    #[test]
    fn audio_pcm_datagram_roundtrip() {
        let payload: Vec<u8> = (0..1152u32).map(|i| (i % 251) as u8).collect();
        let d = encode_audio_pcm_datagram(7, 1_234_567_890, &payload);
        assert_eq!(d[0], AUDIO_PCM_MAGIC);
        assert_eq!(d.len(), AUDIO_PCM_HEADER + payload.len());
        // Same header as 0xC9: seq and pts sit at the same offsets.
        assert_eq!(AUDIO_PCM_HEADER, 13);
        let (seq, pts, out) = decode_audio_pcm_datagram(&d).expect("decode");
        assert_eq!((seq, pts), (7, 1_234_567_890));
        assert_eq!(out, &payload[..]);

        assert!(decode_audio_pcm_datagram(&encode_audio_pcm_datagram(1, 2, &[])).is_some());
        assert!(decode_audio_pcm_datagram(&d[..AUDIO_PCM_HEADER - 1]).is_none());
        let mut wrong = d.clone();
        wrong[0] = AUDIO_MAGIC;
        assert!(decode_audio_pcm_datagram(&wrong).is_none());
    }

    /// Ladder pick must fit the budget it was sized for — keeps this plane off any fragmentation path.
    #[test]
    fn a_ladder_sized_frame_fits_the_datagram_it_was_sized_for() {
        use crate::audio::pcm;
        // 44.1 kHz carries a fractional sample count on most rungs; the frame is the floor.
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400] {
            for bits in [pcm::BITS_16, pcm::BITS_24] {
                for budget in [900usize, 1200, 1400] {
                    let Some(us) = pcm::frame_us_for(rate, bits, 2, budget) else {
                        // 176400/24-bit needs 1069 B for a 1 ms frame; a smaller budget declines the plane.
                        continue;
                    };
                    let samples = pcm::samples_per_frame(rate, us, 2);
                    let mut wire = Vec::new();
                    pcm::from_f32(&vec![0.25f32; samples], bits, &mut wire);
                    let d = encode_audio_pcm_datagram(0, 0, &wire);
                    assert!(
                        d.len() <= budget,
                        "{rate}/{bits} at {budget} B produced a {} B datagram",
                        d.len()
                    );
                }
            }
        }
    }

    #[test]
    fn audio_red_tag_is_disjoint() {
        for other in [
            AUDIO_MAGIC,
            RUMBLE_MAGIC,
            MIC_MAGIC,
            RICH_INPUT_MAGIC,
            HIDOUT_MAGIC,
            HDR_META_MAGIC,
            HOST_TIMING_MAGIC,
            CURSOR_STATE_MAGIC,
            AUDIO_PCM_MAGIC,
            crate::input::INPUT_MAGIC,
        ] {
            assert_ne!(AUDIO_RED_MAGIC, other);
        }
        let red = encode_audio_red_datagram(1, 2, &[9u8; 40], &[8u8; 40]);
        assert!(
            decode_audio_datagram(&red).is_none(),
            "0xC9 must not accept a 0xD2"
        );
        let plain = encode_audio_datagram(1, 2, &[9u8; 40]);
        assert!(
            decode_audio_pcm_datagram(&plain).is_none(),
            "0xD3 must not accept a 0xC9"
        );
        assert!(
            decode_audio_red_datagram(&plain).is_none(),
            "0xD2 must not accept a 0xC9"
        );
    }

    #[test]
    fn rumble_datagram_roundtrip() {
        let d = encode_rumble_datagram(1, 0x1234, 0xFFFF);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(decode_rumble_datagram(&d), Some((1, 0x1234, 0xFFFF)));
        assert!(decode_rumble_datagram(&d[..6]).is_none());
    }

    /// Encode and decode both clamp; a body with no effect bytes must not decode.
    #[test]
    fn trigger_effect_is_clamped_on_both_encode_and_decode() {
        let long = HidOutput::Trigger {
            pad: 1,
            which: 0,
            effect: vec![0xAB; 200],
        };
        let d = long.encode();
        assert_eq!(
            d.len(),
            4 + TRIGGER_EFFECT_MAX,
            "magic + kind + pad + which + at most the parameter block"
        );

        // Decode clamps independently of encode — a peer does not use our encoder.
        let mut hostile = vec![HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 1, 0];
        hostile.extend_from_slice(&[0xCD; 500]);
        match HidOutput::decode(&hostile) {
            Some(HidOutput::Trigger { effect, .. }) => {
                assert_eq!(effect.len(), TRIGGER_EFFECT_MAX, "tail is bounded");
            }
            other => panic!("expected a clamped Trigger, got {other:?}"),
        }

        let ok = HidOutput::Trigger {
            pad: 2,
            which: 1,
            effect: vec![0x02, 0x90, 0xA0, 0xFF, 0, 0, 0, 0, 0, 0, 0],
        };
        assert_eq!(HidOutput::decode(&ok.encode()), Some(ok));
    }

    /// No effect bytes must not decode as empty: an empty block is mode 0x00 and RELEASES a held effect.
    #[test]
    fn a_trigger_with_no_effect_bytes_is_rejected_not_read_as_cancel() {
        let empty = [HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 0, 0];
        assert_eq!(HidOutput::decode(&empty), None);

        // One byte of effect is a legitimate short block (consumers zero-pad it).
        let one = [HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 0, 0, 0x02];
        assert_eq!(
            HidOutput::decode(&one),
            Some(HidOutput::Trigger {
                pad: 0,
                which: 0,
                effect: vec![0x02]
            })
        );
    }

    /// Pin [`HidOutput::HidRaw`] next to [`HidOutput::Trigger`] so the two bounds cannot drift.
    #[test]
    fn hid_raw_stays_bounded_on_both_sides() {
        let long = HidOutput::HidRaw {
            pad: 0,
            kind: HID_RAW_OUTPUT,
            data: vec![0x11; 500],
        };
        assert_eq!(long.encode().len(), 4 + HID_REPORT_MAX);

        let mut hostile = vec![HIDOUT_MAGIC, super::HIDOUT_HID_RAW, 0, HID_RAW_FEATURE];
        hostile.extend_from_slice(&[0x22; 900]);
        match HidOutput::decode(&hostile) {
            Some(HidOutput::HidRaw { data, .. }) => assert_eq!(data.len(), HID_REPORT_MAX),
            other => panic!("expected a clamped HidRaw, got {other:?}"),
        }
    }

    #[test]
    fn rumble_envelope_roundtrip_and_legacy_tolerance() {
        let d = encode_rumble_datagram_v2(2, 0x4000, 0x8000, 7, 400);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(d.len(), RUMBLE_V2_LEN);
        assert_eq!(
            decode_rumble_envelope(&d),
            Some(RumbleUpdate {
                pad: 2,
                low: 0x4000,
                high: 0x8000,
                left_trigger: 0,
                right_trigger: 0,
                envelope: Some(RumbleEnvelope {
                    seq: 7,
                    ttl_ms: 400
                }),
            })
        );
        // Old client: first 7 bytes are the level; the tail is ignored.
        assert_eq!(decode_rumble_datagram(&d), Some((2, 0x4000, 0x8000)));

        // 7-byte (legacy) datagram: no envelope — the client applies its own staleness policy.
        let v1 = encode_rumble_datagram(3, 0x1111, 0x2222);
        assert_eq!(
            decode_rumble_envelope(&v1),
            Some(RumbleUpdate {
                pad: 3,
                low: 0x1111,
                high: 0x2222,
                left_trigger: 0,
                right_trigger: 0,
                envelope: None,
            })
        );

        // Torn tail (8 or 9 bytes): degrade to a level, never panic or drop.
        assert_eq!(
            decode_rumble_envelope(&d[..8]).map(|u| u.envelope),
            Some(None)
        );
        assert_eq!(
            decode_rumble_envelope(&d[..9]).map(|u| u.envelope),
            Some(None)
        );

        assert!(decode_rumble_envelope(&d[..6]).is_none());
        let mut wrong_tag = d;
        wrong_tag[0] = AUDIO_MAGIC;
        assert!(decode_rumble_envelope(&wrong_tag).is_none());
    }

    /// First 10 bytes must be byte-identical to v2, or a v2-era client reads a displaced envelope
    /// and every TTL/seq guarantee on this plane changes meaning.
    #[test]
    fn rumble_v3_roundtrips_and_keeps_the_v2_envelope_in_place() {
        let v2 = encode_rumble_datagram_v2(2, 0x4000, 0x8000, 7, 400);
        let v3 = encode_rumble_datagram_v3(2, 0x4000, 0x8000, 7, 400, 0x1234, 0xFFFF);
        assert_eq!(v3.len(), RUMBLE_V3_LEN);
        assert_eq!(&v3[..RUMBLE_V2_LEN], &v2[..], "v2 is a strict prefix of v3");
        // LE tail pinned as bytes: an endianness slip reads 0x1234 as 0x3412 and a same-encoder
        // round-trip would not catch it.
        assert_eq!(&v3[10..14], &[0x34, 0x12, 0xFF, 0xFF]);
        assert_eq!(
            decode_rumble_envelope(&v3),
            Some(RumbleUpdate {
                pad: 2,
                low: 0x4000,
                high: 0x8000,
                left_trigger: 0x1234,
                right_trigger: 0xFFFF,
                envelope: Some(RumbleEnvelope {
                    seq: 7,
                    ttl_ms: 400
                }),
            })
        );
        let trig_only = encode_rumble_datagram_v3(0, 0, 0, 3, 400, 0x8000, 0);
        let u = decode_rumble_envelope(&trig_only).unwrap();
        assert_eq!((u.low, u.high), (0, 0));
        assert_eq!((u.left_trigger, u.right_trigger), (0x8000, 0));
        assert_eq!(u.envelope.unwrap().ttl_ms, 400);
    }

    #[test]
    fn rumble_v3_and_v2_parse_each_others_datagrams() {
        let v3 = encode_rumble_datagram_v3(1, 0x1111, 0x2222, 9, 250, 0xAAAA, 0xBBBB);

        // New host → old client: level decoder ignores both tails; envelope decoder reads seq/ttl.
        assert_eq!(decode_rumble_datagram(&v3), Some((1, 0x1111, 0x2222)));
        assert_eq!(
            decode_rumble_envelope(&v3).unwrap().envelope,
            Some(RumbleEnvelope {
                seq: 9,
                ttl_ms: 250
            })
        );

        // Old host → new client: v1 and v2 decode with the triggers silent, not "unchanged".
        for (form, d) in [
            ("v1", encode_rumble_datagram(1, 0x1111, 0x2222).to_vec()),
            (
                "v2",
                encode_rumble_datagram_v2(1, 0x1111, 0x2222, 9, 250).to_vec(),
            ),
        ] {
            let u = decode_rumble_envelope(&d).unwrap();
            assert_eq!(
                (u.left_trigger, u.right_trigger),
                (0, 0),
                "{form} must decode to idle triggers"
            );
            assert_eq!((u.pad, u.low, u.high), (1, 0x1111, 0x2222));
        }

        // Torn trigger tail: degrade to v2, never read half a level from one leftover byte.
        let v2 = decode_rumble_envelope(&encode_rumble_datagram_v2(1, 0x1111, 0x2222, 9, 250));
        for n in RUMBLE_V2_LEN..RUMBLE_V3_LEN {
            assert_eq!(
                decode_rumble_envelope(&v3[..n]),
                v2,
                "partial trigger tail ({n} B) must degrade to the v2 decode"
            );
        }
    }

    #[test]
    fn rumble_envelope_seq_gate_drops_reordered_stale_start() {
        use crate::input::GamepadSnapshot;
        // Same reorder gate as gamepad snapshots: a stale start after a stop must not re-light.
        let stop = decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0, 0, 10, 0)).unwrap();
        let stale_start =
            decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0x8000, 0x8000, 9, 400)).unwrap();
        let stop_seq = stop.envelope.unwrap().seq;
        let stale_seq = stale_start.envelope.unwrap().seq;
        assert!(GamepadSnapshot::seq_newer(stop_seq, None));
        assert!(!GamepadSnapshot::seq_newer(stale_seq, Some(stop_seq)));
        assert!(GamepadSnapshot::seq_newer(11, Some(stop_seq)));
        // Wrap: seq 1 supersedes 254.
        assert!(GamepadSnapshot::seq_newer(1, Some(254)));
    }

    #[test]
    fn mic_datagram_roundtrip_and_disjoint_from_audio() {
        let opus = [0x5Au8; 80];
        let d = encode_mic_datagram(42, 9_999, &opus);
        assert_eq!(d[0], MIC_MAGIC);
        let (seq, pts, payload) = decode_mic_datagram(&d).unwrap();
        assert_eq!((seq, pts), (42, 9_999));
        assert_eq!(payload, opus);
        assert!(decode_mic_datagram(&d[..12]).is_none());
        assert!(decode_audio_datagram(&d).is_none());
        assert!(decode_mic_datagram(&encode_audio_datagram(1, 2, &opus)).is_none());
        // Empty payload (DTX) is legal.
        assert!(decode_mic_datagram(&encode_mic_datagram(0, 0, &[]))
            .unwrap()
            .2
            .is_empty());
    }

    #[test]
    fn rich_input_roundtrip() {
        for ev in [
            RichInput::Touchpad {
                pad: 1,
                finger: 0,
                active: true,
                x: 40000,
                y: 12345,
            },
            RichInput::Motion {
                pad: 0,
                gyro: [-100, 200, -300],
                accel: [16384, -8192, 1],
            },
            RichInput::TouchpadEx {
                pad: 2,
                surface: 1,
                finger: 1,
                touch: true,
                click: false,
                x: -12345,
                y: 30000,
                pressure: 4000,
            },
        ] {
            let d = ev.encode();
            assert_eq!(d[0], RICH_INPUT_MAGIC);
            assert_eq!(RichInput::decode(&d), Some(ev));
        }
        let mut data = [0u8; HID_REPORT_MAX];
        data[0] = 0x42; // ID_TRITON_CONTROLLER_STATE
        for (i, b) in data.iter_mut().enumerate().take(46).skip(1) {
            *b = i as u8;
        }
        let raw = RichInput::HidReport {
            pad: 3,
            len: 46,
            data,
        };
        let d = raw.encode();
        assert_eq!(d.len(), 4 + 46); // no fixed-array padding on the wire
        assert_eq!(RichInput::decode(&d), Some(raw));
        // Torn HidReport truncates to what arrived; `len` must not over-read.
        assert_eq!(
            RichInput::decode(&d[..20]),
            Some(RichInput::HidReport {
                pad: 3,
                len: 16,
                data: {
                    let mut t = [0u8; HID_REPORT_MAX];
                    t[..16].copy_from_slice(&data[..16]);
                    t
                },
            })
        );
        assert!(RichInput::decode(&[crate::input::INPUT_MAGIC; 18]).is_none());
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, 0x7F]).is_none());
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD, 0]).is_none());
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD_EX, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn hid_output_roundtrip() {
        let cases = [
            HidOutput::Led {
                pad: 2,
                r: 0xAA,
                g: 0xBB,
                b: 0xCC,
            },
            HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b10101,
            },
            HidOutput::Trigger {
                pad: 1,
                which: 1,
                effect: vec![0x26, 0x90, 0xA0, 0xFF, 0x00, 0x00],
            },
            HidOutput::TrackpadHaptic {
                pad: 0,
                side: 1,
                amplitude: 0x1234,
                period: 0x5678,
                count: 9,
            },
            HidOutput::HidRaw {
                pad: 1,
                kind: HID_RAW_OUTPUT,
                data: vec![0x80, 0, 0, 0, 0x34, 0x12, 0, 0x78, 0x56, 0],
            },
            HidOutput::HidRaw {
                pad: 0,
                kind: HID_RAW_FEATURE,
                data: {
                    let mut f = vec![0u8; HID_REPORT_MAX];
                    f[0] = 1; // Triton feature reports ride report id 1
                    f[1] = 0x87; // ID_SET_SETTINGS_VALUES
                    f
                },
            },
            HidOutput::AudioCtl {
                pad: 1,
                flags: 0b0_0101,
                raw: [0x50, 0x60, 0x70, 0x05, 0x00, 0x00],
            },
            HidOutput::MicLed { pad: 2, mode: 2 },
        ];
        for ev in &cases {
            let d = ev.encode();
            assert_eq!(d[0], HIDOUT_MAGIC);
            assert_eq!(HidOutput::decode(&d).as_ref(), Some(ev));
        }
        assert!(HidOutput::decode(&[HIDOUT_MAGIC, 0x7F]).is_none());
        assert!(HidOutput::decode(
            &RichInput::Motion {
                pad: 0,
                gyro: [0; 3],
                accel: [0; 3]
            }
            .encode()
        )
        .is_none());
    }

    /// `[0xCD][0x07][pad][mode]`; anything shorter is dropped, like every other tag.
    #[test]
    fn mic_led_wire_layout_and_truncation() {
        let m = HidOutput::MicLed { pad: 3, mode: 1 };
        let d = m.encode();
        assert_eq!(d, [0xCD, 0x07, 3, 1]);
        assert_eq!(HidOutput::decode(&d), Some(m.clone()));
        assert_eq!(HidOutput::decode(&d[..3]), None);
        assert_eq!(m.with_pad(5), HidOutput::MicLed { pad: 5, mode: 1 });
    }

    #[test]
    fn audio_ctl_wire_layout_and_truncation() {
        // Pad is in-range (0x000B). Do not pin an out-of-range pad that would alias through `as u8`.
        let a = HidOutput::AudioCtl {
            pad: 0x000B,
            flags: 0x17,
            raw: [1, 2, 3, 4, 5, 6],
        };
        let d = a.encode();
        assert_eq!(d, [0xCD, 0x06, 0x0B, 0x00, 0x17, 1, 2, 3, 4, 5, 6]);
        assert_eq!(HidOutput::decode(&d), Some(a));
        for n in 2..d.len() {
            assert_eq!(HidOutput::decode(&d[..n]), None);
        }
    }

    #[test]
    fn pad_audio_datagram_roundtrip_and_truncation() {
        let opus = [0x5Au8; 61];
        let d = encode_pad_audio_datagram(3, PAD_AUDIO_KIND_HAPTICS, 42, 9_999, &opus);
        assert_eq!(d[0], PAD_AUDIO_MAGIC);
        assert_eq!(d.len(), 15 + opus.len());
        let f = decode_pad_audio_datagram(&d).unwrap();
        assert_eq!((f.pad, f.kind, f.seq, f.pts_ns), (3, 0, 42, 9_999));
        assert_eq!(f.opus, opus);
        for n in 0..15 {
            assert_eq!(decode_pad_audio_datagram(&d[..n]), None);
        }
        assert!(decode_audio_datagram(&d).is_none());
        assert!(decode_mic_datagram(&d).is_none());
        assert!(decode_pad_audio_datagram(&encode_audio_datagram(1, 2, &opus)).is_none());
        // Empty payload (DTX) is legal — header-only datagram.
        let hdr = encode_pad_audio_datagram(0, PAD_AUDIO_KIND_SPEAKER, 0, 0, &[]);
        assert_eq!(hdr.len(), 15);
        assert!(decode_pad_audio_datagram(&hdr).unwrap().opus.is_empty());
    }

    /// Pad is the only u16 index on 0xCD; consumers narrow with `as u8`. Out of range must
    /// not alias onto a real slot (wire pad 256 is slot 0).
    #[test]
    fn audio_ctl_rejects_a_pad_outside_the_index_space() {
        let ok = HidOutput::AudioCtl {
            pad: (crate::input::MAX_PADS - 1) as u16,
            flags: 0x12,
            raw: [1, 2, 3, 4, 5, 6],
        };
        assert_eq!(
            HidOutput::decode(&ok.encode()),
            Some(ok),
            "the last valid pad must still decode"
        );

        for pad in [crate::input::MAX_PADS as u16, 256, u16::MAX] {
            let d = HidOutput::AudioCtl {
                pad,
                flags: 0x12,
                raw: [1, 2, 3, 4, 5, 6],
            }
            .encode();
            assert_eq!(HidOutput::decode(&d), None, "pad {pad} must not decode");
        }

        // 256 as u8 == 0.
        let d = HidOutput::AudioCtl {
            pad: 256,
            flags: 0,
            raw: [0; 6],
        }
        .encode();
        assert!(
            !matches!(
                HidOutput::decode(&d),
                Some(HidOutput::AudioCtl { pad: 0, .. })
            ),
            "wire pad 256 must never surface as pad 0"
        );
    }

    #[test]
    fn cursor_state_roundtrip() {
        for (flags, x, y) in [
            (CURSOR_VISIBLE, 0i32, 0i32),
            (CURSOR_VISIBLE | CURSOR_RELATIVE_HINT, -5, 2160),
            (0, i32::MIN, i32::MAX),
        ] {
            let s = CursorState {
                serial: 42,
                flags,
                x,
                y,
            };
            let d = encode_cursor_state_datagram(&s);
            assert_eq!(decode_cursor_state_datagram(&d), Some(s));
            assert_eq!(s.visible(), flags & CURSOR_VISIBLE != 0);
            assert_eq!(s.relative_hint(), flags & CURSOR_RELATIVE_HINT != 0);
            // Append-extensible like 0xCF: a longer buffer still parses the known prefix.
            let mut ext = d.clone();
            ext.push(0xFF);
            assert_eq!(decode_cursor_state_datagram(&ext), Some(s));
            assert_eq!(decode_cursor_state_datagram(&d[..d.len() - 1]), None);
            let mut bad = d.clone();
            bad[0] = HOST_TIMING_MAGIC;
            assert_eq!(decode_cursor_state_datagram(&bad), None);
        }
    }

    /// A variant that ignored `with_pad` would deliver another pad's rumble to this one.
    #[test]
    fn with_pad_re_addresses_every_hid_output_variant() {
        let every = [
            HidOutput::Led {
                pad: 0,
                r: 1,
                g: 2,
                b: 3,
            },
            HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b101,
            },
            HidOutput::Trigger {
                pad: 0,
                which: 1,
                effect: vec![1, 2, 3],
            },
            HidOutput::TrackpadHaptic {
                pad: 0,
                side: 1,
                amplitude: 7,
                period: 8,
                count: 9,
            },
            HidOutput::HidRaw {
                pad: 0,
                kind: HID_RAW_OUTPUT,
                data: vec![0xAA, 0xBB],
            },
            HidOutput::AudioCtl {
                pad: 0,
                flags: 0b1,
                raw: [1, 2, 3, 4, 5, 6],
            },
        ];

        for ev in every {
            let before = ev.clone();
            let moved = ev.with_pad(9);
            assert_eq!(moved.pad(), 9, "{before:?} did not re-address");
            assert_eq!(moved.with_pad(0), before, "{before:?} lost a field");
        }
    }

    /// The up direction of the same rewrite: a variant that ignored `set_pad` would
    /// drive another pad's device with this one's reports.
    #[test]
    fn set_pad_re_addresses_every_rich_input_variant() {
        let every = [
            RichInput::Touchpad {
                pad: 0,
                finger: 1,
                active: true,
                x: 100,
                y: 200,
            },
            RichInput::Motion {
                pad: 0,
                gyro: [1, 2, 3],
                accel: [4, 5, 6],
            },
            RichInput::TouchpadEx {
                pad: 0,
                surface: 1,
                finger: 2,
                touch: true,
                click: false,
                x: -3,
                y: 4,
                pressure: 5,
            },
            RichInput::HidReport {
                pad: 0,
                len: 2,
                data: [0xAB; HID_REPORT_MAX],
            },
        ];

        for rich in every {
            let mut moved = rich;
            moved.set_pad(9);
            assert_eq!(moved.pad(), 9, "{rich:?} did not re-address");
            moved.set_pad(0);
            assert_eq!(moved, rich, "{rich:?} lost a field");
        }
    }
}
