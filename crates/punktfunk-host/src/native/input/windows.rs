//! The Windows virtual-pad backends behind [`Pads`](super::Pads): UMDF HID managers, one per
//! identity. Xbox360 over XUSB is the common default and stays in `Pads`.

use super::*;

/// Windows UMDF Triton backend.
type Sc2Manager = pf_inject::triton_windows::TritonWindowsManager;

/// Managers are created lazily and own only the indices routed to them.
#[derive(Default)]
pub(super) struct PadBackends {
    steamctrl2: Option<Sc2Manager>,
    dualsense_win: Option<crate::inject::dualsense_windows::DualSenseWindowsManager>,
    /// HID Xbox pad ([`crate::inject::xbox_windows`]), used instead of `xbox360`'s
    /// XUSB companion when [`super::super::gamepad::windows_xbox_hid`] is set. Never both:
    /// two devices for one wire pad is "the game sees two controllers".
    ///
    /// Three managers — one identity each (Wireless / One S / Elite), bound at
    /// construction — so a mixed session can present different Xbox pads at once.
    xbox_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    xbox_one_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    xbox_elite_hid: Option<crate::inject::xbox_windows::XboxWindowsManager>,
    dualsense_edge_win: Option<crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager>,
    dualshock4_win: Option<crate::inject::dualshock4_windows::DualShock4WindowsManager>,
    steamdeck_win: Option<crate::inject::steam_deck_windows::SteamDeckWindowsManager>,
}

impl PadBackends {
    /// Route a pad event to the manager for `kind`, building it on first use. `false` = no
    /// backend of this kind here; the caller's Xbox360 default takes it.
    pub(super) fn route_handle(
        &mut self,
        kind: GamepadPref,
        ev: &punktfunk_core::input::GamepadEvent,
        // A seat is a gamescope shape; a Windows host has one desktop and one `/dev`-less OS.
        _dev: &Option<std::path::PathBuf>,
    ) -> bool {
        match kind {
            GamepadPref::SteamController2 => self
                .steamctrl2
                .get_or_insert_with(Sc2Manager::new)
                .handle(ev),
            GamepadPref::DualSense => self
                .dualsense_win
                .get_or_insert_with(crate::inject::dualsense_windows::DualSenseWindowsManager::new)
                .handle(ev),
            GamepadPref::DualSenseEdge => self
                .dualsense_edge_win
                .get_or_insert_with(
                    crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new,
                )
                .handle(ev),
            GamepadPref::DualShock4 => self
                .dualshock4_win
                .get_or_insert_with(
                    crate::inject::dualshock4_windows::DualShock4WindowsManager::new,
                )
                .handle(ev),
            GamepadPref::SteamDeck => self
                .steamdeck_win
                .get_or_insert_with(crate::inject::steam_deck_windows::SteamDeckWindowsManager::new)
                .handle(ev),
            // HID Xbox unless `windows_xbox_hid` picks XUSB. Guard on each arm: under XUSB,
            // `degrade_xbox_identity` has already folded One/Elite to Xbox360, so only Xbox360
            // reaches here and must fall through to XUSB.
            GamepadPref::Xbox360 if super::super::gamepad::windows_xbox_hid() => self
                .xbox_hid
                .get_or_insert_with(crate::inject::xbox_windows::XboxWindowsManager::new)
                .handle(ev),
            GamepadPref::XboxOne if super::super::gamepad::windows_xbox_hid() => self
                .xbox_one_hid
                .get_or_insert_with(|| {
                    crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                        crate::inject::xbox_windows::XboxWinProto::one_s(),
                    )
                })
                .handle(ev),
            GamepadPref::XboxElite if super::super::gamepad::windows_xbox_hid() => self
                .xbox_elite_hid
                .get_or_insert_with(|| {
                    crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                        crate::inject::xbox_windows::XboxWinProto::elite(),
                    )
                })
                .handle(ev),
            _ => return false,
        }
        true
    }

    /// Touchpad / motion for the pad's manager. No device yet → no-op. Xbox has no rich plane.
    pub(super) fn apply_rich(&mut self, kind: GamepadPref, rich: punktfunk_core::quic::RichInput) {
        match kind {
            GamepadPref::SteamController2 => {
                if let Some(m) = &mut self.steamctrl2 {
                    m.apply_rich(rich)
                }
            }
            GamepadPref::DualSense => {
                if let Some(m) = &mut self.dualsense_win {
                    m.apply_rich(rich)
                }
            }
            GamepadPref::DualSenseEdge => {
                if let Some(m) = &mut self.dualsense_edge_win {
                    m.apply_rich(rich)
                }
            }
            GamepadPref::DualShock4 => {
                if let Some(m) = &mut self.dualshock4_win {
                    m.apply_rich(rich)
                }
            }
            GamepadPref::SteamDeck => {
                if let Some(m) = &mut self.steamdeck_win {
                    m.apply_rich(rich)
                }
            }
            _ => {}
        }
    }

    /// A Triton device is live: its USB OUT is 1 kHz, so haptics poll at 1 ms.
    pub(super) fn sc2_active(&self) -> bool {
        self.steamctrl2.is_some()
    }

    pub(super) fn pump(
        &mut self,
        rumble: &mut impl FnMut(u16, u16, u16, u16, u16),
        hidout: &mut impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        if let Some(m) = &mut self.steamctrl2 {
            m.pump(&mut *rumble, &mut *hidout);
        }
        // All three HID Xbox identities. Rumble only (no rich plane). Missing
        // one is silent: the pad works and never rumbles.
        for m in [
            &mut self.xbox_hid,
            &mut self.xbox_one_hid,
            &mut self.xbox_elite_hid,
        ]
        .into_iter()
        .flatten()
        {
            m.pump(&mut *rumble, &mut *hidout);
        }
        if let Some(m) = &mut self.dualsense_win {
            m.pump(&mut *rumble, &mut *hidout);
        }
        if let Some(m) = &mut self.dualsense_edge_win {
            m.pump(&mut *rumble, &mut *hidout);
        }
        if let Some(m) = &mut self.dualshock4_win {
            m.pump(&mut *rumble, &mut *hidout);
        }
        if let Some(m) = &mut self.steamdeck_win {
            m.pump(&mut *rumble, &mut *hidout);
        }
    }

    /// Re-emit HID reports so a held-steady UMDF pad is not dropped.
    pub(super) fn heartbeat(&mut self) {
        let gap = std::time::Duration::from_millis(8);
        if let Some(m) = &mut self.steamctrl2 {
            m.heartbeat(gap);
        }
        if let Some(m) = &mut self.dualsense_win {
            m.heartbeat(gap);
        }
        if let Some(m) = &mut self.dualsense_edge_win {
            m.heartbeat(gap);
        }
        if let Some(m) = &mut self.dualshock4_win {
            m.heartbeat(gap);
        }
        if let Some(m) = &mut self.steamdeck_win {
            m.heartbeat(gap);
        }
    }
}
