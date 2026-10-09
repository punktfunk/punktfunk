//! Virtual 8BitDo pads in their own HID mode via `/dev/uhid`.
//!
//! No kernel driver claims `2DC8:6003/6009/6012`, so `hid-generic` binds and SDL's and Steam's
//! `8bitdo` driver read hidraw — the hidraw rule in `60-punktfunk.rules` lets the seat user in.
//! Codec, descriptors and the feature reply live in [`super::eightbitdo_proto`]; this file is the
//! UHID transport.

use super::eightbitdo_proto::{
    caps_reply, parse_rumble, EightBitDoState, Model, FEATURE_CAPS, VENDOR,
};
use crate::sensor_clock::SensorClock;
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent, BUS_BLUETOOTH, BUS_USB};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;
use std::time::{Duration, Instant};

/// Drop destroys the device.
pub struct EightBitDoPad {
    dev: UhidDevice,
    clock: SensorClock,
}

impl EightBitDoPad {
    pub fn open(model: Model, index: u8) -> Result<EightBitDoPad> {
        let dev = UhidDevice::open(&Create2 {
            bus: if model.bluetooth() {
                BUS_BLUETOOTH
            } else {
                BUS_USB
            },
            name: model.name(),
            phys: &format!("punktfunk/8bitdo/{index}"),
            uniq: &format!("punktfunk-8bitdo-{index}"),
            rdesc: model.rdesc(),
            vendor: VENDOR as u32,
            product: model.product() as u32,
            version: 0x0100,
        })?;
        Ok(EightBitDoPad {
            dev,
            clock: SensorClock::micros(),
        })
    }
}

/// One manager per model: the model fixes the identity, the face-button swap and the pacing.
pub struct EightBitDoProto {
    model: Model,
}

impl EightBitDoProto {
    pub fn new(model: Model) -> EightBitDoProto {
        EightBitDoProto { model }
    }
}

impl PadProto for EightBitDoProto {
    type Pad = EightBitDoPad;
    type State = EightBitDoState;
    const LABEL: &'static str = "8BitDo";
    const DEVICE: &'static str = "8BitDo";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<EightBitDoPad> {
        let p = EightBitDoPad::open(self.model, idx)?;
        tracing::info!(
            index = idx,
            model = self.model.name(),
            "virtual 8BitDo created (UHID hid-generic)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &EightBitDoState, f: &GamepadFrame) -> EightBitDoState {
        EightBitDoState::merge_frame(self.model, prev, f)
    }

    fn apply_rich(&self, st: &mut EightBitDoState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut EightBitDoPad, st: &EightBitDoState) {
        let clock = self
            .model
            .timestamps()
            .then(|| pad.clock.ticks(Instant::now()) as u32);
        let _ = pad.dev.write_input(&st.serialize(clock));
    }

    /// Rumble on 0xCA; the Pro models' capability feature answered from [`caps_reply`].
    fn service(&self, pad: &mut EightBitDoPad, idx: u8) -> PadFeedback {
        let model = self.model;
        let answers_caps = model.timestamps();
        let mut rumble = None;
        pad.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => {
                if let Some(r) = parse_rumble(data) {
                    rumble = Some(r);
                }
            }
            UhidEvent::GetReport { id, rnum } => {
                let caps = caps_reply(model.devtype(), idx);
                let data = (answers_caps && rnum == FEATURE_CAPS).then_some(&caps[..]);
                let _ = dev.reply_get_report(id, data);
            }
            UhidEvent::SetReport(_) => {}
        });
        PadFeedback {
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            // Every `0x05` names both motors, so each one drives the rumble plane.
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }

    fn report_period(&self) -> Option<Duration> {
        self.model.report_period()
    }
}

pub type EightBitDoManager = UhidManager<EightBitDoProto>;

/// A manager for one model's pads.
pub fn manager(model: Model) -> EightBitDoManager {
    UhidManager::with_backend(EightBitDoProto::new(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::{gamepad as gs, GamepadEvent};

    /// Holds one of each native pad live for `PF_PAD_HOLD_SECS` (default 3) so an SDL probe can
    /// read them: paddle beats every 400 ms, a steady 100 °/s pitch, rumble echoed to stdout.
    #[test]
    #[ignore = "creates real /dev/uhid devices; needs the input group"]
    fn native_pads_hold_for_a_probe() {
        let secs = std::env::var("PF_PAD_HOLD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let mut eightbitdo = [
            (0u8, manager(Model::Ultimate2)),
            (1, manager(Model::Pro2)),
            (2, manager(Model::Pro3)),
        ];
        let (hori_idx, mut hori) = (3u8, crate::hori_steam::HoriManager::new());
        let arrival = |index| GamepadEvent::Arrival {
            index,
            kind: 0,
            capabilities: 0,
            audio_caps: 0,
        };
        let frame = |index: u8, buttons| {
            GamepadEvent::State(GamepadFrame {
                index: index as i16,
                active_mask: 1 << index,
                buttons,
                ..Default::default()
            })
        };
        let motion = |pad| RichInput::Motion {
            pad,
            gyro: [(100 * gs::MOTION_GYRO_LSB_PER_DEG_S) as i16, 0, 0],
            accel: [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
        };
        for (i, m) in &mut eightbitdo {
            m.handle(&arrival(*i));
        }
        hori.handle(&arrival(hori_idx));
        let live = eightbitdo.iter().map(|(_, m)| m.live_pads()).sum::<usize>() + hori.live_pads();
        assert_eq!(live, 4, "every pad must be created");
        println!("4 native pads up for {secs}s");

        let (start, mut last, mut beat) = (Instant::now(), Instant::now(), 0u32);
        let mut last_motion = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            // 50 Hz: under the manager's 100 ms idle watchdog, which zeroes a stalled gyro.
            if last_motion.elapsed() >= Duration::from_millis(20) {
                last_motion = Instant::now();
                for (i, m) in &mut eightbitdo {
                    m.apply_rich(motion(*i));
                }
                hori.apply_rich(motion(hori_idx));
            }
            if last.elapsed() >= Duration::from_millis(400) {
                last = Instant::now();
                beat += 1;
                let buttons = if beat % 2 == 0 {
                    gs::BTN_A | gs::BTN_PADDLE1 | gs::BTN_PADDLE2
                } else {
                    0
                };
                for (i, m) in &mut eightbitdo {
                    m.handle(&frame(*i, buttons));
                }
                hori.handle(&frame(hori_idx, buttons));
            }
            let echo = |pad, low, high, _, _| println!("rumble pad={pad} low={low} high={high}");
            for (_, m) in &mut eightbitdo {
                m.pump(echo, |_| {});
                m.heartbeat(Duration::from_millis(8));
            }
            hori.pump(echo, |_| {});
            hori.heartbeat(Duration::from_millis(8));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
