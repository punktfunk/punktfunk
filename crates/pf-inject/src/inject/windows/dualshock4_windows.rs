//! Virtual DualShock 4 on Windows via the UMDF minidriver. Same sealed channel
//! as [`super::dualsense_windows`], same [`DsState`]. Differs in PnP identity
//! (`VID_054C&PID_09CC`, `pf_dualshock4`) and codec ([`super::dualshock4_proto`]).
//!
//! Stamp `device_type = 1` before the section magic so hidclass binds DS4, not
//! DualSense. Feedback is rumble (0xCA) and lightbar (`Led`, 0xCD). Pin:
//! `dualsense_windows::tests::hwid_matches_inf`. Channel:
//! `design/gamepad-channel-sealing.md`.

use super::dualsense_proto::DsState;
use super::dualsense_windows::{
    create_swdevice, driver_marks, publish_input, OutputDrain, SwDeviceProfile, DEVTYPE_DUALSHOCK4,
    OFF_DEVTYPE, OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::dualshock4_proto::{
    parse_ds4_output, serialize_state, Ds4Feedback, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H, DS4_TOUCH_W,
};
use super::gamepad_raii::PadChannel;
use crate::sensor_clock::SensorClock;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::{Duration, Instant};

/// INF hardware id. A package rename must not change this (`hwid_matches_inf`).
pub(super) const DS4_HWID: &str = "pf_dualshock4";

/// Drop closes the `pf_ds4_<index>` devnode. `pub` because it is `PadProto::Pad`.
pub struct Ds4WinPad {
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: super::gamepad_raii::DriverAttach,
    counter: u8,
    clock: SensorClock,
    /// v2.3 input-seqlock generation for `publish_input`.
    input_gen: u32,
    drain: OutputDrain,
}

impl Ds4WinPad {
    /// Stamp `device_type` and ring ver before the magic, then spawn `pf_ds4_<index>`.
    fn open(index: u8) -> Result<Ds4WinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_DUALSHOCK4;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // `2` = host drains the v2.2 long ring. Before magic so attach sees it.
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS4_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS4_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_ds4_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: DS4_HWID,
            usb_vid_pid: "VID_054C&PID_09CC",
            // Composite USB device (headset audio on 0-2); the HID interface is 3.
            usb_mi: Some(3),
            description: "Punktfunk Virtual DualShock 4",
            enumerator: "VID_054C&PID_09CC&MI_03",
        })?; // `?`: a swallowed fail latched a pad with no devnode; PadSlots never retried.
        let (hsw, instance_id) = (Some(hsw), instance_id);
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // 1500 ms: driver must read `device_type = 1` before hidclass asks for
        // descriptors, or the pad enumerates as DualSense.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(Ds4WinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_dualshock4",
                "pf_gamepad.inf", // one package serves both HID identities
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            counter: 0,
            clock: SensorClock::dualshock4(),
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    fn write_state(&mut self, st: &DsState) {
        self.counter = self.counter.wrapping_add(1);
        let ts = self.clock.ds4_ticks(Instant::now());
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.counter, ts);
        // SAFETY: `data_base()` maps a live SHM_SIZE section; `r` is the 64-byte
        // input slot. Seqlock is `publish_input`.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Drain every new `0x05` oldest-first so a stop-then-LED burst keeps both.
    fn service(&mut self) -> Ds4Feedback {
        self.channel.pump();
        let mut fb = Ds4Feedback::default();
        // SAFETY: the channel's section is live and SHM_SIZE bytes.
        let (proto, rev) = unsafe { driver_marks(self.channel.data_base()) };
        self.attach.observe_pad(proto, rev);
        let base = self.channel.data_base();
        fb.resync = self
            .drain
            .drain(base, |bytes| parse_ds4_output(bytes, &mut fb));
        fb
    }
}

/// Slot table, unplug, heartbeat, and `HidoutDedup` live in [`UhidManager`].
pub struct Ds4WinProto {
    /// Steam back-grip policy. DS4 has no paddle HID slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4WinProto {
    fn default() -> Ds4WinProto {
        Ds4WinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4WinProto {
    type Pad = Ds4WinPad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4/Windows";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<Ds4WinPad> {
        let p = Ds4WinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Touch, motion, and pad click ride the rich plane and must survive a button-only frame.
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = DsState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.touch = prev.touch;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.touch_click = prev.touch_click;
        s
    }

    /// Steam dual pads split one DS4 touchpad left/right; pad clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn neutralize_gyro(&self, st: &mut DsState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut DsState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut Ds4WinPad, st: &DsState) {
        pad.write_state(st);
    }

    /// Rumble on 0xCA, lightbar as 0xCD `Led`. No player LEDs or adaptive triggers.
    fn service(&self, pad: &mut Ds4WinPad, idx: u8) -> PadFeedback {
        let fb = pad.service();
        PadFeedback {
            // Trigger-motor slots are unused; see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            // Rumble-plane liveness; `parse_ds4_output` gates on flag0 bit0.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: fb.resync,
        }
    }
}

pub type DualShock4WindowsManager = UhidManager<Ds4WinProto>;
