//! The copy writers' lock, and the device lists that lie behind it.
//!
//! A removal holds the level throughout, the keep-level thread for each seat it refreshes, and a
//! start or adoption while it writes a trust copy, an environment file or the box's library: a
//! refresh never writes a removed seat's files back, and two writers never share a temp file or
//! put back an environment file's old token. Each person's row's device list is reached only
//! through a held [`Level`], so a change, its save and the trust copy it rewrites always run
//! under the level first, then the lists' own lock. The level is not re-entrant: a path that
//! already holds one is handed it ([`CopyLocks::under`]).

use super::devices;
use crate::model::SeatId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

type Lists = HashMap<SeatId, devices::Held>;

#[derive(Default)]
pub(super) struct CopyLocks {
    level: Mutex<()>,
    devices: Mutex<Lists>,
}

/// The level, held.
pub(super) struct Level<'a> {
    _held: MutexGuard<'a, ()>,
    devices: &'a Mutex<Lists>,
}

impl CopyLocks {
    pub(super) fn level(&self) -> Level<'_> {
        Level {
            _held: self.level.lock().unwrap_or_else(|e| e.into_inner()),
            devices: &self.devices,
        }
    }

    /// `f` under `held`, or under the level taken for `f` alone.
    pub(super) fn under<T>(
        &self,
        held: Option<&mut Level<'_>>,
        f: impl FnOnce(&mut Level<'_>) -> T,
    ) -> T {
        match held {
            Some(level) => f(level),
            None => f(&mut self.level()),
        }
    }
}

impl Level<'_> {
    /// Each person's row's device list, loaded on first use.
    pub(super) fn devices(&mut self) -> MutexGuard<'_, Lists> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }
}
