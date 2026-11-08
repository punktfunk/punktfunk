//! Virtual 2015 Steam Controller (`28DE:1102`) on Windows over the pf_gamepad UMDF channel,
//! device type [`DEVTYPE_STEAMCONTROLLER`]. The Linux pad's state and feature filter
//! ([`ScState`], [`SteamFeedback::forward_sc_set`]) on a devnode shaped like the wired pad's
//! controller interface, `MI_02`, which Steam claims.
//!
//! A client that captured the physical pad sends its reports and the host mirrors them; any
//! other client drives the typed mapping. Steam's feature SETs come back FEATURE-tagged with
//! report id 0 first, and the ones the filter passes go to the client as `HidOutput::HidRaw`.

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use crate::steam_proto::{sc_merge_frame, sc_serial, ScState, SteamFeedback};
use crate::triton_proto::Sc2Identity;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_STEAMCONTROLLER;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE};
use std::time::Instant;

/// INF hardware id. A package rename must not touch it.
pub(super) const SC_HWID: &str = "pf_steamcontroller";

/// One virtual Steam Controller: `SwDeviceCreate`'d `pf_steamctrl_<index>` plus the sealed
/// channel. `pub` because it is `PadProto::Pad`.
pub struct ScWinPad {
    shm: ShmPad,
    /// Sequence of the typed frames; a mirrored report carries the pad's own.
    seq: u32,
}

impl ScWinPad {
    /// The devnode always carries an identity property: it outlives the pad, and a pad without
    /// an identity must not answer with the last one's.
    fn open(index: u8, identity: Option<&Sc2Identity>) -> Result<ScWinPad> {
        let blob = identity.map_or_else(|| vec![0], Sc2Identity::blob);
        let shm = ShmPad::open(
            index,
            DEVTYPE_STEAMCONTROLLER,
            &pf_driver_proto::deck::SC_NEUTRAL_REPORT,
            &SwDeviceProfile {
                instance: &format!("pf_steamctrl_{index}"),
                container_tag: 0x5046_4453, // "PFDS"
                container_index: index,
                hwid: SC_HWID,
                usb_vid_pid: Some("VID_28DE&PID_1102"),
                // Mouse and keyboard on interfaces 0 and 1; Steam claims the controller on 2.
                usb_mi: Some(2),
                bluetooth: false,
                description: "Punktfunk Virtual Steam Controller (2015)",
                // Steam merges a pad's HID views by the USB interface token in the instance path.
                enumerator: "VID_28DE&PID_1102&MI_02",
                property: Some((
                    pf_driver_proto::triton::IDENTITY_PROPKEY_FMTID,
                    pf_driver_proto::triton::IDENTITY_PROPKEY_PID,
                    &blob,
                )),
            },
        )?;
        Ok(ScWinPad { shm, seq: 0 })
    }

    fn write_state(&mut self, st: &ScState) {
        let r = st.report(&mut self.seq, Instant::now());
        self.shm.publish(&r);
    }

    /// Steam's feature SETs for the physical pad, and the ring-overflow flag. An OUTPUT report
    /// is not part of this pad's protocol and stays here.
    fn service(&mut self, idx: u8) -> (Vec<HidOutput>, bool) {
        let mut fb = SteamFeedback::default();
        let resync = self.shm.poll(|bytes, feature| {
            if feature {
                fb.forward_sc_set(bytes.get(1..).unwrap_or_default());
            }
        });
        let hidout = fb
            .raw
            .into_iter()
            .map(|data| HidOutput::HidRaw {
                pad: idx,
                kind: HID_RAW_FEATURE,
                data,
            })
            .collect();
        (hidout, resync)
    }
}

/// Windows Steam Controller [`PadProto`]: sealed-channel open, raw mirroring with the typed
/// fallback, feature writes back to the client. Lifecycle lives in [`UhidManager`].
pub struct ScWinProto {
    /// Fold for wire paddles beyond the pad's two grips (PADDLE3/4).
    remap: crate::steam_remap::RemapConfig,
}

impl Default for ScWinProto {
    fn default() -> ScWinProto {
        ScWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for ScWinProto {
    type Pad = ScWinPad;
    type State = ScState;
    const LABEL: &'static str = "Steam Controller/Windows";
    const DEVICE: &'static str = "Steam Controller";
    const CREATE_HINT: &'static str = " (install/repair: run the Punktfunk installer again)";

    fn open(&mut self, idx: u8) -> Result<ScWinPad> {
        let identity = crate::triton_proto::identity_for(idx);
        let p = ScWinPad::open(idx, identity.as_deref())?;
        let serial = identity.as_ref().and_then(|i| i.serial.clone());
        tracing::info!(
            index = idx,
            serial = serial.unwrap_or_else(|| sc_serial(idx)),
            replies = identity.as_ref().map_or(0, |i| i.replies.len()),
            "virtual Steam Controller created (Windows UMDF shm channel, MI_02 identity)"
        );
        Ok(p)
    }

    /// Typed frames keep arriving beside a mirrored pad's reports: they update the fallback
    /// and show the client is alive.
    fn merge_frame(&self, prev: &ScState, f: &punktfunk_core::input::GamepadFrame) -> ScState {
        let mut next = *prev;
        next.typed_frame(
            sc_merge_frame(&prev.typed, f, self.remap.paddles),
            Instant::now(),
        );
        next
    }

    fn apply_rich(&self, st: &mut ScState, rich: RichInput) {
        st.apply_rich(rich, Instant::now());
    }

    fn write_state(&self, pad: &mut ScWinPad, st: &ScState) {
        pad.write_state(st);
    }

    /// Feature writes on `[0xCD][0x05]`. The pad has no motors: its haptics are among them.
    /// The drain's `resync` reaches `PadFeedback` unchanged.
    fn service(&self, pad: &mut ScWinPad, idx: u8) -> PadFeedback {
        let (hidout, resync) = pad.service(idx);
        PadFeedback {
            hidout,
            resync,
            ..PadFeedback::default()
        }
    }
}

pub type SteamControllerWindowsManager = UhidManager<ScWinProto>;
