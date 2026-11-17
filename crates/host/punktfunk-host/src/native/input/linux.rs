//! The Linux virtual-pad managers behind [`PadBackends`](super::PadBackends): UHID/usbip, one
//! per identity, and the uinput Xbox One and Elite. Xbox360 over uinput stays in `Pads`.

use super::*;
use crate::inject::eightbitdo_proto::Model as EightBitDo;
use crate::inject::switch2_proto::Model as Switch2;

/// In pump and heartbeat order.
pub(super) const MANAGERS: &[ManagerRow] = &[
    (GamepadPref::XboxOne, |_| {
        boxed(crate::inject::gamepad::GamepadManager::with_identity(
            crate::inject::gamepad::PadIdentity::xbox_one(),
        ))
    }),
    (GamepadPref::XboxElite, |_| {
        boxed(crate::inject::gamepad::GamepadManager::with_identity(
            crate::inject::gamepad::PadIdentity::elite2(),
        ))
    }),
    (GamepadPref::DualSense, |gamepad_only| {
        boxed(crate::inject::dualsense::dualsense_manager(gamepad_only))
    }),
    (GamepadPref::DualSenseEdge, |gamepad_only| {
        boxed(crate::inject::dualsense::dualsense_edge_manager(
            gamepad_only,
        ))
    }),
    (GamepadPref::DualShock4, |gamepad_only| {
        boxed(crate::inject::dualshock4::dualshock4_manager(gamepad_only))
    }),
    (GamepadPref::SteamDeck, |_| {
        boxed(crate::inject::steam_controller::SteamControllerManager::new())
    }),
    (GamepadPref::SwitchPro, |_| {
        boxed(crate::inject::switch_pro::SwitchProManager::new())
    }),
    (GamepadPref::SteamController, |_| {
        boxed(crate::inject::steam_controller::SteamCtrlManager::new())
    }),
    (GamepadPref::SteamController2, |_| {
        boxed(pf_inject::steam_controller2::Triton2Manager::new())
    }),
    (GamepadPref::SteamController2Puck, |_| {
        boxed(
            crate::inject::steam_controller2::Triton2Manager::with_backend(
                crate::inject::steam_controller2::TritonProto::puck(),
            ),
        )
    }),
    (GamepadPref::EightBitDoUltimate2, |_| {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Ultimate2))
    }),
    (GamepadPref::EightBitDoPro2, |_| {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Pro2))
    }),
    (GamepadPref::EightBitDoPro3, |_| {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Pro3))
    }),
    (GamepadPref::HoripadSteam, |_| {
        boxed(crate::inject::hori_steam::HoriManager::new())
    }),
    (GamepadPref::JoyConPair, |_| {
        boxed(crate::inject::switch_pro::JoyConPairManager::new())
    }),
    (GamepadPref::Switch2Pro, |_| {
        boxed(crate::inject::switch2_usbip::manager(Switch2::Pro))
    }),
    (GamepadPref::Switch2GameCube, |_| {
        boxed(crate::inject::switch2_usbip::manager(Switch2::GameCube))
    }),
];
