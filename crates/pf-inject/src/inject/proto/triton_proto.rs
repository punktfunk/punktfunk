//! Transport-independent Steam Controller 2 (Valve "Ibex" / SDL "Triton", wired `28DE:1302`)
//! contract — the as-is passthrough sibling of [`super::steam_proto`].
//!
//! The client captures the physical pad and forwards raw input reports
//! ([`RichInput::HidReport`](punktfunk_core::quic::RichInput)); the host mirrors them unchanged.
//! Host hidraw writes (lizard-off / IMU-enable features, `0x80` rumble) go back as
//! [`HidOutput::HidRaw`](punktfunk_core::quic::HidOutput). Steam Input drives the hidraw as it
//! would a physical pad; from kernel 7.3 `hid-steam` binds this PID too (#1368).
//!
//! Ground truth: SDL `SDL_hidapi_steam_triton.c` + `steam/controller_structs.h`. Input ids
//! `0x42`/`0x45` (`TritonMTUNoQuat_t`, 46 bytes with id), `0x47` (trackpad timestamp), `0x43`
//! battery, `0x46`/`0x79` wireless. Feature reports are 64 bytes on id `1`; haptics are OUTPUT
//! `0x80..=0x85`.
//!
//! A typed fallback covers a client that declared the kind but sends no raw feed: ordinary
//! gamepad plane → a minimal `0x42` state report. The first raw report switches to as-is
//! permanently.

use punktfunk_core::input::{gamepad as gs, GamepadFrame};
use punktfunk_core::quic::RichInput;

/// Same as [`super::steam_proto::STEAM_VENDOR`]; repeated so this module stays self-contained.
pub const TRITON_VENDOR: u32 = 0x28DE;
/// BLE `0x1303` and Puck `0x1304`/`0x1305` are client transports only; Steam keys the pad on this PID.
pub const TRITON_WIRED_PRODUCT: u32 = 0x1302;

/// SDL `ETritonReportIDTypes` (`controller_structs.h`).
pub const ID_TRITON_CONTROLLER_STATE: u8 = 0x42;
pub const ID_TRITON_BATTERY_STATUS: u8 = 0x43;
pub const ID_TRITON_CONTROLLER_STATE_BLE: u8 = 0x45;
pub const ID_TRITON_CONTROLLER_STATE_TIMESTAMP: u8 = 0x47;

/// Only rumble is parsed for the 0xCA plane; every output report is still forwarded raw.
pub const ID_OUT_REPORT_HAPTIC_RUMBLE: u8 = 0x80;

/// HID report buffer (feature GET/SET is 64). State `0x42` is [`TRITON_STATE_LEN`] (id + 53).
pub const TRITON_REPORT_LEN: usize = 64;
pub const TRITON_STATE_LEN: usize = 54;

/// Numbered reports are the protocol: inputs `0x40`–`0x45`/`0x79`/`0x7B`, outputs `0x80`–`0x89`,
/// features `1` and `2`. Puck bond queries use feature 2; an unnumbered descriptor makes hidraw
/// frame them wrong and Steam closes the device. `&[u8]` (not `[u8; 372]`) so usbip/uhid can
/// `copy_from_slice`; the bytes live in [`pf_driver_proto::triton::RDESC`].
pub const TRITON_RDESC: &[u8] = &pf_driver_proto::triton::RDESC;

/// SDL `TritonButtons`. Only the bits the typed fallback synthesizes; the raw path carries the rest.
/// SDL's enum swaps the View and Menu names; its mapping and hid-steam agree with these values.
pub mod tbtn {
    pub const A: u32 = 0x0000_0001;
    pub const B: u32 = 0x0000_0002;
    pub const X: u32 = 0x0000_0004;
    pub const Y: u32 = 0x0000_0008;
    pub const QAM: u32 = 0x0000_0010;
    pub const R3: u32 = 0x0000_0020;
    pub const MENU: u32 = 0x0000_0040;
    pub const R4: u32 = 0x0000_0080;
    pub const R5: u32 = 0x0000_0100;
    pub const RB: u32 = 0x0000_0200;
    pub const DPAD_DOWN: u32 = 0x0000_0400;
    pub const DPAD_RIGHT: u32 = 0x0000_0800;
    pub const DPAD_LEFT: u32 = 0x0000_1000;
    pub const DPAD_UP: u32 = 0x0000_2000;
    pub const VIEW: u32 = 0x0000_4000;
    pub const L3: u32 = 0x0000_8000;
    pub const STEAM: u32 = 0x0001_0000;
    pub const L4: u32 = 0x0002_0000;
    pub const L5: u32 = 0x0004_0000;
    pub const LB: u32 = 0x0008_0000;
    pub const RPAD_TOUCH: u32 = 0x0020_0000;
    pub const RPAD_CLICK: u32 = 0x0040_0000;
    pub const RT_CLICK: u32 = 0x0080_0000;
    pub const LPAD_TOUCH: u32 = 0x0200_0000;
    pub const LPAD_CLICK: u32 = 0x0400_0000;
    pub const LT_CLICK: u32 = 0x0800_0000;
}

/// As-is when `raw_len > 0` (raw report is the state). Typed fields feed the fallback until then.
#[derive(Clone, Copy)]
pub struct TritonState {
    pub raw: [u8; TRITON_REPORT_LEN],
    pub raw_len: u8,
    pub buttons: u32,
    pub lt: u16,
    pub rt: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

impl TritonState {
    pub fn neutral() -> TritonState {
        TritonState {
            raw: [0u8; TRITON_REPORT_LEN],
            raw_len: 0,
            buttons: 0,
            lt: 0,
            rt: 0,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
        }
    }

    /// Deck paddle order: PADDLE1/2/3/4 → R4/L4/R5/L5, MISC1 → QAM, DualSense touchpad-click →
    /// right-pad click. Sticks already +y up; triggers 0..255 → 0..32767.
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> TritonState {
        let on = |bit: u32| buttons & bit != 0;
        let trig = |v: u8| ((v as u32 * 32767) / 255) as u16;
        let mut b = 0u32;
        let set = |b: &mut u32, on: bool, m: u32| {
            if on {
                *b |= m;
            }
        };
        set(&mut b, on(gs::BTN_A), tbtn::A);
        set(&mut b, on(gs::BTN_B), tbtn::B);
        set(&mut b, on(gs::BTN_X), tbtn::X);
        set(&mut b, on(gs::BTN_Y), tbtn::Y);
        set(&mut b, on(gs::BTN_LB), tbtn::LB);
        set(&mut b, on(gs::BTN_RB), tbtn::RB);
        set(&mut b, on(gs::BTN_BACK), tbtn::VIEW);
        set(&mut b, on(gs::BTN_START), tbtn::MENU);
        set(&mut b, on(gs::BTN_GUIDE), tbtn::STEAM);
        set(&mut b, on(gs::BTN_LS_CLICK), tbtn::L3);
        set(&mut b, on(gs::BTN_RS_CLICK), tbtn::R3);
        set(&mut b, on(gs::BTN_DPAD_UP), tbtn::DPAD_UP);
        set(&mut b, on(gs::BTN_DPAD_DOWN), tbtn::DPAD_DOWN);
        set(&mut b, on(gs::BTN_DPAD_LEFT), tbtn::DPAD_LEFT);
        set(&mut b, on(gs::BTN_DPAD_RIGHT), tbtn::DPAD_RIGHT);
        set(&mut b, on(gs::BTN_TOUCHPAD), tbtn::RPAD_CLICK);
        set(&mut b, on(gs::BTN_PADDLE1), tbtn::R4);
        set(&mut b, on(gs::BTN_PADDLE2), tbtn::L4);
        set(&mut b, on(gs::BTN_PADDLE3), tbtn::R5);
        set(&mut b, on(gs::BTN_PADDLE4), tbtn::L5);
        set(&mut b, on(gs::BTN_MISC1), tbtn::QAM);
        // 240 ≈ a hard pull, not first contact — the physical pad's LT_CLICK/RT_CLICK threshold.
        set(&mut b, lt >= 240, tbtn::LT_CLICK);
        set(&mut b, rt >= 240, tbtn::RT_CLICK);
        TritonState {
            raw: [0u8; TRITON_REPORT_LEN],
            raw_len: 0,
            buttons: b,
            lt: trig(lt),
            rt: trig(rt),
            lx,
            ly,
            rx,
            ry,
        }
    }

    /// Typed fallback over `prev`. As-is mode is sticky: a typed frame between two raw reports
    /// must not flap the pad back to synth (the client sends both planes).
    pub fn merge_frame(prev: &TritonState, f: &GamepadFrame) -> TritonState {
        TritonState {
            raw: prev.raw,
            raw_len: prev.raw_len,
            ..TritonState::from_gamepad(
                f.buttons,
                f.ls_x,
                f.ls_y,
                f.rs_x,
                f.rs_y,
                f.left_trigger,
                f.right_trigger,
            )
        }
    }

    /// The input report to present and its length: the client's raw report as-is, else a
    /// synthesized `0x42` stamped with the next `seq`. The raw path carries its own sequence.
    pub fn report(&self, seq: &mut u8) -> ([u8; TRITON_REPORT_LEN], usize) {
        let mut r = [0u8; TRITON_REPORT_LEN];
        if self.raw_len > 0 {
            let len = (self.raw_len as usize).min(TRITON_REPORT_LEN);
            r[..len].copy_from_slice(&self.raw[..len]);
            return (r, len);
        }
        *seq = seq.wrapping_add(1);
        let mut state = [0u8; TRITON_STATE_LEN];
        serialize_triton_state(&mut state, self, *seq);
        r[..TRITON_STATE_LEN].copy_from_slice(&state);
        (r, TRITON_STATE_LEN)
    }

    /// A raw report from the client's physical pad becomes the state. Touchpad and motion have
    /// nothing to fold: the raw feed carries pads + IMU, and the synth fallback has no surface.
    pub fn apply_rich(&mut self, rich: RichInput) {
        if let RichInput::HidReport { len, data, .. } = rich {
            let len = (len as usize).min(data.len()).min(self.raw.len());
            if len == 0 {
                return;
            }
            self.raw[..len].copy_from_slice(&data[..len]);
            self.raw_len = len as u8;
        }
    }
}

/// Fallback `0x42` (`TritonMTUNoQuat_t`). Pads and IMU stay zero: no raw feed, and Steam ignores
/// IMU until `SETTING_IMU_MODE` on a real report anyway.
pub fn serialize_triton_state(buf: &mut [u8; TRITON_STATE_LEN], st: &TritonState, seq: u8) {
    buf.fill(0);
    buf[0] = ID_TRITON_CONTROLLER_STATE;
    buf[1] = seq;
    buf[2..6].copy_from_slice(&st.buttons.to_le_bytes());
    buf[6..8].copy_from_slice(&(st.lt as i16).to_le_bytes());
    buf[8..10].copy_from_slice(&(st.rt as i16).to_le_bytes());
    buf[10..12].copy_from_slice(&st.lx.to_le_bytes());
    buf[12..14].copy_from_slice(&st.ly.to_le_bytes());
    buf[14..16].copy_from_slice(&st.rx.to_le_bytes());
    buf[16..18].copy_from_slice(&st.ry.to_le_bytes());
}

/// Raw reports for [`HidOutput::HidRaw`](punktfunk_core::quic::HidOutput), plus 0xCA rumble for
/// phone-mirror clients whose physical pad already got the raw report.
#[derive(Default)]
pub struct TritonFeedback {
    /// Last `0x80` `(left.speed, right.speed)` as `(low, high)`.
    pub rumble: Option<(u16, u16)>,
    /// Kind is `HID_RAW_OUTPUT` or `HID_RAW_FEATURE`.
    pub raw: Vec<(u8, Vec<u8>)>,
}

/// `MsgHapticRumble`: `[0x80][type][intensity u16][left.speed u16][left.gain i8][right.speed u16][right.gain i8]`.
/// Returns `(left.speed, right.speed)` — offsets 4 and 7 skip the intervening i8 gain.
pub fn parse_triton_rumble(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 10 || data[0] != ID_OUT_REPORT_HAPTIC_RUMBLE {
        return None;
    }
    let le = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    Some((le(4), le(7)))
}

/// Triton ids are all non-zero, so a leading `0x00` is hidraw's synthetic report-id on an
/// unnumbered descriptor, not a command.
pub fn strip_report_prefix(data: &[u8]) -> &[u8] {
    match data {
        [0, rest @ ..] if !rest.is_empty() => rest,
        d => d,
    }
}

/// `'T','R','I'` + index, stamped into the fake `0x83` attributes.
pub fn triton_unit_id(index: u8) -> u32 {
    pf_driver_proto::triton::unit_id(index)
}

/// `FVPF…` (`HID_UNIQ`) marks a virtual pad so a concurrent session never treats it as hardware.
/// 13 chars like real `FXA…`. UHID and usbip identity + `0xAE` replies must match.
pub fn triton_serial(index: u8) -> String {
    format!("FVPF1302{index:02}D03")
}

/// GET_REPORT answer: the command byte must echo the last SET (`0x83` / `0xAE`) or Steam never
/// adopts the pad. Frame is feature id **1** (`[0x01][cmd][len][payload…]`); the `0x83` blob
/// carries this PID. `last_set` may be id-first or already stripped (`cmd ≥ 0x80`).
pub fn triton_feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; 64] {
    pf_driver_proto::triton::feature_reply(last_set, serial, unit_id)
}

/// A real pad's identity: its USB serial and its replies to Steam's feature queries, as its
/// client read them. The virtual pad answers with these before the canned table, so Steam sees
/// the controller it knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sc2Identity {
    pub serial: Option<String>,
    /// `(SET frame, GET reply)`, both id-first.
    pub replies: Vec<(Vec<u8>, [u8; 64])>,
    /// A Puck pad's slot, 0–3.
    pub slot: u8,
}

impl Sc2Identity {
    /// The recorded reply to the request `last_set` makes. `0xB4` is the slot's live state,
    /// so it is never replayed.
    pub fn reply(&self, last_set: &[u8]) -> Option<[u8; 64]> {
        pf_driver_proto::triton::recorded_reply(self.pairs(), last_set)
    }

    fn pairs(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.replies
            .iter()
            .map(|(q, r)| (q.as_slice(), r.as_slice()))
    }

    /// The devnode property a Windows virtual SC2 reads this from.
    pub fn blob(&self) -> Vec<u8> {
        pf_driver_proto::triton::identity_blob(self.serial.as_deref().unwrap_or(""), self.pairs())
    }

    /// What a client sent ([`punktfunk_core::quic::PadIdentity`]); `None` when it carries
    /// neither a serial nor a reply, or the replies are torn.
    pub fn from_wire(id: &punktfunk_core::quic::PadIdentity) -> Option<Sc2Identity> {
        let replies: Vec<_> = punktfunk_core::quic::unpack_identity_replies(&id.replies)?
            .into_iter()
            .filter(|(_, rep)| !rep.is_empty())
            .map(|(req, rep)| {
                let mut reply = [0u8; 64];
                reply[..rep.len()].copy_from_slice(&rep);
                (req, reply)
            })
            .collect();
        let serial = (!id.serial.is_empty()).then(|| id.serial.clone());
        (serial.is_some() || !replies.is_empty()).then_some(Sc2Identity {
            serial,
            replies,
            slot: id.slot,
        })
    }
}

/// Identities for the virtual SC2s about to be built, by OS pad slot.
static STAGED: std::sync::Mutex<std::collections::BTreeMap<u8, std::sync::Arc<Sc2Identity>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The identity the next virtual SC2 on OS slot `slot` takes; `None` clears it.
pub fn stage_identity(slot: u8, id: Option<std::sync::Arc<Sc2Identity>>) {
    let mut staged = STAGED.lock().unwrap_or_else(|e| e.into_inner());
    match id {
        Some(id) => staged.insert(slot, id),
        None => staged.remove(&slot),
    };
}

/// What a virtual SC2 on `slot` answers as; `None` keeps the canned identity.
pub fn identity_for(slot: u8) -> Option<std::sync::Arc<Sc2Identity>> {
    STAGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&slot)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw report goes out as-is and leaves the synth sequence alone; the typed fallback
    /// bumps it.
    #[test]
    fn report_mirrors_raw_else_synthesizes_with_the_next_seq() {
        let mut seq = 7;
        let (r, len) = TritonState::neutral().report(&mut seq);
        assert_eq!(
            (len, r[0], r[1], seq),
            (TRITON_STATE_LEN, ID_TRITON_CONTROLLER_STATE, 8, 8)
        );

        let mut st = TritonState::neutral();
        st.raw[..3].copy_from_slice(&[0x45, 0x11, 0x22]);
        st.raw_len = 3;
        let (r, len) = st.report(&mut seq);
        assert_eq!((len, &r[..3], seq), (3, &[0x45, 0x11, 0x22][..], 8));
    }

    #[test]
    fn a_sent_identity_becomes_the_pads_replies() {
        use punktfunk_core::quic::{pack_identity_replies, PadIdentity};
        let sent = PadIdentity {
            pad: 1,
            serial: "FXA0000000001".into(),
            replies: pack_identity_replies([
                (&[0x01, 0x83, 0x00][..], &[0x01, 0x83, 0x1E][..]),
                (&[0x01, 0xAE, 0x15, 0x02][..], &[][..]),
            ]),
            slot: 1,
        };
        let id = Sc2Identity::from_wire(&sent).unwrap();
        assert_eq!(id.serial.as_deref(), Some("FXA0000000001"));
        assert_eq!(id.replies.len(), 1, "an unanswered query is dropped");
        assert_eq!(id.reply(&[0x01, 0x83, 0x00]).unwrap()[2], 0x1E);
        assert!(Sc2Identity::from_wire(&PadIdentity::default()).is_none());

        stage_identity(201, Some(std::sync::Arc::new(id)));
        assert!(identity_for(201).is_some());
        stage_identity(201, None);
    }

    #[test]
    fn a_recorded_identity_answers_by_request_and_never_replays_slot_state() {
        let pad = |b: &[u8]| {
            let mut r = [0u8; 64];
            r[..b.len()].copy_from_slice(b);
            r
        };
        let id = Sc2Identity {
            serial: Some("FXA0000000001".into()),
            replies: vec![
                (vec![0x01, 0x83, 0x00], pad(&[0x01, 0x83, 0x1E, 0x01, 0x02])),
                (
                    vec![0x01, 0xAE, 0x15, 0x01],
                    pad(&[0x01, 0xAE, 0x14, 0x01, 0x46, 0x58, 0x41]),
                ),
                (vec![0x02, 0xB4], pad(&[0x02, 0xB4, 0x01, 0x02])),
            ],
            slot: 0,
        };
        // Steam's own length byte differs from the capture's; the attribute picks the reply.
        let r = id.reply(&[0x01, 0xAE, 0x14, 0x01]).unwrap();
        assert_eq!(&r[..7], &[0x01, 0xAE, 0x14, 0x01, 0x46, 0x58, 0x41]);
        assert_eq!(id.reply(&[0x01, 0x83, 0x00]).unwrap()[2], 0x1E);
        assert!(
            id.reply(&[0x01, 0xAE, 0x15, 0x00]).is_none(),
            "unrecorded attribute"
        );
        assert!(id.reply(&[0x02, 0xB4]).is_none(), "slot state stays live");
    }

    /// `clients/shared/sc2-vectors.json` `buttons`: the Swift and Kotlin wire maps read the same
    /// rows in the other direction.
    #[test]
    fn fallback_buttons_match_the_shared_vectors() {
        let raw = include_str!("../../../../../clients/shared/sc2-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        for row in file["buttons"].as_array().expect("buttons") {
            let wire = row["wire"].as_u64().unwrap() as u32;
            let st = TritonState::from_gamepad(wire, 0, 0, 0, 0, 0, 0);
            assert_eq!(
                u64::from(st.buttons),
                row["sc2"].as_u64().unwrap(),
                "{}",
                row["name"]
            );
        }
    }

    #[test]
    fn fallback_state_serializes_sdl_layout() {
        let st = TritonState::from_gamepad(
            gs::BTN_A | gs::BTN_START | gs::BTN_PADDLE1 | gs::BTN_MISC1,
            1000,
            -2000,
            3000,
            -32768,
            255,
            0,
        );
        assert_eq!(
            st.buttons,
            tbtn::A | tbtn::MENU | tbtn::R4 | tbtn::QAM | tbtn::LT_CLICK
        );
        assert_eq!(st.lt, 32767); // exact full-scale, not the *128 approximation
        let mut r = [0u8; TRITON_STATE_LEN];
        serialize_triton_state(&mut r, &st, 7);
        assert_eq!(r[0], ID_TRITON_CONTROLLER_STATE);
        assert_eq!(r[1], 7);
        assert_eq!(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), st.buttons);
        assert_eq!(i16::from_le_bytes([r[6], r[7]]), 32767); // sTriggerLeft
        assert_eq!(i16::from_le_bytes([r[10], r[11]]), 1000); // sLeftStickX
        assert_eq!(i16::from_le_bytes([r[16], r[17]]), -32768); // sRightStickY
        assert!(r[18..].iter().all(|&b| b == 0)); // pads + IMU zero
    }

    #[test]
    fn rumble_output_report_parses() {
        // [0x80, type, intensity(2), left.speed(2), left.gain, right.speed(2), right.gain]
        let mut d = [0u8; 10];
        d[0] = ID_OUT_REPORT_HAPTIC_RUMBLE;
        d[4..6].copy_from_slice(&0x1234u16.to_le_bytes());
        d[7..9].copy_from_slice(&0x5678u16.to_le_bytes());
        assert_eq!(parse_triton_rumble(&d), Some((0x1234, 0x5678)));
        d[0] = 0x81; // haptic pulse — not rumble
        assert_eq!(parse_triton_rumble(&d), None);
        assert_eq!(parse_triton_rumble(&d[..8]), None);
    }

    #[test]
    fn report_prefix_strips_only_leading_zero() {
        assert_eq!(strip_report_prefix(&[0x00, 0x80, 1, 2]), &[0x80, 1, 2]);
        assert_eq!(strip_report_prefix(&[0x80, 1, 2]), &[0x80, 1, 2]);
        assert_eq!(strip_report_prefix(&[0x01, 0x87]), &[0x01, 0x87]); // feature id 1 kept
        assert_eq!(strip_report_prefix(&[0x00]), &[0x00]); // lone zero: nothing to strip to
    }

    #[test]
    fn feature_reply_echoes_the_queried_command() {
        let serial = triton_serial(0);
        let uid = triton_unit_id(0);
        // 0x83 attributes: id-first frame, product id = 0x1302 in the first block.
        let r = triton_feature_reply(&[0x01, 0x83, 0x00], &serial, uid);
        assert_eq!(&r[..3], &[0x01, 0x83, 0x19]);
        assert_eq!(r[3], 0x01); // ATTRIB product-id tag
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            TRITON_WIRED_PRODUCT
        );
        // 0xAE serial: attribute id + padded string, 20-byte payload.
        let r = triton_feature_reply(&[0x01, 0xAE, 0x01, 0x01], &serial, uid);
        assert_eq!(&r[..3], &[0x01, 0xAE, 0x14]);
        assert_eq!(r[3], 0x01);
        assert_eq!(&r[4..4 + serial.len()], serial.as_bytes());
        let r = triton_feature_reply(&[0x83u8, 0x00], &serial, uid);
        assert_eq!(&r[..3], &[0x01, 0x83, 0x19]);
        // 0x87 settings write reads back as an echo.
        let r = triton_feature_reply(&[0x01, 0x87, 3, 9, 0, 0], &serial, uid);
        assert_eq!(&r[..6], &[0x01, 0x87, 3, 9, 0, 0]);
    }

    /// Indices are < 100; the no_std helper wraps mod 100 where `format!` would grow to 3 digits.
    #[test]
    fn serial_string_matches_the_no_std_bytes() {
        let mut b = [0u8; 13];
        pf_driver_proto::triton::serial(7, &mut b);
        assert_eq!(triton_serial(7).as_bytes(), &b);
    }
}
