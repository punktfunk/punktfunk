//! Decode GameStream input on the AES-GCM control stream ([`super::control`]) into
//! [`punktfunk_core::input::InputEvent`]s.
//!
//! Plaintext is `[u16 type LE][u16 length LE][NV_INPUT]`. Only type `0x0206` is input.
//! `NV_INPUT_HEADER` is `size` BE + `magic` LE; body fields are big-endian except `magic`
//! and keyboard `keyCode` (LE). Layouts match moonlight-common-c `Input.h`; magics match
//! Sunshine `input.cpp` Gen5+ (`scroll = 0x0A`, `controller = 0x0C`).

use punktfunk_core::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};
use punktfunk_core::input::{InputEvent, InputKind};

/// moonlight `packetTypesGen7[IDX_INPUT_DATA]`.
const INPUT_DATA_TYPE: u16 = 0x0206;

// Input.h magics. REL and scroll have Gen5+ replacements; both REL values are accepted.
const MAGIC_KEY_DOWN: u32 = 0x03;
const MAGIC_KEY_UP: u32 = 0x04;
const MAGIC_MOUSE_ABS: u32 = 0x05;
const MAGIC_MOUSE_REL: u32 = 0x06;
const MAGIC_MOUSE_REL_GEN5: u32 = 0x07;
const MAGIC_MOUSE_BTN_DOWN: u32 = 0x08;
const MAGIC_MOUSE_BTN_UP: u32 = 0x09;
const MAGIC_SCROLL_GEN5: u32 = 0x0A;
const MAGIC_UTF8: u32 = 0x17;
const MAGIC_HSCROLL: u32 = 0x5500_0001;
const MAGIC_SS_TOUCH: u32 = 0x5500_0002;
const MAGIC_SS_PEN: u32 = 0x5500_0003;

/// Scroll `code`: `1` = horizontal, `0` = vertical — shared by `MouseScroll` and
/// normalized `Scroll`.
pub const SCROLL_HORIZONTAL: u32 = 1;

/// Keepalives, QoS, gamepad, pen, and touch yield nothing (sibling decoders).
pub fn decode(plaintext: &[u8]) -> Vec<InputEvent> {
    if plaintext.len() < 4 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return Vec::new();
    }
    let p = &plaintext[4..];
    // UTF-8 expands to one `TextInput` per scalar — the only multi-event magic, so it
    // runs before the single-event dispatch.
    if p.len() >= 8 && u32::from_le_bytes([p[4], p[5], p[6], p[7]]) == MAGIC_UTF8 {
        // `size` is BE and excludes itself; it counts magic + body.
        let size = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) as usize;
        let body_len = size.saturating_sub(4).min(p.len() - 8);
        return match std::str::from_utf8(&p[8..8 + body_len]) {
            Ok(s) => s
                .chars()
                .filter(|c| !c.is_control())
                .map(|c| ev(InputKind::TextInput, c as u32, 0, 0, 0))
                .collect(),
            Err(_) => Vec::new(),
        };
    }
    decode_input_packet(p).into_iter().collect()
}

fn decode_input_packet(p: &[u8]) -> Option<InputEvent> {
    if p.len() < 8 {
        return None;
    }
    let magic = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let b = &p[8..];
    let be16 = |o: usize| -> Option<i16> { Some(i16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?])) };

    Some(match magic {
        MAGIC_MOUSE_REL | MAGIC_MOUSE_REL_GEN5 => {
            ev(InputKind::MouseMove, 0, be16(0)? as i32, be16(2)? as i32, 0)
        }
        MAGIC_MOUSE_ABS => {
            // x, y, unused, width, height (BE). Client extent `width<<16 | height` in `flags`
            // so the injector can scale.
            let (x, y) = (be16(0)? as i32, be16(2)? as i32);
            let flags = ((be16(6)? as u16 as u32) << 16) | (be16(8)? as u16 as u32);
            ev(InputKind::MouseMoveAbs, 0, x, y, flags)
        }
        MAGIC_MOUSE_BTN_DOWN => ev(InputKind::MouseButtonDown, *b.first()? as u32, 0, 0, 0),
        MAGIC_MOUSE_BTN_UP => ev(InputKind::MouseButtonUp, *b.first()? as u32, 0, 0, 0),
        // Wheel deltas arrive as signed v120 — normalized `Scroll` directly: this
        // path bypasses the client seam, so it speaks the injector vocabulary.
        MAGIC_SCROLL_GEN5 => scroll_ev(0, be16(0)?),
        MAGIC_HSCROLL => scroll_ev(SCROLL_HORIZONTAL, be16(0)?),
        MAGIC_KEY_DOWN | MAGIC_KEY_UP => {
            // keyCode is LE; Sunshine masks the 0x80 key-down high byte (`& 0xFF`). Moonlight
            // VKs are layout-semantic — tag them so Windows maps under the receiving layout,
            // not the US-positional table first-party clients use.
            let key_code = (u16::from_le_bytes([*b.get(1)?, *b.get(2)?]) & 0x00FF) as u32;
            let modifiers = *b.get(3)? as u32;
            let kind = if magic == MAGIC_KEY_DOWN {
                InputKind::KeyDown
            } else {
                InputKind::KeyUp
            };
            ev(
                kind,
                key_code,
                0,
                0,
                modifiers | crate::inject::KEY_FLAG_SEMANTIC_VK,
            )
        }
        // Gamepad/pen/touch: sibling decoders. Unknown magics drop in the control loop.
        _ => return None,
    })
}

/// Sunshine `SS_PEN_PACKET` (`Input.h`, LE, normalized floats). Unknowns: pressure `0`,
/// rotation `0xFFFF`, tilt `0xFF`. Contact vs hover for `pressure_or_distance` is in
/// [`super::pen`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsPen {
    pub event_type: u8,
    pub tool: u8,
    pub buttons: u8,
    pub x: f32,
    pub y: f32,
    pub pressure_or_distance: f32,
    pub rotation: u16,
    pub tilt: u8,
}

/// Sunshine `SS_TOUCH_PACKET`. Contact area is on the wire but has no native field; dropped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsTouch {
    pub event_type: u8,
    pub rotation: u16,
    pub pointer_id: u32,
    pub x: f32,
    pub y: f32,
    pub pressure_or_distance: f32,
}

/// Sunshine pointer event, sent only after `SS_FF_PEN_TOUCH_EVENTS` ([`super::rtsp`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SsPointer {
    Pen(SsPen),
    Touch(SsTouch),
}

/// True when the plaintext is a pointer magic, so a `decode_pointer` `None` is malformed
/// rather than some other message.
pub fn is_pointer_magic(plaintext: &[u8]) -> bool {
    plaintext.len() >= 12
        && u16::from_le_bytes([plaintext[0], plaintext[1]]) == INPUT_DATA_TYPE
        && matches!(
            u32::from_le_bytes([plaintext[8], plaintext[9], plaintext[10], plaintext[11]]),
            MAGIC_SS_TOUCH | MAGIC_SS_PEN
        )
}

/// Pen/touch event, or `None` for every other message (caller falls through to [`decode`]).
pub fn decode_pointer(plaintext: &[u8]) -> Option<SsPointer> {
    if plaintext.len() < 12 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return None;
    }
    let p = &plaintext[4..];
    let magic = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let b = &p[8..];
    // Finite only: a forged NaN would poison injector scaling.
    let f32at = |o: usize| -> Option<f32> {
        let v = f32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]);
        v.is_finite().then_some(v)
    };
    // Clients send NaN for "unknown"; the spec unknown is 0.0. Do not drop the packet.
    let f32_pressure = |o: usize| -> Option<f32> {
        let v = f32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]);
        Some(if v.is_finite() { v } else { 0.0 })
    };
    match magic {
        // Pad byte at 1 and contact areas after pressure; neither is stored.
        MAGIC_SS_TOUCH => Some(SsPointer::Touch(SsTouch {
            event_type: *b.first()?,
            rotation: u16::from_le_bytes([*b.get(2)?, *b.get(3)?]),
            pointer_id: u32::from_le_bytes([*b.get(4)?, *b.get(5)?, *b.get(6)?, *b.get(7)?]),
            x: f32at(8)?,
            y: f32at(12)?,
            pressure_or_distance: f32_pressure(16)?,
        })),
        // Pad bytes at 3 and 19, then contact areas; none are stored.
        MAGIC_SS_PEN => Some(SsPointer::Pen(SsPen {
            event_type: *b.first()?,
            tool: *b.get(1)?,
            buttons: *b.get(2)?,
            x: f32at(4)?,
            y: f32at(8)?,
            pressure_or_distance: f32_pressure(12)?,
            rotation: u16::from_le_bytes([*b.get(16)?, *b.get(17)?]),
            tilt: *b.get(18)?,
        })),
        _ => None,
    }
}

/// Signed packet v120 → normalized wheel event (Q24.8 delta; no phases on a wheel).
fn scroll_ev(axis: u32, v120: i16) -> InputEvent {
    ScrollEvent {
        source: ScrollSource::Wheel,
        phase: ScrollPhase::None,
        axis,
        delta: i32::from(v120) * SCROLL_SCALE as i32,
    }
    .to_event()
}

fn ev(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
    InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    }
}

const REPEAT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const REPEAT_PERIOD: std::time::Duration = std::time::Duration::from_millis(40);

/// Host-side key repeat. Moonlight drops its own auto-repeat and expects the host to repeat a
/// held key; `SendInput` never does, and Wayland apps repeat on their own. Sunshine's cadence:
/// the last key pressed repeats after 500 ms, then every 40 ms, until it is released.
pub struct KeyRepeat {
    enabled: bool,
    held: Option<(InputEvent, std::time::Instant)>,
}

impl KeyRepeat {
    /// On only where the injector never repeats (Windows).
    pub fn for_this_host() -> KeyRepeat {
        KeyRepeat {
            enabled: cfg!(target_os = "windows"),
            held: None,
        }
    }

    pub fn note(&mut self, ev: &InputEvent, now: std::time::Instant) {
        // The low byte is the VK, as the injectors read it.
        let same_key = |h: &(InputEvent, _)| h.0.code as u8 == ev.code as u8;
        match ev.kind {
            InputKind::KeyDown if self.enabled => self.held = Some((*ev, now + REPEAT_DELAY)),
            InputKind::KeyUp if self.held.as_ref().is_some_and(same_key) => self.held = None,
            _ => {}
        }
    }

    /// The key-down to inject again now, if one is due.
    pub fn due(&mut self, now: std::time::Instant) -> Option<InputEvent> {
        let (ev, next) = self.held.as_mut()?;
        if now < *next {
            return None;
        }
        *next = now + REPEAT_PERIOD;
        Some(*ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_key_repeats_after_the_delay_until_released() {
        let t0 = std::time::Instant::now();
        let key = |kind| ev(kind, 0x41, 0, 0, 0);
        let mut r = KeyRepeat {
            enabled: true,
            held: None,
        };
        r.note(&key(InputKind::KeyDown), t0);
        assert_eq!(r.due(t0 + REPEAT_DELAY / 2), None);
        assert_eq!(r.due(t0 + REPEAT_DELAY), Some(key(InputKind::KeyDown)));
        assert_eq!(r.due(t0 + REPEAT_DELAY + REPEAT_PERIOD / 2), None);
        assert!(r.due(t0 + REPEAT_DELAY + REPEAT_PERIOD).is_some());
        r.note(&key(InputKind::KeyUp), t0 + REPEAT_DELAY * 2);
        assert_eq!(r.due(t0 + REPEAT_DELAY * 3), None);

        let mut off = KeyRepeat {
            enabled: false,
            held: None,
        };
        off.note(&key(InputKind::KeyDown), t0);
        assert_eq!(
            off.due(t0 + REPEAT_DELAY * 3),
            None,
            "Linux apps repeat on their own"
        );
    }

    fn wrap(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut inp = Vec::new();
        inp.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes()); // size excludes itself
        inp.extend_from_slice(&magic.to_le_bytes());
        inp.extend_from_slice(body);
        let mut pt = Vec::new();
        pt.extend_from_slice(&INPUT_DATA_TYPE.to_le_bytes());
        pt.extend_from_slice(&(inp.len() as u16).to_le_bytes());
        pt.extend_from_slice(&inp);
        pt
    }

    #[test]
    fn decodes_relative_mouse() {
        let pt = wrap(MAGIC_MOUSE_REL_GEN5, &[0xff, 0xff, 0x00, 0x02]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::MouseMove);
        assert_eq!((ev[0].x, ev[0].y), (-1, 2));
    }

    #[test]
    fn decodes_scroll_packets_as_normalized_wheel() {
        // Vertical +120 → Scroll, Wheel/None, axis 0, Q24.8 delta.
        let pt = wrap(MAGIC_SCROLL_GEN5, &[0x00, 0x78]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0],
            ScrollEvent {
                source: ScrollSource::Wheel,
                phase: ScrollPhase::None,
                axis: 0,
                delta: 120 * SCROLL_SCALE as i32,
            }
            .to_event()
        );
        // Negative vertical and i16 extremes do not sign-wrap.
        let pt = wrap(MAGIC_SCROLL_GEN5, &[0x80, 0x00]); // -32768
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::Scroll);
        assert_eq!(ev[0].x, i32::from(-32768i16) * 256);
        // Horizontal keeps axis 1 and the sign convention.
        let pt = wrap(MAGIC_HSCROLL, &[0xff, 0x88]); // -120
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0],
            ScrollEvent {
                source: ScrollSource::Wheel,
                phase: ScrollPhase::None,
                axis: SCROLL_HORIZONTAL,
                delta: -120 * SCROLL_SCALE as i32,
            }
            .to_event()
        );
        // Truncated body (missing the delta) yields nothing, not a zero scroll.
        let pt = wrap(MAGIC_SCROLL_GEN5, &[0x00]);
        assert!(decode(&pt).is_empty());
        let pt = wrap(MAGIC_SCROLL_GEN5, &[]);
        assert!(decode(&pt).is_empty());
    }

    #[test]
    fn decodes_key_down_masking_high_byte() {
        let pt = wrap(MAGIC_KEY_DOWN, &[0x00, 0xa4, 0x80, 0x04, 0x00, 0x00]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::KeyDown);
        assert_eq!(ev[0].code, 0xA4);
        assert_eq!(ev[0].flags, 0x04 | crate::inject::KEY_FLAG_SEMANTIC_VK);
    }

    #[test]
    fn decodes_utf8_text_per_scalar() {
        let pt = wrap(MAGIC_UTF8, "aß😀".as_bytes());
        let ev = decode(&pt);
        assert_eq!(ev.len(), 3);
        assert!(ev.iter().all(|e| e.kind == InputKind::TextInput));
        assert_eq!(ev[0].code, 'a' as u32);
        assert_eq!(ev[1].code, 'ß' as u32);
        assert_eq!(ev[2].code, 0x1F600);
        // Invalid UTF-8 decodes to empty, not mojibake.
        let bad = wrap(MAGIC_UTF8, &[0xff, 0xfe]);
        assert!(decode(&bad).is_empty());
    }

    #[test]
    fn decodes_ss_pen_and_touch_golden_bytes() {
        // Contact-area floats are on the wire and ignored.
        let mut body = vec![0x01, 0x01, 0x01, 0x00];
        for f in [0.5f32, 0.25, 0.75] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        body.extend_from_slice(&180u16.to_le_bytes());
        body.extend_from_slice(&[45, 0x00]);
        for f in [0.0f32, 0.0] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        let pt = wrap(0x5500_0003, &body);
        assert_eq!(
            decode_pointer(&pt),
            Some(SsPointer::Pen(SsPen {
                event_type: 0x01,
                tool: 0x01,
                buttons: 0x01,
                x: 0.5,
                y: 0.25,
                pressure_or_distance: 0.75,
                rotation: 180,
                tilt: 45,
            }))
        );
        // Classic decoder must not misparse a pen packet as mouse/key.
        assert!(decode(&pt).is_empty());

        let mut body = vec![0x03, 0x00];
        body.extend_from_slice(&0xFFFFu16.to_le_bytes());
        body.extend_from_slice(&42u32.to_le_bytes());
        for f in [1.0f32, 0.0, 1.0, 0.0, 0.0] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        let pt = wrap(0x5500_0002, &body);
        assert_eq!(
            decode_pointer(&pt),
            Some(SsPointer::Touch(SsTouch {
                event_type: 0x03,
                rotation: 0xFFFF,
                pointer_id: 42,
                x: 1.0,
                y: 0.0,
                pressure_or_distance: 1.0,
            }))
        );

        assert_eq!(decode_pointer(&pt[..pt.len() - 18]), None);
        let mut nan = body.clone();
        nan[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(decode_pointer(&wrap(0x5500_0002, &nan)), None);
        // NaN pressureOrDistance → 0.0 (unknown). Dropping the packet would kill real clients.
        let mut nan_pod = body.clone();
        nan_pod[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        match decode_pointer(&wrap(0x5500_0002, &nan_pod)) {
            Some(SsPointer::Touch(t)) => assert_eq!(t.pressure_or_distance, 0.0),
            other => panic!("NaN pressure must decode with pod=0.0, got {other:?}"),
        }
        assert_eq!(
            decode_pointer(&wrap(MAGIC_MOUSE_REL_GEN5, &[0, 0, 0, 0])),
            None
        );
    }

    #[test]
    fn ignores_non_input_type() {
        let mut pt = vec![0x00, 0x02]; // keepalive type 0x0200
        pt.extend_from_slice(&[0x08, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode(&pt).is_empty());
    }
}
