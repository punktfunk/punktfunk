//! Virtual Steam Deck on Windows via the UMDF minidriver — analogue of the Linux
//! UHID Deck ([`super::steam_controller`]'s `SteamProto`). Shares
//! [`super::steam_proto`]: the `ID_CONTROLLER_DECK_STATE` serializer, the
//! XInput/rich mappers, the `0xEB` rumble parser.
//!
//! Transport is the sealed shared-memory channel plus a `SwDeviceCreate` devnode
//! (device-type 3). USB hardware ids must carry `&MI_02` (wired Deck controller
//! interface); hidclass mirrors that into the HID child as `bInterfaceNumber`,
//! and Steam Input claims on it. Missing `MI_` → hidapi reports interface 0.
//!
//! Steam writes rumble (`0xEB`) and trackpad haptics (`0x8F`) via SET_FEATURE;
//! the driver republishes them into the output slot (report-id-0 prefixed).
//! [`parse_steam_output`] reads the same wire shape as Linux. No gamepad-mode
//! entry pulse — that gate is Linux-evdev only.

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use super::steam_proto::{neutral_deck_report, parse_steam_output, DeckEncoder, SteamState};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;

/// INF hardware id. A package rename must not touch it
/// (`dualsense_windows::tests::hwid_matches_inf`).
pub(super) const DECK_HWID: &str = "pf_steamdeck";

/// One virtual Deck: `SwDeviceCreate`'d `pf_deck_<index>` plus the sealed
/// channel. `pub` because it is `PadProto::Pad`.
pub struct DeckWinPad {
    shm: ShmPad,
    enc: DeckEncoder,
}

impl DeckWinPad {
    /// Spawn `pf_deck_<index>` with the `MI_02` USB identity Steam's promotion gate requires.
    fn open(index: u8) -> Result<DeckWinPad> {
        let shm = ShmPad::open(
            index,
            pf_driver_proto::gamepad::DEVTYPE_STEAMDECK,
            &neutral_deck_report(),
            &SwDeviceProfile {
                instance: &format!("pf_deck_{index}"),
                container_tag: 0x5046_4453, // "PFDS"
                container_index: index,
                hwid: DECK_HWID,
                usb_vid_pid: Some("VID_28DE&PID_1205"),
                // Wired Deck controller interface. Without this the HID child has no MI_
                // token, hidapi reports interface 0, and Steam never claims the pad.
                usb_mi: Some(2),
                bluetooth: false,
                description: "Punktfunk Virtual Steam Deck",
                enumerator: "punktfunk",
                property: None,
            },
        )?;
        Ok(DeckWinPad {
            shm,
            enc: DeckEncoder::default(),
        })
    }

    fn write_state(&mut self, st: &SteamState) {
        let r = self.enc.encode(st);
        self.shm.publish(&r);
    }

    fn service(&mut self) -> (Option<(u16, u16)>, bool) {
        let mut rumble = None;
        let resync = self.shm.poll(|bytes, _| {
            // Last rumble-carrying report wins. `0x8F` trackpad-haptic reports
            // carry none and must not clear it.
            if let Some(r) = parse_steam_output(bytes).rumble {
                rumble = Some(r);
            }
        });
        (rumble, resync)
    }
}

/// Windows Deck [`PadProto`]: sealed-channel open under the promoted identity,
/// same [`SteamState`] mappers as Linux. Slot table, unplug, heartbeat, and
/// rumble dedup live in [`UhidManager`].
#[derive(Default)]
pub struct DeckWinProto;

impl PadProto for DeckWinProto {
    type Pad = DeckWinPad;
    type State = SteamState;
    const LABEL: &'static str = "Steam Deck/Windows";
    const DEVICE: &'static str = "Steam Deck";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DeckWinPad> {
        let p = DeckWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Steam Deck created (Windows UMDF shm channel, MI_02 promoted identity)"
        );
        Ok(p)
    }

    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
        SteamState::merge_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut DeckWinPad, st: &SteamState) {
        pad.write_state(st);
    }

    /// Rumble on the 0xCA plane. No lightbar / adaptive triggers, so `hidout`
    /// stays empty — same as Linux.
    fn service(&self, pad: &mut DeckWinPad, _idx: u8) -> PadFeedback {
        // `Some` means a rumble-carrying report landed, even at an unchanged
        // level — that is the rumble-plane activity signal.
        let (rumble, resync) = pad.service();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

pub type SteamDeckWindowsManager = UhidManager<DeckWinProto>;
