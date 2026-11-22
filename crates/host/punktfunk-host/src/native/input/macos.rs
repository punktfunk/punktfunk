//! The macOS virtual-pad managers behind [`PadBackends`](super::PadBackends): `IOHIDUserDevice`,
//! one per identity. Xbox360 is the Xbox Wireless pad, built here so it heartbeats like the
//! other Xbox identities; `Pads` never needs its own.

use super::*;
use crate::inject::dualsense_macos::{DsMacProto, DualSenseMacManager};
use crate::inject::xbox_macos::{XboxMacManager, XboxMacProto};

/// In pump and heartbeat order.
pub(super) const MANAGERS: &[ManagerRow] = &[
    (GamepadPref::Xbox360, |_| boxed(XboxMacManager::new())),
    (
        GamepadPref::DualSense,
        |_| boxed(DualSenseMacManager::new()),
    ),
    (GamepadPref::XboxOne, |_| {
        boxed(XboxMacManager::with_backend(XboxMacProto::one_s()))
    }),
    (GamepadPref::XboxElite, |_| {
        boxed(XboxMacManager::with_backend(XboxMacProto::elite()))
    }),
    (GamepadPref::DualSenseEdge, |_| {
        boxed(DualSenseMacManager::with_backend(DsMacProto::edge()))
    }),
    (GamepadPref::DualShock4, |_| {
        boxed(crate::inject::dualshock4_macos::DualShock4MacManager::new())
    }),
    (GamepadPref::SwitchPro, |_| {
        boxed(crate::inject::switch_pro_macos::SwitchProMacManager::new())
    }),
];
