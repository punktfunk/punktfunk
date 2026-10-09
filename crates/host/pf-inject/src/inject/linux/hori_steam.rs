//! Virtual Wireless HORIPAD for Steam via `/dev/uhid`, wired identity `0F0D:01AB`.
//!
//! `hid-generic` binds; SDL's and Steam's `steam_hori` driver read hidraw. The pad takes no
//! output reports and answers no features. Codec and descriptor live in [`super::hori_proto`].

use super::hori_proto::{serial, HoriState, NAME, PRODUCT, RDESC, REPORT_PERIOD, VENDOR};
use crate::sensor_clock::SensorClock;
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent, BUS_USB};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;
use std::time::{Duration, Instant};

/// Drop destroys the device.
pub struct HoriPad {
    dev: UhidDevice,
    clock: SensorClock,
    serial: [u8; 6],
}

impl HoriPad {
    pub fn open(index: u8) -> Result<HoriPad> {
        let dev = UhidDevice::open(&Create2 {
            bus: BUS_USB,
            name: NAME,
            phys: &format!("punktfunk/horipad/{index}"),
            uniq: &format!("punktfunk-horipad-{index}"),
            rdesc: &RDESC,
            vendor: VENDOR as u32,
            product: PRODUCT as u32,
            version: 0x0100,
        })?;
        Ok(HoriPad {
            dev,
            clock: SensorClock::micros(),
            serial: serial(index),
        })
    }
}

#[derive(Default)]
pub struct HoriProto;

impl PadProto for HoriProto {
    type Pad = HoriPad;
    type State = HoriState;
    const LABEL: &'static str = "HORIPAD";
    const DEVICE: &'static str = "HORIPAD for Steam";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<HoriPad> {
        let p = HoriPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual HORIPAD for Steam created (UHID hid-generic)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &HoriState, f: &GamepadFrame) -> HoriState {
        HoriState::merge_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut HoriState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut HoriPad, st: &HoriState) {
        let clock = pad.clock.ticks(Instant::now()) as u16;
        let _ = pad.dev.write_input(&st.serialize(clock, pad.serial));
    }

    /// No outputs to read; a feature request gets EIO rather than a stalled reader.
    fn service(&self, pad: &mut HoriPad, _idx: u8) -> PadFeedback {
        pad.dev.poll(|dev, ev| {
            if let UhidEvent::GetReport { id, .. } = ev {
                let _ = dev.reply_get_report(id, None);
            }
        });
        PadFeedback::default()
    }

    fn report_period(&self) -> Option<Duration> {
        Some(REPORT_PERIOD)
    }
}

pub type HoriManager = UhidManager<HoriProto>;
