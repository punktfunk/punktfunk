//! The Windows virtual-pad managers behind [`PadBackends`](super::PadBackends): UMDF HID, one
//! per identity. Xbox360 over XUSB stays in `Pads`.

use super::*;
use crate::inject::eightbitdo_proto::Model as EightBitDo;
use crate::inject::xbox_windows::{XboxWinProto, XboxWindowsManager};
use crate::native::gamepad::windows_xbox_hid;

/// In pump and heartbeat order.
pub(super) const MANAGERS: &[ManagerRow] = &[
    (GamepadPref::SteamController2, |_| {
        boxed(pf_inject::triton_windows::TritonWindowsManager::new())
    }),
    (GamepadPref::DualSense, |_| {
        boxed(crate::inject::dualsense_windows::DualSenseWindowsManager::new())
    }),
    // HID Xbox, one identity per manager, unless `windows_xbox_hid` picks XUSB: then
    // `degrade_xbox_identity` has folded One and Elite to Xbox360, which falls through to the
    // XUSB pad in `Pads`. Never both: two devices for one wire pad are two controllers.
    (GamepadPref::Xbox360, |_| {
        windows_xbox_hid()
            .then(XboxWindowsManager::new)
            .and_then(boxed)
    }),
    (GamepadPref::XboxOne, |_| {
        windows_xbox_hid()
            .then(|| XboxWindowsManager::with_backend(XboxWinProto::one_s()))
            .and_then(boxed)
    }),
    (GamepadPref::XboxElite, |_| {
        windows_xbox_hid()
            .then(|| XboxWindowsManager::with_backend(XboxWinProto::elite()))
            .and_then(boxed)
    }),
    (GamepadPref::DualSenseEdge, |_| {
        boxed(crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new())
    }),
    (GamepadPref::DualShock4, |_| {
        boxed(crate::inject::dualshock4_windows::DualShock4WindowsManager::new())
    }),
    (GamepadPref::SteamDeck, |_| {
        boxed(crate::inject::steam_deck_windows::SteamDeckWindowsManager::new())
    }),
    (GamepadPref::SteamController, |_| {
        boxed(crate::inject::steam_controller_windows::SteamControllerWindowsManager::new())
    }),
    (GamepadPref::SwitchPro, |_| {
        boxed(crate::inject::switch_pro_windows::SwitchProWindowsManager::new())
    }),
    (GamepadPref::EightBitDoUltimate2, |_| {
        boxed(crate::inject::eightbitdo_windows::manager(
            EightBitDo::Ultimate2,
        ))
    }),
    (GamepadPref::EightBitDoPro2, |_| {
        boxed(crate::inject::eightbitdo_windows::manager(EightBitDo::Pro2))
    }),
    (GamepadPref::EightBitDoPro3, |_| {
        boxed(crate::inject::eightbitdo_windows::manager(EightBitDo::Pro3))
    }),
    (GamepadPref::HoripadSteam, |_| {
        boxed(crate::inject::hori_windows::HoriWindowsManager::new())
    }),
    (GamepadPref::JoyConPair, |_| {
        boxed(crate::inject::switch_pro_windows::JoyConWindowsManager::new())
    }),
];
