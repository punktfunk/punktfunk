//! The device list of a person's row, and the pairing list of its trust copy that follows it.
//!
//! A person's seat opens for a device that unlocked it with the person's password or its own
//! key. That device is present until it leaves or a time limit passes. The lists of such devices
//! lie behind the copy lock ([`super::copy_lock`]) and load on first use ([`with_devices`]).
//!
//! [`edit_devices`] drops the presence that expired and saves a change, then the trust copy's
//! pairing list follows who is admitted. When a person's session ends, so does every presence on
//! its row ([`end_presence`]). At supervisor start each list loads again, so a presence that ran
//! out while the supervisor was down admits nobody ([`LinuxBackend::load_devices`]).

use super::copy_lock::CopyGuard;
use super::{devices, trust_copy};
use super::{Inner, LinuxBackend, SeatAccount};
use crate::backend::BackendError;
use crate::model::{Ledger, SeatId};
use crate::unlock::DeviceList;

/// The wall clock in unix seconds, as presence is kept.
pub(super) fn unix_now() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `f` over `id`'s held device list, loaded on first use, with the lists' lock held throughout.
pub(super) fn with_devices<T>(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    f: impl FnOnce(&mut devices::Held, i64) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let now = unix_now();
    let mut all = guard.devices();
    let held = all
        .entry(id.clone())
        .or_insert_with(|| devices::load(&inner.state, id, now));
    f(held, now)
}

/// `id`'s list after `edit`, from the list with its expired presence dropped. A change is saved,
/// then the trust copy's pairing list follows who is admitted.
pub(super) fn edit_devices(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    edit: &mut dyn FnMut(&mut DeviceList, i64),
) -> Result<DeviceList, BackendError> {
    with_devices(inner, guard, id, |held, now| {
        let mut next = held.list.clone();
        next.expire(now);
        edit(&mut next, now);
        if next != held.list {
            held.commit(&inner.state, id, next.clone())?;
            trust_copy::rewrite_admitted(&inner.box_dir, &inner.state, id, &next.admitted())?;
        }
        Ok(next)
    })
}

/// A person's session ended: so has every presence on its row.
pub(super) fn end_presence(inner: &Inner, guard: &mut CopyGuard<'_>, id: &SeatId) {
    if let Err(error) = edit_devices(inner, guard, id, &mut |list, _| list.end_presence()) {
        tracing::warn!(seat = %id, %error, "presence not ended");
    }
}

/// A person's trust copy for `reader`, its pairing list holding only the devices admitted now.
pub(super) fn person_trust(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    reader: trust_copy::Reader,
) -> Result<bool, BackendError> {
    with_devices(inner, guard, id, |held, now| {
        let mut next = held.list.clone();
        if next.expire(now) {
            held.commit(&inner.state, id, next.clone())?;
        }
        let admitted = next.admitted();
        trust_copy::refresh_person_trust(&inner.box_dir, &inner.state, id, reader, &admitted)
    })
}

impl LinuxBackend {
    /// Loads each person's row's device list and writes its trust copy's pairing list from it,
    /// so a presence that ran out while the supervisor was down admits nobody.
    pub(super) fn load_devices(&self, ledger: &Ledger) {
        for seat in ledger
            .seats
            .iter()
            .filter(|s| SeatAccount::of(&s.kind) == SeatAccount::Person)
        {
            let mut guard = self.inner.copy_lock.lock();
            let written = with_devices(&self.inner, &mut guard, &seat.id, |held, _| {
                let admitted = held.list.admitted();
                trust_copy::rewrite_admitted(
                    &self.inner.box_dir,
                    &self.inner.state,
                    &seat.id,
                    &admitted,
                )
            });
            if let Err(error) = written {
                tracing::warn!(seat = %seat.id, %error, "pairing list not written");
            }
        }
    }
}
