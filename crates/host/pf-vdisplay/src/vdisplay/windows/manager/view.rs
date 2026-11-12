//! The manager's last committed slot map, for the readers that must not wait on a
//! transition: `/display/state`, admission, the connector choice and the IDD-push preempt's
//! wait. Every transition refreshes it as it commits ([`View::of`]); nothing here reaches
//! the OS.

use super::*;

/// One slot as the last commit left it.
struct Committed {
    slot: u32,
    generation: u64,
    mode: Mode,
    position: (i32, i32),
    life: lifecycle::State,
    joined: u32,
    client_fp: Option<[u8; 32]>,
}

/// The committed slots, in slot order, and whether their group keeps the host's monitors off
/// (an exclusive isolate is live).
#[derive(Default)]
pub(super) struct View(Vec<Committed>, bool);

impl View {
    pub(super) fn of(inner: &MgrInner) -> View {
        let slots = inner
            .slots
            .iter()
            .map(|(&slot, Slot { mon, life, joined })| Committed {
                slot,
                generation: mon.generation,
                mode: mon.mode,
                position: mon.position,
                life: *life,
                joined: *joined,
                client_fp: mon.client_fp,
            });
        View(slots.collect(), inner.group.ccd_exclusive)
    }

    /// `/display/state`'s rows in acquire (generation) order, linger countdowns read at `now`.
    pub(super) fn managed(&self, backend: &'static str, now: Instant) -> Vec<ManagedInfo> {
        let mut out: Vec<ManagedInfo> = self
            .0
            .iter()
            .map(|c| ManagedInfo {
                backend,
                mode: (c.mode.width, c.mode.height, c.mode.refresh_hz),
                state: c.life.label(),
                expires_in_ms: match c.life {
                    lifecycle::State::Lingering { until } => {
                        Some(until.saturating_duration_since(now).as_millis() as u64)
                    }
                    _ => None,
                },
                sessions: c.life.refs(),
                generation: c.generation,
                slot_id: c.slot,
                position: c.position,
            })
            .collect();
        out.sort_by_key(|i| i.generation);
        out
    }

    /// Each active slot and the device on it.
    pub(super) fn held(&self) -> Vec<(u32, Option<[u8; 32]>)> {
        self.0
            .iter()
            .filter(|c| matches!(c.life, lifecycle::State::Active { .. }))
            .map(|c| (c.slot, c.client_fp))
            .collect()
    }

    /// The device on each slot, while their group keeps the host's monitors off.
    pub(super) fn dark(&self) -> Vec<Option<[u8; 32]>> {
        let slots = self.0.iter().filter(|_| self.1);
        slots.map(|c| c.client_fp).collect()
    }

    /// Some slot lingered past its deadline at `now`.
    pub(super) fn expired(&self, now: Instant) -> bool {
        self.0
            .iter()
            .any(|c| matches!(c.life, lifecycle::State::Lingering { until } if now >= until))
    }

    /// The monitor of `generation` has more than one hold.
    pub(super) fn shared(&self, generation: u64) -> bool {
        (self.0.iter()).any(|c| c.generation == generation && c.life.refs() > 1)
    }

    /// `slot`'s committed life and its joined holds; `Idle` when it is not in the map.
    pub(super) fn life(&self, slot: u32) -> (lifecycle::State, u32) {
        self.0
            .iter()
            .find(|c| c.slot == slot)
            .map_or((lifecycle::State::Idle, 0), |c| (c.life, c.joined))
    }
}
