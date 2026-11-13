//! The virtual-pad managers behind [`Pads`](super::Pads), one body for every OS. Each OS lists
//! its managers once, in [`os::MANAGERS`]: the kind each builds and its constructor. A manager is
//! built on its kind's first event and owns only the indices routed to it. Xbox360 over
//! uinput or XUSB is the common default and stays in `Pads`.

use super::*;
use crate::inject::uhid_manager::{PadProto, UhidManager};
use punktfunk_core::input::GamepadEvent;
use punktfunk_core::quic::{HidOutput, RichInput};

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod os;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod os {
    /// Off Linux and Windows a pad is Xbox360 or nothing.
    pub(super) const MANAGERS: &[super::ManagerRow] = &[];
}

/// One virtual-pad manager as an object, so one table holds managers of every backend.
trait PadManager {
    fn handle(&mut self, ev: &GamepadEvent);
    /// Never creates a pad: a rich sample for an index with no device is dropped.
    fn apply_rich(&mut self, rich: RichInput);
    fn pump(
        &mut self,
        rumble: &mut dyn FnMut(u16, u16, u16, u16, u16),
        hidout: &mut dyn FnMut(HidOutput),
    );
    fn heartbeat(&mut self, max_gap: std::time::Duration);
    /// A refresh of OS-slot pad `pad` that changed nothing: proof its client is alive.
    fn heard(&mut self, _pad: usize) {}
}

impl<B: PadProto> PadManager for UhidManager<B> {
    fn handle(&mut self, ev: &GamepadEvent) {
        UhidManager::handle(self, ev);
    }
    fn apply_rich(&mut self, rich: RichInput) {
        UhidManager::apply_rich(self, rich);
    }
    fn pump(
        &mut self,
        rumble: &mut dyn FnMut(u16, u16, u16, u16, u16),
        hidout: &mut dyn FnMut(HidOutput),
    ) {
        UhidManager::pump(self, rumble, hidout);
    }
    fn heartbeat(&mut self, max_gap: std::time::Duration) {
        UhidManager::heartbeat(self, max_gap);
    }
    fn heard(&mut self, pad: usize) {
        UhidManager::heard(self, pad);
    }
}

/// The uinput Xbox pads: evdev holds the last state, so no rich plane and no heartbeat.
#[cfg(target_os = "linux")]
impl PadManager for crate::inject::gamepad::GamepadManager {
    fn handle(&mut self, ev: &GamepadEvent) {
        crate::inject::gamepad::GamepadManager::handle(self, ev);
    }
    fn apply_rich(&mut self, _rich: RichInput) {}
    fn pump(
        &mut self,
        rumble: &mut dyn FnMut(u16, u16, u16, u16, u16),
        _hidout: &mut dyn FnMut(HidOutput),
    ) {
        self.pump_rumble(rumble);
    }
    fn heartbeat(&mut self, _max_gap: std::time::Duration) {}
}

/// A manager's constructor. `None` = this host builds no device of that kind, and the caller's
/// Xbox360 default takes it.
type Build = fn() -> Option<Box<dyn PadManager>>;

/// One row of an OS's manager table: the kind, and its manager's constructor.
type ManagerRow = (GamepadPref, Build);

/// A table constructor's manager.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn boxed<M: PadManager + 'static>(m: M) -> Option<Box<dyn PadManager>> {
    Some(Box::new(m))
}

/// The managers of [`os::MANAGERS`], in its order; `None` until the kind's first event.
pub(super) struct PadBackends {
    built: Vec<Option<Box<dyn PadManager>>>,
}

impl Default for PadBackends {
    fn default() -> PadBackends {
        PadBackends {
            built: os::MANAGERS.iter().map(|_| None).collect(),
        }
    }
}

impl PadBackends {
    /// The manager `kind` routes to, `None` until built, and its constructor. `None` = no
    /// backend of this kind here.
    fn manager_for(
        &mut self,
        kind: GamepadPref,
    ) -> Option<(&mut Option<Box<dyn PadManager>>, Build)> {
        let i = os::MANAGERS.iter().position(|(k, _)| *k == kind)?;
        Some((&mut self.built[i], os::MANAGERS[i].1))
    }

    /// Route a pad event to the manager for `kind`, building it on first use. `false` = no
    /// backend of this kind here; the caller's Xbox360 default takes it.
    pub(super) fn route_handle(&mut self, kind: GamepadPref, ev: &GamepadEvent) -> bool {
        let Some((slot, build)) = self.manager_for(kind) else {
            return false;
        };
        if slot.is_none() {
            *slot = build();
        }
        let Some(m) = slot else {
            return false;
        };
        m.handle(ev);
        true
    }

    /// Touchpad / motion for the pad's manager. No device yet → no-op. Xbox has no rich plane.
    pub(super) fn apply_rich(&mut self, kind: GamepadPref, rich: RichInput) {
        if matches!(
            kind,
            GamepadPref::Xbox360 | GamepadPref::XboxOne | GamepadPref::XboxElite
        ) {
            return;
        }
        if let Some((Some(m), _)) = self.manager_for(kind) {
            m.apply_rich(rich);
        }
    }

    /// An unchanged refresh of OS-slot pad `slot`, for the manager of `kind`. No device yet or
    /// no backend of this kind: nothing to keep alive.
    pub(super) fn heard(&mut self, kind: GamepadPref, slot: u8) {
        if let Some((Some(m), _)) = self.manager_for(kind) {
            m.heard(usize::from(slot));
        }
    }

    pub(super) fn pump(
        &mut self,
        rumble: &mut impl FnMut(u16, u16, u16, u16, u16),
        hidout: &mut impl FnMut(HidOutput),
    ) {
        for m in self.built.iter_mut().flatten() {
            m.pump(&mut *rumble, &mut *hidout);
        }
    }

    /// Re-emit HID reports so kernel/SDL do not drop a held-steady UHID/UMDF pad.
    pub(super) fn heartbeat(&mut self) {
        let gap = std::time::Duration::from_millis(8);
        for m in self.built.iter_mut().flatten() {
            m.heartbeat(gap);
        }
    }
}
