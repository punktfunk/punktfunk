//! Virtual Switch Pro Controller on Windows via the UMDF minidriver: device type 8,
//! `VID_057E&PID_2009`, hardware id `pf_switchpro`. Same sealed channel as
//! [`super::dualsense_windows`]; state mapping is [`super::switch_proto`].
//!
//! The driver answers the `0x80` / `0x01` handshake itself (`pf_driver_proto::switch::reply`)
//! and serves the host's latest `0x30` every 8 ms. Rumble and player lights come back through
//! the output ring. Pin: `dualsense_windows::tests::hwid_matches_inf`.

use super::dualsense_windows::{
    create_swdevice, driver_marks, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::PadChannel;
use super::switch_proto::{
    parse_output, player_leds_bits, serialize_report_0x30, SwitchOutput, SwitchState,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO;
use pf_driver_proto::switch as wire;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::Duration;

/// INF hardware id. A package rename must not change this (`hwid_matches_inf`).
pub(super) const SWITCH_HWID: &str = "pf_switchpro";

/// Drop closes the `pf_swpro_<index>` devnode. `pub` because it is `PadProto::Pad`.
pub struct SwitchWinPad {
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: super::gamepad_raii::DriverAttach,
    /// v2.3 input-seqlock generation for `publish_input`.
    input_gen: u32,
    drain: OutputDrain,
}

impl SwitchWinPad {
    /// Stamp `device_type` and ring ver before the magic, then spawn `pf_swpro_<index>`.
    fn open(index: u8) -> Result<SwitchWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_SWITCH_PRO;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // `2` = host drains the v2.2 long ring. Before magic so attach sees it.
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; wire::REPORT_LEN],
                wire::neutral_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_swpro_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_5357, // "PFSW"
            container_index: index,
            hwid: SWITCH_HWID,
            usb_vid_pid: "VID_057E&PID_2009",
            // A wired Pro Controller is a single-interface HID device.
            usb_mi: None,
            description: "Punktfunk Virtual Pro Controller",
            enumerator: "VID_057E&PID_2009",
        })?; // `?`: a swallowed fail latched a pad with no devnode; PadSlots never retried.
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = Some(super::gamepad_raii::SwDevice::new(hsw));
        // The driver must read `device_type = 8` before hidclass asks for descriptors.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(SwitchWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                SWITCH_HWID,
                "pf_gamepad.inf", // one INF serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// The driver stamps the timer byte per served report, so the host's is left at zero.
    fn write_state(&mut self, st: &SwitchState) {
        let r = serialize_report_0x30(st, 0);
        // SAFETY: `data_base()` maps a live SHM_SIZE section; `r` is the 64-byte input slot.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Rumble from every `0x01` / `0x10`, player lights from subcommand `0x30`, oldest first.
    fn service(&mut self, pad: u8) -> PadFeedback {
        self.channel.pump();
        // SAFETY: the channel's section is live and SHM_SIZE bytes.
        let (proto, rev) = unsafe { driver_marks(self.channel.data_base()) };
        self.attach.observe_pad(proto, rev);
        let mut fb = PadFeedback::default();
        let base = self.channel.data_base();
        fb.resync = self.drain.drain(base, |bytes| match parse_output(bytes) {
            Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                if let (0x30, Some(&arg)) = (id, args.first()) {
                    fb.hidout.push(HidOutput::PlayerLeds {
                        pad,
                        bits: player_leds_bits(arg),
                    });
                }
            }
            Some(SwitchOutput::Rumble(r)) => fb.rumble = Some((r.0, r.1, 0, 0)),
            Some(SwitchOutput::UsbCmd(_)) | None => {}
        });
        fb
    }
}

/// Slot table, unplug, heartbeat, and `HidoutDedup` live in [`UhidManager`].
pub struct SwitchWinProto {
    /// Steam back-grip fold. A Pro Controller has no paddle slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchWinProto {
    fn default() -> SwitchWinProto {
        SwitchWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchWinProto {
    type Pad = SwitchWinPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro/Windows";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<SwitchWinPad> {
        let p = SwitchWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> SwitchState {
        SwitchState::neutral()
    }

    /// Button/stick/trigger frame. Keep prev motion — it arrives on the rich plane.
    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = SwitchState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s
    }

    /// IMU samples only; a Pro Controller has no touchpad.
    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            st.apply_motion(gyro, accel);
        }
    }

    fn neutralize_gyro(&self, st: &mut SwitchState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SwitchState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut SwitchWinPad, st: &SwitchState) {
        pad.write_state(st);
    }

    /// HD rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchWinPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // Every subcommand carries rumble, so a poll that saw rumble is the activity signal.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

pub type SwitchProWindowsManager = UhidManager<SwitchWinProto>;
