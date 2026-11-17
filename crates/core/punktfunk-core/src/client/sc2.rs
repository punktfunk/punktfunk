//! A Steam Controller's raw reports on their way to the host, either generation. Every client's
//! capture hands them to [`NativeClient::send_rich_input`](super::NativeClient::send_rich_input),
//! and one [`Filter`] per pad decides what the host sees: wireless status stays local, the ring
//! chord and a masked pad stay off the wire, Steam and QAM follow the gate, and a frozen IMU
//! block goes out zeroed. Raw HID reports come only from these captures, so a report's first
//! bytes alone select the rules ([`Layout`]).

/// SDL `ETritonReportIDTypes`.
const ID_STATE: u8 = 0x42;
const ID_STATE_BLE: u8 = 0x45;
const ID_WIRELESS_X: u8 = 0x46;
const ID_STATE_TIMESTAMP: u8 = 0x47;
const ID_WIRELESS: u8 = 0x79;

/// SDL `TritonButtons` bits the gate acts on. SDL's enum calls `0x4000` MENU, but drives BACK
/// from it, as hid-steam does (`clients/shared/sc2-vectors.json` `buttons`).
const BTN_A: u32 = 0x0000_0001;
const BTN_QAM: u32 = 0x0000_0010;
const BTN_SELECT: u32 = 0x0000_4000;
const BTN_STEAM: u32 = 0x0001_0000;

/// The 2015 controller's frames carry no report id: a version word of 1, then the type
/// (SDL `ValveInReportHeader_t`).
const CLASSIC_STATE: u8 = 0x01;
const CLASSIC_WIRELESS: u8 = 0x03;

/// Where a state report keeps its buttons, and the bits the gate acts on.
struct Layout {
    at: usize,
    len: usize,
    a: u32,
    select: u32,
    /// Steam, and QAM where the pad has one.
    system: u32,
    /// Bits the gate may hold back. The rest say what the report's other fields mean.
    buttons: u32,
}

const TRITON: Layout = Layout {
    at: 2,
    len: 4,
    a: BTN_A,
    select: BTN_SELECT,
    system: BTN_STEAM | BTN_QAM,
    buttons: u32::MAX,
};

/// `ID_CONTROLLER_STATE`: 24 bits at 8..11. Bit 19 (left touch) says 16..20 is the left pad, not
/// the stick; bit 20 is the right touch; bit 23 says the two interleave. Held back, a resting
/// thumb would read as a pushed stick.
const CLASSIC: Layout = Layout {
    at: 8,
    len: 3,
    a: 0x80,
    select: 0x1000,
    system: 0x2000,
    buttons: 0x47_FFFF,
};

impl Layout {
    /// The layout of state report `r`; `None` for a report the gate leaves alone.
    fn of(r: &[u8]) -> Option<&'static Layout> {
        match r {
            [ID_STATE | ID_STATE_BLE | ID_STATE_TIMESTAMP, ..] if r.len() >= 6 => Some(&TRITON),
            [0x01, 0x00, CLASSIC_STATE, ..] if r.len() >= 11 => Some(&CLASSIC),
            _ => None,
        }
    }

    fn read(&self, r: &[u8]) -> u32 {
        let mut b = [0u8; 4];
        b[..self.len].copy_from_slice(&r[self.at..self.at + self.len]);
        u32::from_le_bytes(b)
    }
}

/// The feature queries a 2015 controller's identity is made of, id first (its reports have no
/// id, so it reads 0): the attributes, the board serial, the unit serial.
const CLASSIC_REQUESTS: &[&[u8]] = &[
    &[0x00, 0x83, 0x00],
    &[0x00, 0xAE, 0x15, 0x00],
    &[0x00, 0xAE, 0x15, 0x01],
];

/// What a client reads off a 2015 Steam Controller, as [`identity_requests`] for the newer pad.
pub fn classic_identity_requests() -> impl Iterator<Item = &'static [u8]> {
    CLASSIC_REQUESTS.iter().copied()
}

/// Wire pads a client can address: every `pad` masks to 16.
pub(crate) const PADS: usize = 16;

/// Feature queries whose replies make a pad's identity, id first: the attributes, the four
/// strings, the three firmware blocks. Steam asks the same; the slot state (`0xB4`) is live
/// and stays out.
const PAD_REQUESTS: &[&[u8]] = &[
    &[0x01, 0x83, 0x00],
    &[0x01, 0xAE, 0x15, 0x00],
    &[0x01, 0xAE, 0x15, 0x01],
    &[0x01, 0xAE, 0x15, 0x02],
    &[0x01, 0xAE, 0x15, 0x03],
    &[0x01, 0xF2, 0x01, 0x00],
    &[0x01, 0xF2, 0x01, 0x01],
    &[0x01, 0xF2, 0x01, 0x02],
];
/// A Puck slot adds the pad's two config keys and the dongle's attributes and bond.
const PUCK_REQUESTS: &[&[u8]] = &[
    b"\x01\xED\x17user/wireless_transport",
    b"\x01\xED\x08esb/bond",
    &[0x02, 0x83, 0x00],
    &[0x02, 0xA3, 0x00],
];

/// What a client reads off a pad (SET the request, GET the reply) for
/// [`NativeClient::send_pad_identity`](super::NativeClient::send_pad_identity). A Puck stalls
/// each GET until the pad answers over the radio, about 40 ms; retry for a second.
pub fn identity_requests(puck: bool) -> impl Iterator<Item = &'static [u8]> {
    let extra: &[&[u8]] = if puck { PUCK_REQUESTS } else { &[] };
    PAD_REQUESTS.iter().chain(extra).copied()
}

/// [`Sc2Gate::from_bits`]: the C ABI and JNI spelling of a gate.
pub const SC2_GATE_MASKED: u32 = 1;
pub const SC2_GATE_SYSTEM_LOCAL: u32 = 2;
pub const SC2_GATE_CHORDS: u32 = 4;

/// How a client wants one pad's raw reports gated. The default forwards everything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sc2Gate {
    /// The client's overlay owns the pad: state reports go out neutral, and a button held now
    /// stays off the wire until it is released.
    pub masked: bool,
    /// Steam and QAM stay with the client.
    pub system_local: bool,
    /// The client opens its ring on Select then A, so that chord stays off the wire.
    pub chords: bool,
}

impl Sc2Gate {
    /// Unknown bits are ignored.
    pub fn from_bits(bits: u32) -> Self {
        Sc2Gate {
            masked: bits & SC2_GATE_MASKED != 0,
            system_local: bits & SC2_GATE_SYSTEM_LOCAL != 0,
            chords: bits & SC2_GATE_CHORDS != 0,
        }
    }
}

/// One wire pad's gate and the state its rules carry from report to report.
#[derive(Default)]
pub(crate) struct Filter {
    pub(crate) gate: Sc2Gate,
    ring: RingGate,
    imu: ImuGate,
}

/// Whether a state report has a button held or a pad touched, for the keep-awake rule. A raw
/// pad's buttons ride no other event; its IMU fills every report, held or not.
pub(crate) fn holds_button(r: &[u8]) -> bool {
    Layout::of(r).is_some_and(|l| l.read(r) != 0)
}

impl Filter {
    /// Gate one report in place. False for a report that stays off the wire: the host keeps its
    /// own connect edges, a Puck's and a 2015 dongle's alike.
    pub(crate) fn apply(&mut self, r: &mut [u8]) -> bool {
        if matches!(
            r,
            [] | [ID_WIRELESS | ID_WIRELESS_X, ..] | [0x01, 0x00, CLASSIC_WIRELESS, ..]
        ) {
            return false;
        }
        if let Some(l) = Layout::of(r) {
            let gate = self.gate;
            let held = l.read(r);
            let mut b = self.ring.apply(held & l.buttons, gate, l) | (held & !l.buttons);
            if gate.masked {
                r[l.at..].fill(0);
            } else {
                if gate.system_local {
                    b &= !l.system;
                }
                r[l.at..l.at + l.len].copy_from_slice(&b.to_le_bytes()[..l.len]);
            }
        }
        self.imu.apply(r);
        true
    }

    /// A new pad on this index: drop the old one's held buttons and IMU clock, keep the gate.
    pub(crate) fn reset(&mut self) {
        self.ring = RingGate::default();
        self.imu = ImuGate::default();
    }
}

/// Buttons held off the wire until the hardware releases them: the ring chord, decided here
/// because a client's own mask lands reports later, and anything held while masked. Their
/// presses never went out, so their releases must not either.
#[derive(Default)]
struct RingGate {
    held: u32,
    swallow: u32,
}

impl RingGate {
    /// What is left of `buttons` for the host. Select first, then A, as on the typed plane.
    fn apply(&mut self, buttons: u32, gate: Sc2Gate, l: &Layout) -> u32 {
        let was = std::mem::replace(&mut self.held, buttons);
        self.swallow &= buttons;
        if gate.masked {
            self.swallow |= buttons;
        } else if gate.chords && buttons & !was & l.a != 0 && was & l.select != 0 {
            self.swallow |= l.a | l.select;
        }
        buttons & !self.swallow
    }
}

/// The pad streams IMU only after Steam writes `SETTING_IMU_MODE`. Until then the block and its
/// timestamp are a frozen resting sample, which Steam's desktop gyro-mouse reads as constant
/// rotation. Pass it only while the timestamp moves; `0x47` diverges from byte 18 and passes.
#[derive(Default)]
struct ImuGate {
    last: u32,
    seen: bool,
    stale: u8,
}

impl ImuGate {
    /// `TritonMTUNoQuat_t.imu` (struct offset 29 + id byte): u32 timestamp, 3× accel, 3× gyro.
    const OFFSET: usize = 30;
    const LEN: usize = 16;
    /// Three repeats still pass (report rate beats the IMU rate); the fourth freezes.
    const STALE_LIMIT: u8 = 4;

    fn apply(&mut self, r: &mut [u8]) {
        if r.len() < Self::OFFSET + Self::LEN || !matches!(r[0], ID_STATE | ID_STATE_BLE) {
            return;
        }
        let o = Self::OFFSET;
        let ts = u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        let live = if !std::mem::replace(&mut self.seen, true) {
            self.stale = Self::STALE_LIMIT;
            false
        } else if ts != self.last {
            self.stale = 0;
            true
        } else {
            self.stale = (self.stale + 1).min(Self::STALE_LIMIT);
            self.stale < Self::STALE_LIMIT
        };
        self.last = ts;
        if !live {
            r[o..o + Self::LEN].fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw report is play only with a button held: the IMU fills every one.
    #[test]
    fn only_a_held_button_counts_as_play() {
        let mut r = [0u8; 12];
        r[0] = ID_STATE;
        r[8] = 0x55;
        assert!(!holds_button(&r));
        r[3] = 1;
        assert!(holds_button(&r));
        assert!(!holds_button(&[ID_WIRELESS, 0, 1, 1, 1, 1]));
        assert!(!holds_button(&[ID_STATE, 0, 1]));
    }

    const OPEN: Sc2Gate = Sc2Gate {
        masked: false,
        system_local: false,
        chords: true,
    };

    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../../../clients/shared/sc2-vectors.json");
        serde_json::from_str(raw).expect("vector file parses")
    }

    fn state(ts: u32, buttons: u32) -> [u8; 54] {
        let mut r = [0u8; 54];
        r[0] = ID_STATE;
        r[1] = 7;
        r[2..6].copy_from_slice(&buttons.to_le_bytes());
        r[10] = 0x40; // left stick x
        r[30..34].copy_from_slice(&ts.to_le_bytes());
        r[36] = 0x11; // gyro
        r
    }

    fn filter(gate: Sc2Gate) -> Filter {
        Filter {
            gate,
            ..Filter::default()
        }
    }

    /// Buttons one report puts on the wire through `f`.
    fn sent(f: &mut Filter, gate: Sc2Gate, buttons: u32) -> u32 {
        f.gate = gate;
        let mut r = state(5, buttons);
        f.apply(&mut r);
        u32::from_le_bytes([r[2], r[3], r[4], r[5]])
    }

    /// A 2015 controller's state frame: version word, type, length, packet number, buttons.
    fn classic(buttons: u32) -> [u8; 64] {
        let mut r = [0u8; 64];
        r[..5].copy_from_slice(&[0x01, 0x00, CLASSIC_STATE, 0x3C, 9]);
        r[8..11].copy_from_slice(&buttons.to_le_bytes()[..3]);
        r[11] = 0x7F; // left trigger, the byte after the buttons
        r[16] = 0x40; // left pad or stick x
        r
    }

    /// The same gate on the 2015 pad's own bits. Its pad-touch flags are not buttons: they
    /// pass the ring chord and come back at once after a mask.
    #[test]
    fn a_2015_controller_is_gated_on_its_own_layout() {
        const A: u32 = 0x80;
        const SELECT: u32 = 0x1000;
        const STEAM: u32 = 0x2000;
        const LPAD_TOUCH: u32 = 0x08_0000;
        let buttons = |r: &[u8]| u32::from_le_bytes([r[8], r[9], r[10], 0]);
        let mut f = filter(OPEN);
        let mut r = classic(SELECT | LPAD_TOUCH);
        assert!(f.apply(&mut r));
        assert_eq!(buttons(&r), SELECT | LPAD_TOUCH);
        let mut r = classic(SELECT | A | LPAD_TOUCH);
        f.apply(&mut r);
        assert_eq!(buttons(&r), LPAD_TOUCH, "the chord report");
        assert_eq!((r[11], r[16]), (0x7F, 0x40), "trigger and pad untouched");

        f.gate.system_local = true;
        let mut r = classic(STEAM | 0x40);
        f.apply(&mut r);
        assert_eq!(buttons(&r), 0x40);

        f.gate.masked = true;
        let mut r = classic(A | LPAD_TOUCH);
        assert!(f.apply(&mut r));
        assert_eq!(r[..5], [0x01, 0x00, CLASSIC_STATE, 0x3C, 9]);
        assert!(r[8..].iter().all(|&b| b == 0));
        f.gate = OPEN;
        let mut r = classic(A | LPAD_TOUCH);
        f.apply(&mut r);
        assert_eq!(buttons(&r), LPAD_TOUCH, "A was held through the mask");

        assert!(holds_button(&classic(A)));
        assert!(!holds_button(&classic(0)));
        let mut edge = [0x01, 0x00, CLASSIC_WIRELESS, 0x01, 0x02];
        assert!(!f.apply(&mut edge), "a dongle's connect edge stays local");
        assert!(
            f.apply(&mut [0x01, 0x00, 0x04, 0x0B, 0, 0]),
            "battery status"
        );
    }

    #[test]
    fn classic_identity_requests_pack_and_carry_no_report_id() {
        let asked: Vec<_> = classic_identity_requests().collect();
        assert_eq!(asked.len(), 3);
        assert!(asked.iter().all(|r| r[0] == 0));
        let packed = crate::quic::pack_identity_replies(asked.iter().map(|r| (*r, *r)));
        assert_eq!(
            crate::quic::unpack_identity_replies(&packed).unwrap().len(),
            3
        );
    }

    #[test]
    fn identity_requests_name_their_own_lengths() {
        assert_eq!(identity_requests(false).count(), 8);
        let puck: Vec<_> = identity_requests(true).collect();
        assert_eq!(puck.len(), 12);
        for key in puck.iter().filter(|r| r[1] == 0xED) {
            assert_eq!(usize::from(key[2]), key.len() - 3, "{key:02X?}");
        }
        assert!(
            identity_requests(true).all(|r| r[1] != 0xB4),
            "slot state stays live"
        );
        let packed = crate::quic::pack_identity_replies(puck.iter().map(|r| (*r, *r)));
        let back = crate::quic::unpack_identity_replies(&packed).unwrap();
        assert_eq!(back.len(), 12);
        assert_eq!(back[8].0, puck[8]);
        assert!(crate::quic::unpack_identity_replies(&packed[..packed.len() - 1]).is_none());
        assert!(
            crate::quic::unpack_identity_replies(&[1, 0x83]).is_none(),
            "a request alone"
        );
    }

    #[test]
    fn gate_bits_round_trip() {
        assert_eq!(Sc2Gate::from_bits(0), Sc2Gate::default());
        let all = SC2_GATE_MASKED | SC2_GATE_SYSTEM_LOCAL | SC2_GATE_CHORDS;
        assert_eq!(
            Sc2Gate::from_bits(all | 0x80),
            Sc2Gate {
                masked: true,
                system_local: true,
                chords: true
            }
        );
    }

    /// The chord's Select is the bit the shared vectors map to wire Back.
    #[test]
    fn ring_select_is_the_shared_back_bit() {
        let file = vectors();
        let back = file["buttons"]
            .as_array()
            .expect("buttons")
            .iter()
            .find(|r| r["name"] == "back")
            .expect("back row");
        assert_eq!(u64::from(BTN_SELECT), back["sc2"].as_u64().unwrap());
    }

    /// `clients/shared/sc2-vectors.json`'s trace through one gate.
    #[test]
    fn imu_gate_matches_the_shared_trace() {
        let file = vectors();
        let mut gate = ImuGate::default();
        for (i, step) in file["imu_trace"]
            .as_array()
            .expect("imu_trace")
            .iter()
            .enumerate()
        {
            let len = step["len"].as_u64().unwrap() as usize;
            let mut r = vec![0u8; len];
            r[0] = step["id"].as_u64().unwrap() as u8;
            let ts = step["ts"].as_u64().unwrap() as u32;
            let imu = ImuGate::OFFSET..len.min(ImuGate::OFFSET + ImuGate::LEN);
            r[ImuGate::OFFSET..ImuGate::OFFSET + 4].copy_from_slice(&ts.to_le_bytes());
            r[ImuGate::OFFSET + 4..imu.end].fill(0x11);
            let before = r.clone();
            gate.apply(&mut r);
            if step["pass"].as_bool().unwrap() {
                assert_eq!(r, before, "step {i}");
            } else {
                assert!(r[imu].iter().all(|&b| b == 0), "step {i}");
            }
        }
    }

    #[test]
    fn frozen_imu_is_zeroed_and_a_moving_one_passes() {
        let mut f = filter(OPEN);
        let mut r = state(100, 0);
        assert!(f.apply(&mut r));
        assert_eq!(r[36], 0, "first sample is unproven");
        let mut r = state(100, 0);
        f.apply(&mut r);
        assert_eq!(r[36], 0, "frozen timestamp");
        let mut r = state(101, 0);
        f.apply(&mut r);
        assert_eq!(r[36], 0x11, "moving timestamp");
        for _ in 0..3 {
            let mut r = state(101, 0);
            f.apply(&mut r);
            assert_eq!(r[36], 0x11, "short repeats pass");
        }
        let mut r = state(101, 0);
        f.apply(&mut r);
        assert_eq!(r[36], 0, "fourth repeat freezes");
    }

    #[test]
    fn reset_forgets_the_old_pad_but_keeps_the_gate() {
        let mut f = filter(OPEN);
        let mut r = state(100, 0);
        f.apply(&mut r);
        let mut r = state(101, 0);
        f.apply(&mut r);
        assert_eq!(r[36], 0x11);
        f.reset();
        let mut r = state(102, 0);
        f.apply(&mut r);
        assert_eq!(r[36], 0, "first sample of the new pad is unproven");
        assert_eq!(f.gate, OPEN);
    }

    #[test]
    fn mask_neutralises_state_but_keeps_id_and_seq() {
        let mut f = filter(Sc2Gate {
            masked: true,
            ..OPEN
        });
        let mut r = state(5, 0x1);
        assert!(f.apply(&mut r));
        assert_eq!((r[0], r[1]), (ID_STATE, 7));
        assert!(r[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn local_system_buttons_stay_off_the_wire() {
        let mut f = filter(Sc2Gate {
            system_local: true,
            ..OPEN
        });
        let mut r = state(5, BTN_STEAM | BTN_QAM | 0x1);
        f.apply(&mut r);
        assert_eq!(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), 0x1);
        assert_eq!(r[10], 0x40);
    }

    #[test]
    fn wireless_status_and_empty_reports_never_reach_the_host() {
        let mut f = filter(OPEN);
        assert!(!f.apply(&mut [ID_WIRELESS, 0x01]));
        assert!(!f.apply(&mut [ID_WIRELESS_X, 0x02]));
        assert!(!f.apply(&mut []));
        assert!(f.apply(&mut [0x43, 80]));
    }

    #[test]
    fn select_then_a_stays_off_the_wire_until_each_is_released() {
        let mut f = Filter::default();
        assert_eq!(sent(&mut f, OPEN, BTN_SELECT), BTN_SELECT);
        assert_eq!(
            sent(&mut f, OPEN, BTN_SELECT | BTN_A),
            0,
            "the chord report"
        );
        assert_eq!(sent(&mut f, OPEN, BTN_SELECT | BTN_A | 0x2), 0x2);
        assert_eq!(sent(&mut f, OPEN, BTN_A), 0, "A outlives Select");
        assert_eq!(sent(&mut f, OPEN, 0), 0);
        assert_eq!(sent(&mut f, OPEN, BTN_A), BTN_A, "a fresh press goes out");
    }

    #[test]
    fn a_then_select_or_no_listener_is_no_chord() {
        let mut f = Filter::default();
        sent(&mut f, OPEN, BTN_A);
        assert_eq!(sent(&mut f, OPEN, BTN_A | BTN_SELECT), BTN_A | BTN_SELECT);
        let deaf = Sc2Gate {
            chords: false,
            ..OPEN
        };
        let mut f = Filter::default();
        sent(&mut f, deaf, BTN_SELECT);
        assert_eq!(sent(&mut f, deaf, BTN_SELECT | BTN_A), BTN_SELECT | BTN_A);
    }

    /// The ring closes on A; the host must not see that A pressed again.
    #[test]
    fn a_button_held_through_the_mask_stays_off_until_released() {
        let mut f = Filter::default();
        let masked = Sc2Gate {
            masked: true,
            ..OPEN
        };
        sent(&mut f, masked, BTN_A);
        assert_eq!(sent(&mut f, OPEN, BTN_A), 0);
        assert_eq!(sent(&mut f, OPEN, 0), 0);
        assert_eq!(sent(&mut f, OPEN, BTN_A), BTN_A);
    }
}
