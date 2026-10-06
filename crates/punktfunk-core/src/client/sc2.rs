//! Steam Controller 2 raw reports on their way to the host. Every client's capture hands them to
//! [`NativeClient::send_rich_input`](super::NativeClient::send_rich_input), and one [`Filter`]
//! per pad decides what the host sees: wireless status stays local, the ring chord and a masked
//! pad stay off the wire, Steam and QAM follow the gate, and a frozen IMU block goes out zeroed.
//! Raw HID reports come only from SC2 captures, so the report id alone selects the rules.

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

/// Wire pads a client can address: every `pad` masks to 16.
pub(crate) const PADS: usize = 16;

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

impl Filter {
    /// Gate one id-first report in place. False for a report that stays off the wire: the host
    /// keeps its own Puck connect edges.
    pub(crate) fn apply(&mut self, r: &mut [u8]) -> bool {
        match r.first() {
            None | Some(&(ID_WIRELESS | ID_WIRELESS_X)) => return false,
            Some(&(ID_STATE | ID_STATE_BLE | ID_STATE_TIMESTAMP)) if r.len() >= 6 => {
                let gate = self.gate;
                let mut b = self
                    .ring
                    .apply(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), gate);
                if gate.masked {
                    r[2..].fill(0);
                } else {
                    if gate.system_local {
                        b &= !(BTN_STEAM | BTN_QAM);
                    }
                    r[2..6].copy_from_slice(&b.to_le_bytes());
                }
            }
            _ => {}
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
    fn apply(&mut self, buttons: u32, gate: Sc2Gate) -> u32 {
        let was = std::mem::replace(&mut self.held, buttons);
        self.swallow &= buttons;
        if gate.masked {
            self.swallow |= buttons;
        } else if gate.chords && buttons & !was & BTN_A != 0 && was & BTN_SELECT != 0 {
            self.swallow |= BTN_A | BTN_SELECT;
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

    const OPEN: Sc2Gate = Sc2Gate {
        masked: false,
        system_local: false,
        chords: true,
    };

    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../../clients/shared/sc2-vectors.json");
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
