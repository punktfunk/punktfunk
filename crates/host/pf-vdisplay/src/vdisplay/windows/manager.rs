//! Host-lifetime virtual-display ownership: one process-wide slot map whose
//! slots run [`lifecycle::State`], the linger timer, and a typed
//! [`ControlDevice`].
//!
//! [`VirtualDisplayManager`] is the singleton ([`vdm`]). The session holds a
//! [`MonitorLease`]; `Drop` releases the slot's refcount. A stale lease — its
//! monitor was preempted and recreated under it — is a no-op, so it cannot tear
//! down the live monitor.
//!
//! Driver IOCTL and the per-monitor REMOVE key sit behind [`VdisplayDriver`].
//! Topology, GDI/CCD glue, and generation-stamped leases are driver-neutral.
//! The isolate ladder reaches the OS through [`CcdOps`].
//!
//! Lock order: `setup_lock` → `turn` → {`state`, `view`, `device`, `pinger`,
//! `exclusive_watch`}, the five never nested. A [`Transition`] holds `turn` across its act
//! and `state` only to check the slot map out and back in; readers take `view` alone.
//! Evidence: `design/display-management.md`.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, LUID, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use super::{DisplayOwnership, Mode, VirtualOutput};
use crate::driver::ControlDevice;
use crate::lifecycle::{self, Acquire, Release};
use pf_win_display::topology_churn::{self, Finished, Outcome};
use pf_win_display::win_display::{
    count_other_active, force_extend_topology, resolve_gdi_name, restore_displays_ccd,
    set_active_mode, set_virtual_primary_ccd, wait_mode_settled, wait_target_departed,
    CcdTargetKey, IsolateOutcome, SavedConfig,
};

#[path = "manager/ccd.rs"]
mod ccd;
use ccd::{CcdOps, WinCcd};

#[path = "manager/driver.rs"]
mod driver;
pub(crate) use driver::{AddedMonitor, MonitorKey, VdisplayDriver};

#[path = "manager/instance.rs"]
mod instance;
use instance::claim_instance;
pub use instance::claim_instance_eagerly;

#[path = "manager/knobs.rs"]
mod knobs;
pub(crate) use knobs::unconfigured_topology;
use knobs::{linger_for, topology_action};

#[path = "manager/view.rs"]
mod view;
use view::View;

#[path = "manager/ladder.rs"]
mod ladder;
use ladder::{AddError, Found, Ladder, Next, Reason, Regroup, Seen, SlotChange};

/// One live virtual monitor, owned by manager state (not by a session).
/// No `Drop`: [`teardown_removed`](VirtualDisplayManager::teardown_removed)
/// must fire the REMOVE IOCTL; a bare drop orphans the driver-side monitor.
/// Group topology (CCD snapshot, DDC, PnP) lives on [`GroupState`], not here.
struct Monitor {
    key: MonitorKey,
    target_id: u32,
    /// IddCx DISPLAY adapter LUID from the ADD reply — not the render GPU
    /// (that one is only in the shared frame header). Do not compare picks
    /// against this.
    luid: LUID,
    /// SET_RENDER_ADAPTER pin at ADD (`None` = no GPU selectable). Never
    /// re-issued on reuse — [`warn_if_pick_moved`] compares the current pick
    /// against this.
    render_pin: Option<LUID>,
    /// ADD-time v5 hardware-cursor flag. Survives re-arrival so the new
    /// monitor keeps the cursor channel.
    hw_cursor: bool,
    /// OS target carries an irrevocable hardware-cursor declare: DWM drops
    /// the pointer from frames forever (survives REMOVE→ADD via the stable
    /// target id). A session without the cursor channel must self-composite.
    cursor_excluded: bool,
    /// WUDFHost pid from the ADD reply. One process hosts every slot's
    /// publisher, so its death is all-slot shared fate.
    wudf_pid: u32,
    gdi_name: Option<String>,
    /// Mode the OS committed, not the one the client asked for. Read back
    /// through [`committed_mode_or`]. Not the resize discriminator — see
    /// `requested_mode`.
    mode: Mode,
    /// Mode asked for at the last ADD/mode-set.
    ///
    /// [`needs_resize`] must see this, not only `mode`. `set_active_mode`
    /// commits the highest advertised refresh ≤ requested, so the two fields
    /// can disagree for the monitor's life. Diffing acquire against `mode`
    /// alone turns every rebuild (`build_pipeline_with_retry` re-asks the
    /// negotiated rate) into an in-place resize then REMOVE→ADD, exhausting
    /// the IddCx slot pool (0x80070490).
    requested_mode: Mode,
    /// Driver-resolved monitor id (EDID serial / ConnectorIndex).
    resolved_monitor_id: u32,
    /// Desktop origin from the group layout. `(0,0)` until a multi-slot arrange.
    position: (i32, i32),
    /// Generation stamp; a [`MonitorLease`] releases only if this still matches.
    generation: u64,
    /// Device whose acquire created this monitor. Its `keep_alive` overlay decides
    /// the linger at release ([`linger_for`]); `None` follows the host.
    client_fp: Option<[u8; 32]>,
    /// A further screen of a client, held beside its first ([`VirtualDisplayManager::place_beside`]).
    beside: Option<Beside>,
}

/// Where a further screen's monitor sits: `offset` from the monitor of client `anchor`, as the
/// client's own monitors sit.
#[derive(Clone, Copy, Debug)]
struct Beside {
    anchor: Option<[u8; 32]>,
    offset: (i32, i32),
}

impl Monitor {
    /// The monitor an ADD reply describes, at the origin with no GDI name yet, asked and
    /// recorded at `mode` until the read-back. `cursor_excluded` comes from the reply: the
    /// driver's per-target declare registry is ground truth.
    fn arrived(
        added: &AddedMonitor,
        mode: Mode,
        render_pin: Option<LUID>,
        hw_cursor: bool,
        generation: u64,
    ) -> Monitor {
        Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            hw_cursor,
            cursor_excluded: added.cursor_excluded,
            wudf_pid: added.wudf_pid,
            gdi_name: None,
            mode,
            requested_mode: mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: (0, 0),
            generation,
            client_fp: None,
            beside: None,
        }
    }

    /// This monitor's complete CCD identity — every display-global helper selects paths by it
    /// (target ids alone are only unique per adapter; `luid` is the IddCx display adapter's).
    fn ccd_key(&self) -> CcdTargetKey {
        CcdTargetKey::from_luid_parts(self.luid.LowPart, self.luid.HighPart, self.target_id)
    }

    /// The capture target handed to a session (`None` until the GDI name resolves on a WDDM GPU).
    fn target(&self) -> Option<pf_frame::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| pf_frame::dxgi::WinCaptureTarget {
                adapter_luid: pf_frame::dxgi::pack_luid(self.luid),
                gdi_name: n,
                target_id: self.target_id,
                wudf_pid: self.wudf_pid,
                cursor_excluded: self.cursor_excluded,
            })
    }
}

/// One live or kept monitor. [`lifecycle::State`] decides every refcount, linger and pin
/// transition; this file owns the monitor and the driver calls. Idle is absence from the map:
/// a slot whose `life` reaches Idle leaves it, and is torn down, under the same lock.
struct Slot {
    mon: Monitor,
    life: lifecycle::State,
    /// The holds in `life` that [`VirtualDisplayManager::join`] took for other clients.
    joined: u32,
}

/// What an acquire on this slot is, asked of the lifecycle without taking a hold: Create when
/// empty, Join when Active, Reuse when kept. Windows answers Reuse with preempt-and-recreate:
/// a reused IddCx swap-chain is dead.
fn acquire_kind(slot: Option<&Slot>) -> Acquire {
    let mut life = slot.map_or_else(lifecycle::State::default, |s| s.life);
    life.acquire()
}

/// What one acquire asks for.
struct Ask {
    slot: u32,
    mode: Mode,
    client_fp: Option<[u8; 32]>,
    client_hdr: Option<pf_frame::HdrMeta>,
    hw_cursor: bool,
    quit: Option<Arc<AtomicBool>>,
}

/// What the ladder's acts carry from rung to rung.
#[derive(Default)]
struct Rungs {
    /// The monitor the ADD made, completed rung by rung.
    mon: Option<Monitor>,
    /// A re-arrival's predecessor and its holds, out of the map until the commit.
    old: Option<Slot>,
    /// The first failed ADD: what a rollback or a failure reports.
    err: Option<anyhow::Error>,
    /// A re-arrival's render pin, kept for its rollback.
    render_pin: Option<LUID>,
    /// The new monitor heads the group.
    first: bool,
    /// The first member's pre-mutation active set ([`standby_baseline`]).
    baseline: Option<Vec<CcdTargetKey>>,
    settle_start: Option<Instant>,
}

impl Rungs {
    /// The verified-state wait before capture opens. A rejected mode burns the ceiling.
    fn settle(&mut self, ceiling: Duration) -> Seen {
        let mon = self.mon.as_ref().expect("the settle rung follows an ADD");
        let start = Instant::now();
        let settled = wait_mode_settled(mon.ccd_key(), mon.requested_mode, ceiling);
        let settle_ms = start.elapsed().as_millis() as u64;
        if self.old.is_some() {
            tracing::info!(
                settle_ms,
                verified = settled,
                "re-arrival topology settle (verified-state wait)"
            );
        } else {
            tracing::info!(
                settle_ms,
                verified = settled,
                "topology settle (verified-state wait)"
            );
        }
        self.settle_start = Some(start);
        Seen::Settled
    }

    /// Keeps a failed ADD's error. A re-arrival's first one gains its context, and a failed
    /// rollback adds its own cause to it.
    fn failed(&mut self, slot: u32, e: anyhow::Error) {
        let err = match (self.old.is_some(), self.err.take()) {
            (false, _) => e,
            (true, None) => {
                let e = e.context("re-arrival ADD at the new mode");
                tracing::warn!(
                    slot,
                    error = %format!("{e:#}"),
                    "re-arrival ADD failed — rolling back to the previous mode"
                );
                e
            }
            (true, Some(first)) => {
                tracing::error!(
                    slot,
                    error = %format!("{e:#}"),
                    "re-arrival rollback ADD also failed — the slot now has NO monitor"
                );
                first.context(format!("rollback also failed: {e:#}"))
            }
        };
        self.err = Some(err);
    }

    fn error(&mut self) -> anyhow::Error {
        self.err
            .take()
            .unwrap_or_else(|| anyhow::anyhow!("virtual-display ADD failed"))
    }
}

/// How one climb ended.
enum Climb {
    Done(Result<VirtualOutput>),
    /// The control device died under the ADD or its seat adapter left; the error is the
    /// retire's cause.
    Retire(anyhow::Error),
}

/// The acquire's slot as the ladder reads it. A live slot is joined unless its WUDFHost died:
/// it is a live lease (a build retry, a concurrent session), and REMOVE→ADD on it ends in
/// 0x80070490. WUDFHost death is all-slot shared fate.
fn found(t: &MgrInner, ask: &Ask) -> Found {
    let slot = t.slots.get(&ask.slot);
    match (acquire_kind(slot), slot) {
        (Acquire::Reuse, _) => Found::Kept,
        (Acquire::Join, Some(s)) => Found::Live {
            host_alive: wudf_alive(s.mon.wudf_pid),
            resize: needs_resize(s.mon.requested_mode, s.mon.mode, ask.mode),
            resolved: s.mon.gdi_name.is_some(),
        },
        _ => Found::Empty,
    }
}

/// The fail-closed backstop for a session that got past admission: `max_displays` counts
/// active, lingering and pinned slots, so one live slot never trips it.
fn budget_spent(live: usize, max: u32) -> anyhow::Error {
    anyhow::anyhow!(
        "display budget exhausted: {live} display(s) live/kept, max_displays = {max} — freeing \
         one (session end, linger expiry, or /display/release) admits the next"
    )
}

/// This process hosts a seat inside its own RDP session.
fn in_seat_session() -> bool {
    pf_paths::seat::role().runs_seat_session()
        && pf_win_display::console_session_mismatch().is_some()
}

/// A seat session that lists no display path has lost the config its adapter published, and no
/// monitor added to it can get one. Dropping the connection makes the keeper reattach, which
/// brings up a new adapter with a lit placeholder for the caller's next attempt.
fn rebuild_empty_seat_session() {
    if !in_seat_session() || !pf_win_display::win_display::session_has_no_paths() {
        return;
    }
    match pf_win_display::disconnect_own_session() {
        Ok(()) => tracing::warn!("seat session has no display paths — reconnecting it"),
        Err(e) => tracing::warn!(error = %e, "seat session with no display paths stayed connected"),
    }
}

/// Says so when this process is outside the active console session, where display
/// activation, mode-set and capture fail. A seat host runs there on purpose.
fn warn_outside_console_session() {
    let Some((own, console)) = pf_win_display::console_session_mismatch() else {
        return;
    };
    if pf_paths::seat::role().is_seat_host() {
        tracing::warn!(
            own_session = own,
            console_session = console,
            "punktfunk seat host is intentionally outside the active console session — \
             the seat keeper must keep this RDP session active; display activation errors \
             that follow are real and mean the keeper or session is unavailable"
        );
    } else {
        tracing::error!(
            own_session = own,
            console_session = console,
            "punktfunk-host is NOT in the active console session — display activation, \
             mode-set and capture will fail (disconnected RDP session?). Reconnect the \
             console (`tscon {own} /dest:console`) or run the host via the installed \
             service, which follows the console session"
        );
    }
}

/// Group topology for the one Windows desktop. First slot isolates and
/// captures; last member restores. Per-monitor restore would flash physical
/// panels between sibling sessions.
#[derive(Default)]
struct GroupState {
    /// First slot's pre-isolate snapshot; last-member teardown restores it.
    /// `Some` also means an exclusive isolate is live, so add/remove re-issues
    /// isolate over the grown/shrunk set.
    ccd_saved: Option<SavedConfig>,
    /// Panels that acknowledged DDC/CI off at first isolate. Last-member
    /// teardown wakes them after CCD restore iff > 0.
    ddc_panels_off: u32,
    /// PnP instance ids disabled at first isolate. Last-member teardown
    /// re-enables them before the CCD restore.
    pnp_disabled: Vec<String>,
    /// AMD connector emulation pinned at first isolate. Last-member teardown
    /// owes the unlock; pinned emulation outlives the process (crash journal).
    edid_locked: bool,
    /// `ccd_saved` came from an exclusive isolate. `Primary` also snapshots
    /// but keeps physicals lit — the re-assert watchdog must not "fix" those.
    ccd_exclusive: bool,
    /// Topology the first member opened the group with: its device's pin over the
    /// host's. Later members and swaps follow it rather than deciding again.
    topology: crate::policy::Topology,
}

/// One stage of [`VirtualDisplayManager::resolve_target_gdi`]'s ladder: poll for the target's GDI name until
/// the 3 s ceiling. 50 ms sampling (latency plan P0.5) — a typical activation resolves on an early
/// poll, so finer sampling shaves ~150 ms off every stage crossing.
fn poll_gdi_name(key: CcdTargetKey) -> Option<String> {
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(50));
        if let Some(n) = resolve_gdi_name(key) {
            return Some(n);
        }
    }
    None
}

/// Test-only: fail the next N CCD isolates against the real driver.
/// `#[cfg(test)]` so live hardware tests can reach it without a production
/// knob that could leave isolation silently disabled.
#[cfg(test)]
pub(crate) static FAIL_NEXT_ISOLATES: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// How long a slot transition's isolate holds descriptor-following: the swap-chain bounce the
/// write causes lands after the write returns.
const ISOLATE_HOLD: Duration = Duration::from_secs(3);

/// Every CCD isolate in this file, as one topology transaction: descriptor-following holds for
/// `hold`, and the generation moves only when the verification read saw a path switch off.
/// `store` stores the isolate for the connected set (see
/// [`CcdOps::isolate_displays_ccd_checked`]). Returns the pre-isolate snapshot (`None` when
/// nothing was attempted) and the finished transaction. Tests fail the next N isolates
/// through [`FAIL_NEXT_ISOLATES`], so an injected failure passes the same gates a real one
/// does.
fn isolate_txn(
    ccd: &dyn CcdOps,
    reason: &'static str,
    keep: &[CcdTargetKey],
    hold: Duration,
    store: bool,
) -> (Option<SavedConfig>, Finished) {
    let txn = topology_churn::begin(reason, hold);
    #[cfg(test)]
    let injected = FAIL_NEXT_ISOLATES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok();
    #[cfg(not(test))]
    let injected = false;
    let isolated = if injected {
        tracing::warn!(
            keep = ?keep,
            reason,
            "TEST fault injection: forcing the CCD isolate to fail"
        );
        None
    } else {
        ccd.isolate_displays_ccd_checked(keep, store)
    };
    let finished =
        topology_churn::finish(txn, isolate_txn_outcome(isolated.as_ref().map(|(_, o)| *o)));
    (isolated.map(|(saved, _)| saved), finished)
}

/// The transaction outcome an isolate observed: only a path that verifiably switched off counts
/// as a change.
fn isolate_txn_outcome(outcome: Option<IsolateOutcome>) -> Outcome {
    match outcome {
        Some(IsolateOutcome::Verified { deactivated, .. }) if deactivated > 0 => Outcome::Changed,
        Some(IsolateOutcome::Verified { .. } | IsolateOutcome::NothingActive) => Outcome::Unchanged,
        Some(IsolateOutcome::Unverified { .. }) | None => Outcome::Unknown,
    }
}

/// Re-assert rounds before the watchdog CONCEDES the fixed cadence (immunity plan WP10 item 6):
/// past this, something on the host demonstrably restores the path every cycle, and re-fighting
/// it every two seconds only multiplies swap-chain bounces for the stream. Counted two ways and
/// the larger wins: consecutive rounds, and rounds within [`REASSERT_WINDOW`] — a fight that
/// alternates "lost" and "stable again" every cycle (measured on `.173`: the TV re-activates
/// ~1 s after each re-assert) never grows a consecutive count, yet is the same fight.
const REASSERT_BREAKER_ROUNDS: u32 = 4;
/// The rolling window the second count uses.
const REASSERT_WINDOW: Duration = Duration::from_secs(60);

/// What one exclusive-watch cycle owes the group, per [`ReassertBreaker::on_cycle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cycle {
    /// No non-managed display is active. `was_fighting` counts the re-asserts it ends.
    Stable { was_fighting: u32 },
    /// The breaker tripped earlier: mutate nothing for the group's life.
    Conceded,
    /// The breaker trips now: concede the isolate, the stream continues on the shared desktop.
    Concede { rounds: u32 },
    /// Re-assert the isolate; `round` is the larger of the two counts.
    Reassert { round: u32 },
}

/// Re-assert breaker for one exclusive group (immunity plan WP10 item 6). Pure: the watch
/// feeds it each cycle's survivor count and acts on the [`Cycle`] it returns.
#[derive(Default)]
struct ReassertBreaker {
    /// Consecutive re-assert cycles; a stable cycle resets it.
    fighting: u32,
    /// Every re-assert's stamp, for the rolling count (the alternating fight).
    recent: std::collections::VecDeque<Instant>,
    /// Tripped: no further write, nor stream rebuild, for the group's life. A doubling
    /// back-off still cost the client a capture+encoder rebuild per re-assert.
    conceded: bool,
}

impl ReassertBreaker {
    /// One cycle that saw `survivors` non-managed displays active at `now`.
    fn on_cycle(&mut self, survivors: u32, now: Instant) -> Cycle {
        if survivors == 0 {
            let was_fighting = std::mem::take(&mut self.fighting);
            return Cycle::Stable { was_fighting };
        }
        if self.conceded {
            return Cycle::Conceded;
        }
        self.fighting += 1;
        self.recent.push_back(now);
        while self
            .recent
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= REASSERT_WINDOW)
        {
            self.recent.pop_front();
        }
        let rounds = self.fighting.max(self.recent.len() as u32);
        if rounds >= REASSERT_BREAKER_ROUNDS {
            self.conceded = true;
            return Cycle::Concede { rounds };
        }
        Cycle::Reassert { round: rounds }
    }
}

/// The exclusive watch's wait: until the display actor publishes a new topology snapshot
/// (immunity plan WP9), else `interval`, in 250 ms slices so stop+join stays bounded.
/// `false` once `stop` is set.
fn wait_watch_cycle(stop: &AtomicBool, interval: Duration) -> bool {
    let seen = pf_win_display::display_events::snapshot().generation;
    let mut slept = Duration::ZERO;
    while slept < interval {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let slice = Duration::from_millis(250).min(interval - slept);
        match pf_win_display::display_events::wait_for_change(seen, slice) {
            Some(s) if s.generation > seen => break,
            Some(_) => {}
            None => thread::sleep(slice), // actor not running: plain cadence
        }
        slept += slice;
    }
    true
}

/// Mode `target_id` is actually running, for a caller about to record it.
/// `requested` is the fallback when the read-back cannot be trusted.
///
/// Every store of `Monitor.mode` owes this call. `set_active_mode` commits
/// the highest advertised refresh ≤ requested, and `wait_mode_settled`
/// verifies resolution only — a true settle says nothing about refresh.
///
/// Deliberately narrowed to the REFRESH. A read-back that FAILS, or that reports a different
/// RESOLUTION, keeps `requested`: the create path proceeds even when its settle timed out, so the
/// OS may still be sitting on its own default there, and recording that would hand the capturer +
/// the client a size nobody negotiated. The capturer already re-resolves the live size on its own
/// (`active_resolution` poll, game-capture GB1); the refresh is the field only this read-back can
/// answer.
fn committed_mode_or(key: CcdTargetKey, requested: Mode) -> Mode {
    let Some((width, height, refresh_hz)) = pf_win_display::win_display::active_mode(key) else {
        return requested;
    };
    if (width, height) != (requested.width, requested.height) {
        tracing::warn!(
            target = %key,
            requested = format!("{}x{}", requested.width, requested.height),
            active = format!("{width}x{height}"),
            "the OS is not running the requested resolution after the settle — recording the \
             requested mode (the capturer re-resolves the live size itself)"
        );
        return requested;
    }
    if refresh_hz != requested.refresh_hz {
        tracing::warn!(
            target = %key,
            requested_hz = requested.refresh_hz,
            committed_hz = refresh_hz,
            "the OS committed a different refresh than requested — recording what the display \
             actually runs; the client streams below the rate it asked for"
        );
    }
    Mode {
        width,
        height,
        refresh_hz,
    }
}

/// Whether an acquire for `want` on a live monitor needs a mid-stream resize.
///
/// Matching either `requested` or `committed` is a join: the session re-asks
/// the negotiated mode on every rebuild, and that cannot beat what ADD got.
/// Keying on `committed` alone turns every re-acquire on a refresh-clamping
/// box into an in-place resize then REMOVE→ADD, forever.
fn needs_resize(requested: Mode, committed: Mode, want: Mode) -> bool {
    want != requested && want != committed
}

/// Slot map plus the one group record, checked out together by a [`Transition`]: every
/// group mutation happens on a slot transition.
#[derive(Default)]
struct MgrInner {
    /// Live/kept slots, keyed by identity slot (`1..=15`) or `0` for
    /// anonymous/GameStream (at most one; no identity to find another slot by).
    slots: BTreeMap<u32, Slot>,
    group: GroupState,
}

impl MgrInner {
    /// Live target keys in acquire (generation) order — the CCD isolate keep-set + the layout member order.
    fn target_keys(&self) -> Vec<CcdTargetKey> {
        let mut mons: Vec<&Monitor> = self.slots.values().map(|s| &s.mon).collect();
        mons.sort_by_key(|m| m.generation);
        mons.iter().map(|m| m.ccd_key()).collect()
    }

    /// The managed keep-set once `new` joins: every live sibling, then `new`.
    fn keep_with(&self, new: CcdTargetKey) -> Vec<CcdTargetKey> {
        let mut keep = self.target_keys();
        keep.push(new);
        keep
    }

    /// The exclusive watch's snapshot gates: the keep-set and how many non-managed displays
    /// are active. `None` mutates nothing this cycle: no exclusive isolate is live, or the
    /// display snapshot is unknown, which is not the same as stable.
    fn exclusive_survivors(&self, ccd: &dyn CcdOps) -> Option<(Vec<CcdTargetKey>, u32)> {
        if self.group.ccd_saved.is_none() || !self.group.ccd_exclusive {
            return None;
        }
        let keep = self.target_keys();
        if keep.is_empty() {
            return None;
        }
        // Not FRESH: the actor's last query failed, or it never published.
        let snap = ccd.snapshot_or_query();
        if !snap.is_fresh() && snap.generation > 0 {
            tracing::debug!(
                failures = snap.failures,
                "exclusive re-assert watchdog: display snapshot is last-known-good — \
                 topology state unknown this cycle, mutating nothing"
            );
            return None;
        }
        // Under a live exclusive isolate our own targets are active, so a snapshot
        // without them is an untrustworthy read, not an empty desk.
        if !keep.iter().any(|k| snap.target(*k).is_some()) {
            tracing::debug!(
                "exclusive re-assert watchdog: the snapshot carries none of our \
                 targets — unknown this cycle, mutating nothing"
            );
            return None;
        }
        let survivors = snap.count_other_active(&keep);
        Some((keep, survivors))
    }

    /// One re-assert through [`isolate_txn`], stored for the connected set: Windows re-applies
    /// the stored layout when a display re-plugs itself, so storing the isolate ends that fight.
    /// Only a finished `Changed` bumps [`topology_reassert_gen`] and parks the re-lit display's
    /// devnode for the session; a change seen past the hold deadline finishes as unknown.
    fn reassert_isolate(&mut self, ccd: &dyn CcdOps, keep: &[CcdTargetKey], hold: Duration) {
        let (_, finished) = isolate_txn(ccd, "exclusive-reassert", keep, hold, true);
        if finished.outcome != Outcome::Changed {
            return;
        }
        // The forced re-commit hands the IDD path a fresh swap-chain: the session
        // re-attaches capture instead of streaming a frozen frame.
        TOPOLOGY_REASSERT_GEN.fetch_add(1, Ordering::Relaxed);
        // Quiets the re-lit display's wake events; it does not stop a re-plug re-resolving
        // the layout. Leased like the acquire's park, re-enabled at teardown. With no
        // baseline here, the park reaches every inactive monitor the isolate switched off,
        // which is the operator's `pnp_disable_monitors` to allow, as on the acquire pass.
        let prefs = crate::policy::prefs();
        if !prefs.standby_sink_neutralise() || !prefs.pnp_disable_monitors() {
            return;
        }
        let parked = ccd.disable_connected_inactive(keep, &[], finished.generation);
        if !parked.is_empty() {
            tracing::info!(
                parked = parked.len(),
                "exclusive re-assert: PnP-disabled the re-lit display for the \
                 session (re-enabled at teardown)"
            );
        }
        for id in parked {
            if !self.group.pnp_disabled.contains(&id) {
                self.group.pnp_disabled.push(id);
            }
        }
    }
}

/// One slot or group transition. It holds `turn` from decide to commit and checks the slot
/// map out of `state` for that span, so its act (every sleep, DDC, devnode, CCD and IOCTL)
/// runs with `state` released. Dropping it commits: the map goes back into `state`, the view
/// is refreshed and its row changes are reported ([`display_changes`]), before the turn is
/// released. A lease dropped inside one deadlocks.
struct Transition<'m> {
    mgr: &'m VirtualDisplayManager,
    inner: MgrInner,
    _turn: MutexGuard<'m, ()>,
}

impl Deref for Transition<'_> {
    type Target = MgrInner;
    fn deref(&self) -> &MgrInner {
        &self.inner
    }
}

impl DerefMut for Transition<'_> {
    fn deref_mut(&mut self) -> &mut MgrInner {
        &mut self.inner
    }
}

impl Drop for Transition<'_> {
    fn drop(&mut self) {
        let inner = std::mem::take(&mut self.inner);
        let view = View::of(&inner);
        let (backend, now) = (self.mgr.driver.name(), Instant::now());
        let after = view.managed(backend, now);
        *self.mgr.state.lock().unwrap_or_else(|e| e.into_inner()) = inner;
        let before = std::mem::replace(
            &mut *self.mgr.view.lock().unwrap_or_else(|e| e.into_inner()),
            view,
        )
        .managed(backend, now);
        // After the view, so the console's refetch on the event reads this commit. The sink
        // takes only the event bus's own lock and never blocks, so `turn` held here is safe.
        for event in display_changes(&before, &after) {
            crate::emit_display_event(event);
        }
    }
}

/// What a commit changed in the `/display/state` rows, keyed by generation (the
/// `/display/release` slot): one `Released` for the rows it dropped, then a `Created` per
/// row it added. A re-arrival keeps its generation, so, like an in-place resize, it is neither.
fn display_changes(before: &[ManagedInfo], after: &[ManagedInfo]) -> Vec<crate::DisplayEvent> {
    let absent =
        |rows: &[ManagedInfo], generation: u64| rows.iter().all(|r| r.generation != generation);
    let gone = before
        .iter()
        .filter(|b| absent(after, b.generation))
        .count() as u32;
    let released = (gone > 0).then_some(crate::DisplayEvent::Released { count: gone });
    let created = after
        .iter()
        .filter(|a| absent(before, a.generation))
        .map(|a| crate::DisplayEvent::Created {
            backend: a.backend.to_string(),
            width: a.mode.0,
            height: a.mode.1,
            refresh_hz: a.mode.2,
        });
    released.into_iter().chain(created).collect()
}

/// Device-level watchdog pinger, running while any slot lives (any IOCTL
/// bumps the watchdog, so one thread serves N monitors). Same stop+join
/// shape as the exclusive-topology re-assert watchdog.
struct Pinger {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Pinger {
    /// Signal, wake and join. The pinger parks between pings, so the join returns within a
    /// scheduler slice, not after the rest of a watchdog/3 interval; the exclusive watch's
    /// join is bounded by its 250 ms slices. Neither blocks on the turn, so a transition
    /// stopping them cannot deadlock.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.thread().unpark();
        let _ = self.thread.join();
    }
}

/// Control-device cache. A gone-class IOCTL retires the handle; the next
/// [`VirtualDisplayManager::ensure_device`] reopens and re-handshakes.
///
/// Every consumer holds its own `Arc` clone across IOCTLs. Retire drops only
/// the manager's reference; the handle closes when the last in-flight user
/// drains. That close is required: an open control handle vetoes the PnP
/// disable/restart wake-from-sleep recovery uses. Do not keep a retired
/// handle open for the process lifetime.
#[derive(Default)]
struct DeviceSlot {
    current: Option<Arc<ControlDevice>>,
    /// A global orphan reap is eligible only on the first unreserved open.
    /// Reopens and seats mode can overlap monitors owned elsewhere.
    opened_once: bool,
}

pub struct VirtualDisplayManager {
    driver: Box<dyn VdisplayDriver>,
    /// The OS display calls of the isolate ladder ([`WinCcd`] outside tests).
    ccd: Box<dyn CcdOps>,
    /// Opened on first acquire; reopened after a gone-classified retire
    /// (see [`DeviceSlot`]). Shared via the `&'static` singleton.
    device: Mutex<DeviceSlot>,
    watchdog_s: AtomicU32,
    /// Handshake protocol version (0 until first open). In-place resize
    /// needs `>= 4`; a v3 driver keeps the advertised fast path plus re-arrival.
    driver_proto: AtomicU32,
    /// Latched after UPDATE_MODES failed to make a new mode settable. The OS
    /// pins a monitor's settable set at arrival, so later attempts only waste
    /// ~1 s before the same re-arrival fallback. One attempt per process.
    update_modes_futile: AtomicBool,
    generation: AtomicU64,
    /// One [`Transition`] at a time, held across its act: the slot map and the CCD topology
    /// change in one order. The exclusive watch only `try_lock`s it.
    turn: Mutex<()>,
    /// The committed slot map and group. Locked only to check them out and back in.
    state: Mutex<MgrInner>,
    /// The last committed slot map. A leaf lock: what `/display/state`, admission and the
    /// connector choice read, so none of them waits on a transition.
    view: Mutex<View>,
    /// Serializes IDD-push setup (preempt + create) manager-wide: ADD/REMOVE
    /// stay exclusive. 400 ms async-departure settle and the IddCx slot-budget
    /// wedge both want zero concurrent ADD/REMOVE. Held across pipeline build.
    setup_lock: Mutex<()>,
    /// Per-slot IDD-push stop flags. A new connection signals only the session
    /// holding that identity's slot. A different identity is admission, never
    /// a preempt. Entries persist (bounded at 16); signalling a dead flag is
    /// harmless.
    idd_session_stops: Mutex<std::collections::HashMap<u32, Arc<AtomicBool>>>,
    pinger: Mutex<Option<Pinger>>,
    /// Exclusive-topology re-assert watchdog. A verified isolate is not
    /// durable (see [`Self::ensure_exclusive_watch`]).
    exclusive_watch: Mutex<Option<Pinger>>,
    // Per-client monitor ids live on `super::identity::global()`. CREATE
    // resolves via `identity::resolve_slot` so the EDID serial + ConnectorIndex
    // stay stable across reconnects and Windows reapplies saved DPI scaling.
}

static VDM: OnceLock<VirtualDisplayManager> = OnceLock::new();

/// Bumped on every exclusive-topology eviction the re-assert watchdog does.
/// An eviction is a real topology change: the OS recreates the IDD swap-chain
/// while the session's capture ring still waits on the old attachment, so
/// frames stop. Stream loops sample this and rebuild capture in place.
static TOPOLOGY_REASSERT_GEN: AtomicU64 = AtomicU64::new(0);

pub fn topology_reassert_gen() -> u64 {
    TOPOLOGY_REASSERT_GEN.load(Ordering::Relaxed)
}

/// Initialise the process-wide manager with `driver`. Idempotent: first
/// backend wins; a later call ignores its driver.
pub(crate) fn init(driver: Box<dyn VdisplayDriver>) -> &'static VirtualDisplayManager {
    VDM.get_or_init(|| VirtualDisplayManager::new(driver, Box::new(WinCcd)))
}

/// Process-wide manager. Panics if reached before a backend called [`init`].
pub fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

/// Whether this host has a pf-vdisplay driver that can run the hardware-cursor channel. Every
/// driver a v7 host accepts can, so this is the handshake itself: it opens the control device
/// once if nothing else has, so Welcome never guesses. `false` when the driver is missing or
/// speaks another protocol.
///
/// The first session's Welcome runs before `vdisplay::open` constructs the
/// backend, so this must not assume an initialised manager. `init` is
/// idempotent and constructing the driver facade is free.
/// The protocol the driver answered at the last handshake, for the status surface; `None`
/// before any session opened the control device. No probe of its own: a v7 driver under a v8
/// host should show here, not only in the session that fails.
pub fn driver_protocol() -> Option<u32> {
    let m = init(Box::new(crate::driver::PfVdisplayDriver));
    let v = m.driver_proto.load(Ordering::Relaxed);
    (v != 0).then_some(v)
}

pub fn hw_cursor_capable() -> bool {
    let m = init(Box::new(crate::driver::PfVdisplayDriver));
    if m.driver_proto.load(Ordering::Relaxed) != 0 {
        return true;
    }
    let _ = m.ensure_device();
    m.driver_proto.load(Ordering::Relaxed) != 0
}

/// Live control device for IDD-push sealed-channel delivery. The caller
/// (and every closure it builds) holds the `Arc` for as long as it may
/// issue IOCTLs; the handle closes when the last holder drains. `None`
/// before the first backend open.
pub fn control_device_handle() -> Option<Arc<ControlDevice>> {
    VDM.get().and_then(VirtualDisplayManager::device_handle)
}

/// Retire the cached control handle from outside the manager, for a caller
/// that knows the device died. Takes the `device` mutex: must not run from
/// inside it (not from `VdisplayDriver::open`, which `ensure_device` holds).
/// No-op before any backend opened the device.
pub(crate) fn invalidate_cached_device(why: &str) {
    if let Some(m) = VDM.get() {
        m.invalidate_device(&anyhow::anyhow!("{why}"));
    }
}

/// Best-effort "is this WUDFHost pid still alive?" for the JOIN path.
/// `OpenProcess` failing or the process signaled ⇒ dead. Pid reuse could
/// alias a fresh process as alive; the joining session then retries into
/// its rebuild budget — acceptable for a sub-second window.
fn wudf_alive(pid: u32) -> bool {
    if pid == 0 {
        return true; // pre-v2 driver reports no pid — never preempt on the probe's account
    }
    // SAFETY: plain FFI probe; the opened handle (checked) is closed exactly once below, and the
    // 0 ms wait only reads its signaled state.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let alive = WaitForSingleObject(h, 0) != WAIT_OBJECT_0;
        let _ = CloseHandle(h);
        alive
    }
}

/// True when an IOCTL failure means the control device itself is gone.
/// The cached handle can only keep failing and must be retired.
/// 0x80070490 (ERROR_NOT_FOUND, ADD slot-exhaustion) is not here — it has
/// its own reap-and-retry and the device is alive when it fires.
fn is_device_gone(e: &anyhow::Error) -> bool {
    let Some(w) = e.downcast_ref::<windows::core::Error>() else {
        return false;
    };
    // Win32 codes as HRESULTs: FILE_NOT_FOUND(2), INVALID_HANDLE(6), BAD_COMMAND(22),
    // GEN_FAILURE(31), DEV_NOT_EXIST(55), OPERATION_ABORTED(995), DEVICE_NOT_CONNECTED(1167 =
    // 0x48F — one below the 0x490 wedge), DEVICE_REMOVED(1617).
    const GONE: [i32; 8] = [
        0x8007_0002u32 as i32,
        0x8007_0006u32 as i32,
        0x8007_0016u32 as i32,
        0x8007_001Fu32 as i32,
        0x8007_0037u32 as i32,
        0x8007_03E3u32 as i32,
        0x8007_048Fu32 as i32,
        0x8007_0651u32 as i32,
    ];
    GONE.contains(&w.code().0)
}

/// A seat ADD that ends in 0x80070490 usually means this handle's adapter left with the RDP
/// connection. The reopen reaches the session's current adapter; exhausted slots fail it again.
fn is_stale_seat_add(e: &anyhow::Error) -> bool {
    in_seat_session()
        && e.downcast_ref::<windows::core::Error>()
            .is_some_and(|w| w.code().0 == 0x8007_0490u32 as i32)
}

impl VirtualDisplayManager {
    fn new(driver: Box<dyn VdisplayDriver>, ccd: Box<dyn CcdOps>) -> VirtualDisplayManager {
        VirtualDisplayManager {
            driver,
            ccd,
            device: Mutex::new(DeviceSlot::default()),
            watchdog_s: AtomicU32::new(3),
            driver_proto: AtomicU32::new(0),
            update_modes_futile: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            turn: Mutex::new(()),
            state: Mutex::new(MgrInner::default()),
            view: Mutex::new(View::default()),
            setup_lock: Mutex::new(()),
            idd_session_stops: Mutex::new(std::collections::HashMap::new()),
            pinger: Mutex::new(None),
            exclusive_watch: Mutex::new(None),
        }
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
    }

    /// Begins a slot or group transition once the one before it committed; see [`Transition`].
    fn transition(&self) -> Transition<'_> {
        self.check_out(self.turn.lock().unwrap())
    }

    /// [`Self::transition`], or `None` while another one runs.
    fn try_transition(&self) -> Option<Transition<'_>> {
        Some(self.check_out(self.turn.try_lock().ok()?))
    }

    fn check_out<'m>(&'m self, turn: MutexGuard<'m, ()>) -> Transition<'m> {
        let inner = std::mem::take(&mut *self.state.lock().unwrap());
        Transition {
            mgr: self,
            inner,
            _turn: turn,
        }
    }

    /// Opens and caches the control device, clearing orphans on the first open
    /// only. #624 scopes `CLEAR_ALL` to the calling process's own monitors, so
    /// a seat cannot reap a sibling's and this needs no reservation gate. The
    /// returned `Arc` keeps each IOCTL's handle alive across concurrent
    /// retirement.
    fn ensure_device(&self) -> Result<Arc<ControlDevice>> {
        let mut slot = self.device.lock().unwrap();
        if let Some(d) = &slot.current {
            return Ok(d.clone());
        }
        let reap = !slot.opened_once;
        claim_instance()?;
        // `open` is safe: it discharges FFI inside its body. The `device`
        // mutex serializes racing opens — serialization, not soundness, so
        // this is not `unsafe`.
        let (handle, watchdog_s, driver_proto) = self.driver.open(reap)?;
        slot.opened_once = true;
        self.watchdog_s.store(watchdog_s, Ordering::Relaxed);
        self.driver_proto.store(driver_proto, Ordering::Relaxed);
        let dev = Arc::new(handle);
        slot.current = Some(dev.clone());
        if !reap {
            tracing::info!("virtual-display control device reopened (retired handle replaced)");
        }
        Ok(dev)
    }

    /// Live control device for the pinger/linger threads. `None` before the
    /// first open, or between a retire and the next reopen.
    fn device_handle(&self) -> Option<Arc<ControlDevice>> {
        self.device.lock().unwrap().current.clone()
    }

    /// Drop the manager's reference after a gone-classified IOCTL. The handle
    /// closes once the last in-flight user drains (see [`DeviceSlot`]). Next
    /// [`ensure_device`](Self::ensure_device) reopens and re-handshakes.
    fn invalidate_device(&self, why: &anyhow::Error) {
        let mut slot = self.device.lock().unwrap();
        if slot.current.take().is_some() {
            tracing::warn!(
                "virtual-display control device retired — closes when its last user drains, \
                 reopening on next use (cause: {why:#})"
            );
        }
    }

    /// Open and initialise the backend (validates the driver is present).
    pub(crate) fn open_backend(&self) -> Result<()> {
        // The `device` mutex serialises the open itself. No transition runs here: one would
        // stall every slot change behind the PowerShell the open may spawn.
        self.ensure_device().map(|_| ())
    }

    /// Acquires the process-resolved connector slot, then joins or creates its monitor by
    /// climbing the isolate ladder ([`Ladder`]) in one transition. A seat override is resolved
    /// before any driver access. The returned [`MonitorLease`] releases only its matching
    /// generation. A device that dies under the ADD or whose seat adapter left is retired once
    /// that transition commits, then reopened outside any and climbed once more.
    pub(crate) fn acquire(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
        client_hdr: Option<pf_frame::HdrMeta>,
        hw_cursor: bool,
        quit: Option<Arc<AtomicBool>>,
    ) -> Result<VirtualOutput> {
        warn_outside_console_session();
        self.ensure_linger_timer();
        let slot =
            resolve_slot_id(client_fp, (mode.width, mode.height)).map_err(anyhow::Error::new)?;
        let ask = Ask {
            slot,
            mode,
            client_fp,
            client_hdr,
            hw_cursor,
            quit,
        };
        let mut reopened = false;
        loop {
            // Before the transition: the open can spawn PowerShell for an adapter reload, and
            // every release would wait on the turn meanwhile.
            let dev = self.ensure_device()?;
            if reopened {
                tracing::info!(
                    "virtual-display control device reopened — retrying the monitor create"
                );
            }
            let climbed = self.climb(&dev, &mut self.transition(), &ask, reopened);
            match climbed {
                Climb::Done(out) => return out,
                Climb::Retire(e) => {
                    self.invalidate_device(&e);
                    reopened = true;
                }
            }
        }
    }

    /// Drops `slot`'s monitor if its adapter left this seat session, and says why the control
    /// device goes with it. A reattach replaces the adapter under a new LUID and WUDFHost, while
    /// this process's handle keeps the old WUDFHost running, so only the session's paths show it.
    fn forget_left_adapter(&self, t: &mut MgrInner, slot: u32) -> Option<anyhow::Error> {
        let key = t.slots.get(&slot)?.mon.ccd_key();
        if !in_seat_session() || !pf_win_display::win_display::adapter_left_session(key) {
            return None;
        }
        let Slot { mon, .. } = t.slots.remove(&slot)?;
        tracing::warn!(slot, old_target = %key, "seat adapter was replaced — recreating the monitor");
        self.teardown_removed(None, t, vec![mon]);
        Some(anyhow::anyhow!("seat adapter of {key} left the session"))
    }

    /// One climb of the ladder for `ask` on the checked-out map: each [`Next`] is one act, and
    /// what it saw picks the next. A slot on a departed seat adapter retires the device first.
    fn climb(
        &'static self,
        dev: &ControlDevice,
        t: &mut MgrInner,
        ask: &Ask,
        reopened: bool,
    ) -> Climb {
        if let Some(why) = self.forget_left_adapter(t, ask.slot) {
            return Climb::Retire(why);
        }
        let max = crate::policy::prefs().get().effective().max_displays;
        let live = t.slots.len() as u32;
        let (mut ladder, mut next) = Ladder::start(found(t, ask), live, max, reopened);
        let mut r = Rungs::default();
        loop {
            let seen = match next {
                Next::Preempt => self.preempt(dev, t, ask.slot),
                Next::ResizeInPlace => self.resize_slot(dev, t, ask),
                Next::Remove => self.remove_for_rearrival(dev, t, ask, &mut r),
                Next::Add { rollback } => self.add(dev, t, ask, &mut r, rollback),
                Next::ResolveTarget => self.resolve(t, ask.slot, &mut r),
                Next::Isolate => self.isolate(t, &r),
                Next::WaitSettle(ceiling) => r.settle(ceiling),
                Next::Verify => self.verify(t, &mut r),
                Next::Commit(change) => return Climb::Done(self.commit(t, ask, r, change)),
                Next::Fail(Reason::Budget) => {
                    return Climb::Done(Err(budget_spent(t.slots.len(), max)));
                }
                Next::Fail(Reason::Add) => return Climb::Done(Err(r.error())),
                Next::RetireDevice => return Climb::Retire(r.error()),
            };
            next = ladder.step(seen);
        }
    }

    /// Tears down the slot's kept or dead monitor and waits out its departure: a reused IddCx
    /// swap-chain is dead, and a back-to-back REMOVE→ADD is rejected under reconnect churn.
    /// Generation-stamped leases on the old monitor no-op on release.
    fn preempt(&self, dev: &ControlDevice, t: &mut MgrInner, slot: u32) -> Seen {
        let Some(Slot { mon, life, .. }) = t.slots.remove(&slot) else {
            return Seen::Preempted;
        };
        let old_key = mon.ccd_key();
        let kept = !matches!(life, lifecycle::State::Active { .. });
        if kept {
            tracing::info!(
                slot,
                old_target = %old_key,
                "IDD-push reconnect — preempting the kept (lingering/pinned) monitor, recreating a fresh one"
            );
        } else {
            tracing::warn!(
                slot,
                old_target = %old_key,
                wudf_pid = mon.wudf_pid,
                "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
            );
        }
        self.teardown_removed(Some(dev), t, vec![mon]);
        // Verified-state wait; the ceiling is the old fixed 400 ms settle (latency plan P0.3).
        if !wait_target_departed(old_key, Duration::from_millis(400)) && kept {
            tracing::debug!(
                old_target = %old_key,
                "preempted monitor still in the active CCD set after the departure ceiling"
            );
        }
        Seen::Preempted
    }

    /// The live monitor resized in place ([`Self::resize_in_place`]); `false` re-arrives it.
    fn resize_slot(&self, dev: &ControlDevice, t: &mut MgrInner, ask: &Ask) -> Seen {
        let Some(Slot { mon, .. }) = t.slots.get_mut(&ask.slot) else {
            unreachable!("a resize means a live slot");
        };
        match self.resize_in_place(dev, mon, ask.mode) {
            Ok(()) => Seen::Resized(true),
            Err(e) => {
                // First-seen size: the OS pins settable modes at arrival, and the re-arrival
                // teaches it. Info, not warn.
                tracing::info!(
                    slot = ask.slot,
                    reason = %format!("{e:#}"),
                    "in-place resize not possible — monitor re-arrival"
                );
                Seen::Resized(false)
            }
        }
    }

    /// A re-arrival's bare REMOVE (no topology restore, the pinger stays up) and its departure.
    /// The REMOVE frees the preferred id for the ADD and is best-effort: the driver reaps a
    /// stale same-id monitor anyway. The old slot leaves the map with its holds until commit.
    fn remove_for_rearrival(
        &self,
        dev: &ControlDevice,
        t: &mut MgrInner,
        ask: &Ask,
        r: &mut Rungs,
    ) -> Seen {
        let Some(old) = t.slots.remove(&ask.slot) else {
            unreachable!("a resize means a live slot");
        };
        let (was, mode) = (old.mon.mode, ask.mode);
        tracing::info!(
            slot = ask.slot,
            old = format!("{}x{}@{}", was.width, was.height, was.refresh_hz),
            new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
            old_target = old.mon.target_id,
            "virtual-display: re-arriving monitor for a mid-stream resize (exact mode)"
        );
        if let Err(e) = self.driver.remove_monitor(dev, &old.mon.key) {
            tracing::warn!(
                old_target = old.mon.target_id,
                "re-arrival REMOVE failed (continuing to ADD): {e:#}"
            );
        }
        // Ceiling 400 ms; the driver's ghost-reap ADD retry is the backstop if CCD reports
        // departed early.
        let depart_start = Instant::now();
        let departed = wait_target_departed(old.mon.ccd_key(), Duration::from_millis(400));
        tracing::info!(
            depart_ms = depart_start.elapsed().as_millis() as u64,
            verified = departed,
            "re-arrival: old monitor departure settle"
        );
        r.render_pin = resolve_render_pin();
        r.old = Some(old);
        Seen::Removed
    }

    /// One ADD rung: a fresh monitor, or a re-arrival's at the asked or the old mode. The
    /// first monitor starts the pinger: the driver tears every display down unless pinged
    /// inside its watchdog window.
    fn add(
        &'static self,
        dev: &ControlDevice,
        t: &mut MgrInner,
        ask: &Ask,
        r: &mut Rungs,
        rollback: bool,
    ) -> Seen {
        let added = match &r.old {
            None => self.add_fresh(dev, t, ask, r),
            Some(old) => self.add_again(dev, ask, &old.mon, r.render_pin, rollback),
        };
        match added {
            Ok(mon) => {
                self.ensure_pinger();
                r.mon = Some(mon);
                Seen::Added(Ok(()))
            }
            Err(e) => {
                let class = if is_device_gone(&e) || is_stale_seat_add(&e) {
                    AddError::DeviceGone
                } else {
                    AddError::Refused
                };
                r.failed(ask.slot, e);
                Seen::Added(Err(class))
            }
        }
    }

    /// A fresh monitor on the slot. The first member decides the group's topology and reads
    /// the standby baseline before anything moves; a dark console display is woken; the exact
    /// connector is ADDed, and a driver's fallback connector is removed and refused, since it
    /// could land in another process's reserved range.
    fn add_fresh(
        &'static self,
        dev: &ControlDevice,
        t: &mut MgrInner,
        ask: &Ask,
        r: &mut Rungs,
    ) -> Result<Monitor> {
        r.first = t.slots.is_empty();
        if r.first {
            t.group.topology = topology_action(ask.client_fp);
        }
        wake_console_display()?;
        let slot_plan = process_slot_plan().map_err(anyhow::Error::new)?;
        let render_pin = resolve_render_pin();
        // Gate on an opened device: the version is 0 until the handshake ran, and the capture
        // layer must not create a section nobody will publish into.
        let hw_cursor = ask.hw_cursor && self.driver_proto.load(Ordering::Relaxed) != 0;
        if r.first && crate::policy::prefs().standby_sink_neutralise() {
            r.baseline = standby_baseline();
        }
        // The slot id doubles as the driver-preferred monitor id (EDID serial / ConnectorIndex),
        // so Windows reapplies saved DPI on reconnect; `0` (anonymous) auto-allocates.
        let added = self.driver.add_monitor(
            dev,
            ask.mode,
            render_pin,
            ask.slot,
            ask.client_hdr,
            hw_cursor,
        )?;
        self.refuse_cross_slot(dev, slot_plan, &added)?;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(Monitor {
            client_fp: ask.client_fp,
            ..Monitor::arrived(&added, ask.mode, render_pin, hw_cursor, generation)
        })
    }

    /// A re-arrival's ADD on the same slot, at the asked mode or, as the rollback, at the old
    /// monitor's requested one: those differ when the refresh was clamped, and re-ADDing the
    /// clamp would make the next acquire at the negotiated rate look like another resize. The
    /// new monitor keeps `generation` (lease continuity) and the layout `position`.
    fn add_again(
        &self,
        dev: &ControlDevice,
        ask: &Ask,
        old: &Monitor,
        render_pin: Option<LUID>,
        rollback: bool,
    ) -> Result<Monitor> {
        let mode = if rollback {
            old.requested_mode
        } else {
            ask.mode
        };
        let added = self.driver.add_monitor(
            dev,
            mode,
            render_pin,
            ask.slot,
            ask.client_hdr,
            old.hw_cursor,
        )?;
        Ok(Monitor {
            position: old.position,
            client_fp: old.client_fp,
            beside: old.beside,
            ..Monitor::arrived(&added, mode, render_pin, old.hw_cursor, old.generation)
        })
    }

    /// The activation ladder for the new monitor ([`Self::resolve_target_gdi`]); a joined
    /// slot that never resolved runs it again ([`Self::resolve_joined`]).
    fn resolve(&self, t: &mut MgrInner, slot: u32, r: &mut Rungs) -> Seen {
        let rearrival = r.old.is_some();
        let Some(mon) = r.mon.as_mut() else {
            return self.resolve_joined(t, slot);
        };
        let key = mon.ccd_key();
        mon.gdi_name = self.resolve_target_gdi(key);
        match (&mon.gdi_name, rearrival) {
            (Some(n), false) => tracing::info!(
                backend = self.driver.name(),
                target = %key,
                gdi = %n,
                "IDD target activated into a display path"
            ),
            (Some(n), true) => tracing::info!(
                backend = self.driver.name(),
                "re-arrival target {key} -> {n}"
            ),
            (None, false) => tracing::warn!(
                "virtual-display target {key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
            ),
            (None, true) => tracing::warn!(
                "re-arrival target {key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
            ),
        }
        Seen::Target(mon.gdi_name.is_some())
    }

    /// A same-mode join of a monitor the OS never activated. A plain join hands that dead
    /// target back, so the caller's whole retry budget re-reads one failure; the monitor is
    /// live, so it runs the activation ladder again.
    fn resolve_joined(&self, t: &mut MgrInner, slot: u32) -> Seen {
        let Some(s) = t.slots.get_mut(&slot) else {
            unreachable!("a join means a live slot");
        };
        let key = s.mon.ccd_key();
        if let Some(name) = self.resolve_target_gdi(key) {
            tracing::info!(
                slot,
                target = %key,
                gdi_name = %name,
                "virtual-display target activated on a later acquire"
            );
            s.mon.gdi_name = Some(name);
        }
        Seen::Target(s.mon.gdi_name.is_some())
    }

    /// Writes the mode the ADD advertised, then the group topology: a fresh member's
    /// ([`Self::apply_group_topology_on_add`]), or a re-arrival's re-isolate with the new
    /// target in place of the old ([`Self::reisolate_after_swap`]).
    fn isolate(&'static self, t: &mut MgrInner, r: &Rungs) -> Seen {
        let mon = r.mon.as_ref().expect("the isolate rung follows an ADD");
        let gdi = mon
            .gdi_name
            .as_deref()
            .expect("the isolate rung follows a target");
        let (key, mode) = (mon.ccd_key(), mon.requested_mode);
        // ADD only advertises; force the mode so DXGI captures the requested size. CCD takes it
        // as data; GDI can only pick from an enumeration that has not refreshed this soon
        // after arrival.
        if !pf_win_display::win_display::set_active_mode_ccd(key, mode)
            && !set_active_mode(gdi, mode)
        {
            tracing::warn!(
                target = %key,
                mode = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                "neither CCD nor GDI wrote the mode — the display stays on the one it runs"
            );
        }
        if r.old.is_some() {
            self.reisolate_after_swap(t, key);
        } else {
            self.apply_group_topology_on_add(t, key, gdi, r.first);
        }
        Seen::Isolated
    }

    /// Records the mode the OS committed, not the one asked for ([`committed_mode_or`]); a
    /// first member with a baseline then parks the standby sinks.
    fn verify(&self, t: &mut MgrInner, r: &mut Rungs) -> Seen {
        let mon = r.mon.as_mut().expect("the verify rung follows an ADD");
        let key = mon.ccd_key();
        mon.mode = committed_mode_or(key, mon.requested_mode);
        // No baseline ⇒ no pass: see `standby_baseline`.
        if let (Some(baseline), Some(start)) = (r.baseline.as_deref(), r.settle_start)
            && crate::policy::prefs().standby_sink_neutralise()
        {
            neutralise_standby_sinks(&*self.ccd, t, key, baseline, start);
        }
        Seen::Verified
    }

    /// Stores the slot change, then pays what it owes the group ([`SlotChange::owes`]).
    fn commit(
        &'static self,
        t: &mut MgrInner,
        ask: &Ask,
        mut r: Rungs,
        change: SlotChange,
    ) -> Result<VirtualOutput> {
        let out = self.store(t, ask, &mut r, change);
        let owes = change.owes();
        if owes.regroup {
            self.regroup(t);
        }
        if owes.layout {
            self.apply_group_layout(t);
        }
        out
    }

    /// The slot change itself. A join or a resize adds a hold and keeps the generation, so
    /// the old lease stays valid; a re-arrival keeps the old holds and generation, so both
    /// leases match on release.
    fn store(
        &'static self,
        t: &mut MgrInner,
        ask: &Ask,
        r: &mut Rungs,
        change: SlotChange,
    ) -> Result<VirtualOutput> {
        let slot = ask.slot;
        let quit = ask.quit.clone();
        match change {
            SlotChange::Create => {
                let mon = r.mon.take().expect("a create commits its ADD");
                let out = self.output_for(slot, &mon, quit, DisplayOwnership::Owned);
                let mut life = lifecycle::State::default();
                life.acquire(); // Create: Idle → Active { refs: 1 }
                let joined = 0;
                t.slots.insert(slot, Slot { mon, life, joined });
                Ok(out)
            }
            SlotChange::Join | SlotChange::Resized => {
                let Some(Slot { mon, life, .. }) = t.slots.get_mut(&slot) else {
                    unreachable!("a join means a live slot");
                };
                life.acquire();
                if change == SlotChange::Resized {
                    tracing::info!(
                        slot,
                        refs = life.refs(),
                        backend = self.driver.name(),
                        "virtual monitor resized IN PLACE (identity + swap-chain kept)"
                    );
                } else {
                    tracing::info!(
                        slot,
                        refs = life.refs(),
                        backend = self.driver.name(),
                        "virtual monitor reused (concurrent session)"
                    );
                    warn_if_pick_moved(mon);
                }
                Ok(self.output_for(slot, mon, quit, DisplayOwnership::Owned))
            }
            SlotChange::ReArrived | SlotChange::RolledBack => {
                let mon = r.mon.take().expect("a re-arrival commits its ADD");
                let Slot {
                    mut life, joined, ..
                } = r.old.take().expect("a re-arrival keeps its old slot");
                if change == SlotChange::RolledBack {
                    t.slots.insert(slot, Slot { mon, life, joined });
                    return Err(r.error()).context("mid-stream resize re-arrival");
                }
                life.acquire();
                let out = self.output_for(slot, &mon, quit, DisplayOwnership::Owned);
                tracing::info!(
                    slot,
                    refs = life.refs(),
                    backend = self.driver.name(),
                    "virtual monitor re-arrived for a mid-stream resize"
                );
                t.slots.insert(slot, Slot { mon, life, joined });
                Ok(out)
            }
            SlotChange::Lost => {
                Err(r.error()).context("mid-stream resize re-arrival (slot left empty)")
            }
        }
    }

    /// [`VirtualOutput`] for `mon` in `slot`: preferred mode, capture target,
    /// generation-stamped lease. `quit` is read by the lease `Drop` (see
    /// [`Self::release`]). A session's own monitor is `Owned`: the manager owns its lifecycle
    /// and the registry delegates through `vd.create`. A joiner's is `External`, and its lease
    /// gives back a joined hold.
    fn output_for(
        &'static self,
        slot: u32,
        mon: &Monitor,
        quit: Option<Arc<AtomicBool>>,
        ownership: DisplayOwnership,
    ) -> VirtualOutput {
        VirtualOutput {
            node_id: 0,
            preferred_mode: Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz)),
            win_capture: mon.target(),
            keepalive: Box::new(MonitorLease {
                mgr: self,
                slot,
                generation: mon.generation,
                quit,
                joined: ownership == DisplayOwnership::External,
                slept_at: crate::suspend::slept(),
            }),
            ownership,
        }
    }

    /// Start the device-level watchdog pinger if it is not running. One thread
    /// serves every slot — any IOCTL bumps the watchdog.
    fn ensure_pinger(&'static self) {
        let mut guard = self.pinger.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let interval =
            Duration::from_millis(self.watchdog_s.load(Ordering::Relaxed) as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("vdisplay-pinger".into())
            .spawn(move || {
                let mut warned = false;
                // Gone-class failures in a row: one is a transient the next ping clears, two
                // is a device that is really gone. Retiring on one silences the keepalive,
                // and the driver's silence watchdog then departs live monitors.
                let mut gone_streak = 0u32;
                while !stop_t.load(Ordering::Relaxed) {
                    if let Some(h) = vdm().device_handle() {
                        // The `h` Arc keeps a concurrently retired handle open: at worst the
                        // IOCTL fails (see `DeviceSlot`).
                        match vdm().driver.ping(&h) {
                            Ok(()) => {
                                warned = false;
                                gone_streak = 0;
                                // The driver's lines ride the keepalive: this thread already
                                // holds the only handle open for the whole device lifetime, so
                                // the encoder's diagnostics reach the log even between sessions.
                                vdm().driver.drain_log(&h);
                            }
                            Err(e) if is_device_gone(&e) => {
                                gone_streak += 1;
                                if gone_streak >= 2 {
                                    // Device is gone. Retire so the next session
                                    // reopens; the monitors are already dead
                                    // driver-side.
                                    vdm().invalidate_device(&e);
                                }
                            }
                            Err(e) => {
                                if !warned {
                                    tracing::warn!(
                                        "virtual-display keepalive PING failed (control handle lost?): {e:#}"
                                    );
                                    warned = true;
                                }
                            }
                        }
                    }
                    // Parked, not asleep: `stop` unparks, so the last member's teardown
                    // does not wait out the interval with the turn held. A spurious wake
                    // pings early, which the watchdog does not mind.
                    thread::park_timeout(interval);
                }
            });
        // Not `thread::spawn`: that panics if the OS refuses the thread, and this holds
        // `pinger` inside a transition, so unwinding poisons it and the turn. A missing
        // pinger degrades to the driver watchdog; a poisoned manager does not.
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not spawn the virtual-display keepalive pinger — the driver's host-gone \
                     watchdog will tear this monitor down when it expires"
                );
                return;
            }
        };
        *guard = Some(Pinger { stop, thread });
    }

    /// Start the exclusive-topology re-assert watchdog (idempotent).
    ///
    /// A verified isolate is not durable: until a re-assert stores it, the CCD database
    /// still holds the user's layout for the connected set, and a re-resolution (a display
    /// re-plugging itself) brings that back.
    ///
    /// Each cycle, at most every [`knobs::exclusive_reassert_ms`]: wait, try for a transition,
    /// read the survivors, ask the [`ReassertBreaker`], act with `state` released. A re-assert
    /// evicts via full isolate; the forced re-commit restarts presentation, and the session
    /// heals the swap-chain bounce off [`topology_reassert_gen`]. Teardown stops+joins this
    /// thread inside a transition, so a blocking wait for the turn here would deadlock, and
    /// the wait is sliced so stop+join is bounded by ~250 ms, not a full cycle.
    fn ensure_exclusive_watch(&'static self) {
        let interval = Duration::from_millis(knobs::exclusive_reassert_ms());
        if interval.is_zero() {
            return; // knob 0 = disabled
        }
        let mut guard = self.exclusive_watch.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("vdisplay-exclusive-watch".into())
            .spawn(move || {
                let mut breaker = ReassertBreaker::default();
                while wait_watch_cycle(&stop_t, interval) {
                    let Some(mut inner) = self.try_transition() else {
                        continue;
                    };
                    let Some((keep, survivors)) = inner.exclusive_survivors(&*self.ccd) else {
                        continue;
                    };
                    match breaker.on_cycle(survivors, Instant::now()) {
                        Cycle::Stable { was_fighting: 0 } => {}
                        Cycle::Stable { was_fighting } => {
                            tracing::info!(
                                reasserts = was_fighting,
                                "exclusive topology stable again — no non-managed display active"
                            );
                            // Close the churn window now — descriptor-following
                            // resumes instead of waiting out the hold expiry.
                            topology_churn::release();
                        }
                        Cycle::Conceded => tracing::debug!(
                            survivors,
                            "exclusive topology conceded — a non-managed display is active and \
                             stays so; not re-asserting"
                        ),
                        Cycle::Concede { rounds } => {
                            tracing::error!(
                                survivors,
                                rounds,
                                "exclusive topology keeps being re-activated (4 re-asserts in a \
                                 row or within a minute) — something on this host is fighting \
                                 the isolate; CONCEDING for this session: the display stays \
                                 active, the stream continues on the shared desktop, no further \
                                 re-assert"
                            );
                            topology_churn::release();
                        }
                        Cycle::Reassert { round } => {
                            tracing::warn!(
                                survivors,
                                round,
                                "exclusive topology lost — a non-managed display re-activated \
                                 after the verified isolate (hybrid-GPU driver / display-poller \
                                 software restoring the saved layout?); re-asserting the isolate"
                            );
                            inner.reassert_isolate(
                                &*self.ccd,
                                &keep,
                                interval + Duration::from_secs(3),
                            );
                        }
                    }
                }
            });
        // Not `.expect()`: this holds `exclusive_watch` inside a transition, and a panic
        // poisons it and the turn. A missing watchdog is a degradation; wedging the manager
        // is not.
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not spawn the exclusive re-assert watchdog — the isolate will not be \
                     re-asserted if something re-lights a physical display"
                );
                return;
            }
        };
        *guard = Some(Pinger { stop, thread });
    }

    /// Arrange live slots' desktop origins (`auto-row` default, console `manual`
    /// pins win) and commit them in one CCD apply. No-ops for a single member.
    fn apply_group_layout(&self, inner: &mut MgrInner) {
        use crate::layout::{arrange, Member};
        if inner.slots.len() < 2 {
            return;
        }
        let layout_policy = crate::policy::prefs().get().effective().layout;
        // Members in acquire (generation) order — the auto-row order; identity slot 0 = anonymous (no
        // manual pin can address it, so it always auto-rows). `(slot, generation, key, width)`
        // copied out so the arrangement below can write back through `get_mut`.
        let mut ordered: Vec<(u32, u64, CcdTargetKey, i32)> = inner
            .slots
            .iter()
            .map(|(slot, s)| {
                let m = &s.mon;
                (*slot, m.generation, m.ccd_key(), m.mode.width as i32)
            })
            .collect();
        ordered.sort_by_key(|&(_, generation, _, _)| generation);
        let members: Vec<Member> = ordered
            .iter()
            .map(|&(slot, _, _, width)| Member {
                identity_slot: (slot != 0).then_some(slot),
                width,
            })
            .collect();
        let mut placements = arrange(&members, &layout_policy);
        // A further screen holds its offset from its client's first, wherever that one went.
        for i in 0..ordered.len() {
            let Some(b) = inner.slots.get(&ordered[i].0).and_then(|s| s.mon.beside) else {
                continue;
            };
            let at = ordered.iter().position(|&(slot, ..)| {
                inner
                    .slots
                    .get(&slot)
                    .is_some_and(|s| s.mon.client_fp == b.anchor)
            });
            if let Some(j) = at {
                // Saturating, as `layout` is: the offset is the client's, and a wrapped
                // origin fails the one CCD call for every member (a debug build panics
                // inside the transition).
                placements[i] = crate::layout::Placement {
                    x: placements[j].x.saturating_add(b.offset.0),
                    y: placements[j].y.saturating_add(b.offset.1),
                };
            }
        }
        let positions: Vec<(CcdTargetKey, i32, i32)> = ordered
            .iter()
            .zip(&placements)
            .map(|(&(_, _, target, _), p)| (target, p.x, p.y))
            .collect();
        pf_win_display::win_display::apply_source_positions(&positions);
        for (&(slot, ..), p) in ordered.iter().zip(&placements) {
            if let Some(s) = inner.slots.get_mut(&slot) {
                s.mon.position = (p.x, p.y);
            }
        }
    }

    /// Wait for Windows to auto-activate a freshly-ADDed IDD target into its OWN display path and
    /// return its GDI name — the capture target. The ladder's resolve rung, for the fresh CREATE,
    /// the mid-stream re-arrival and a join that never resolved. `None` on a GPU-less box
    /// (target added but not WDDM-activated); the capture backend re-resolves once a GPU is
    /// present.
    ///
    /// A three-stage ladder, each stage a real failure mode. Plain poll FIRST — never a forced
    /// topology change: bare `SDC_TOPOLOGY_EXTEND` is ACCESS_DENIED from Session 0 on a headless
    /// box and breaks the auto-activate. force-EXTEND is the integrated-screen FALLBACK: a fresh
    /// IDD there is CLONED onto the panel (shared source, no committed path of its own), and the
    /// EXTEND de-clones it. LAST RESORT is explicit path activation: a closed lid defeats both
    /// (the clamshell policy suppresses auto-activation and the EXTEND preset "succeeds"
    /// committing nothing), and `activate_target_path` commits the path directly, which ignores
    /// the lid policy. A seat session with no paths left is rebuilt for the caller's next attempt
    /// ([`rebuild_empty_seat_session`]).
    ///
    /// CAVEAT (unobserved): textbook CCD also allows a clone with a *shared-source ACTIVE* path
    /// (resolve → `Some`), which the `is_none()` gate would miss; widening needs a
    /// `target_is_cloned` helper plus on-laptop validation.
    ///
    /// Call inside a transition: this mutates the live CCD topology, and one transition at a
    /// time keeps two path commits from interleaving.
    fn resolve_target_gdi(&self, key: CcdTargetKey) -> Option<String> {
        // 50 ms sampling under each stage's 3 s ceiling: a typical activation resolves on an early
        // poll.
        if let Some(n) = poll_gdi_name(key) {
            return Some(n);
        }
        force_extend_topology();
        if let Some(n) = poll_gdi_name(key) {
            return Some(n);
        }
        if pf_win_display::win_display::activate_target_path(key) {
            if let Some(n) = poll_gdi_name(key) {
                return Some(n);
            }
        }
        rebuild_empty_seat_session();
        None
    }

    /// A taken `preferred_monitor_id` is not refused by the driver: it falls back to the
    /// lowest free connector across the whole device. Losing DPI persistence that way is
    /// survivable, so an unreserved host takes what it is given. Landing in someone else's
    /// range is not, and nothing else would notice: remove that monitor and fail the ADD.
    fn refuse_cross_slot(
        &self,
        dev: &ControlDevice,
        slot_plan: super::identity::WindowsSlotPlan,
        added: &AddedMonitor,
    ) -> Result<()> {
        let missed_seat_slot = slot_plan
            .seat_slot()
            .is_some_and(|slot| added.resolved_monitor_id != slot);
        let crossed_out_of_range = slot_plan
            .ordinary_max()
            .is_some_and(|max| added.resolved_monitor_id > max);
        if !missed_seat_slot && !crossed_out_of_range {
            return Ok(());
        }
        let resolved_id = added.resolved_monitor_id;
        // `added.key` names the fallback monitor the successful ADD created.
        let cleanup = self.driver.remove_monitor(dev, &added.key);
        let reason = match slot_plan.seat_slot() {
            Some(slot) => format!("this seat's connector {slot}"),
            None => format!(
                "the console connector range 0..={}",
                slot_plan.ordinary_max().expect("console plan has a bound")
            ),
        };
        if let Err(error) = cleanup {
            anyhow::bail!(
                "pf-vdisplay assigned connector slot {resolved_id} outside {reason}; refusing \
                 cross-slot fallback, and cleanup failed: {error:#}"
            );
        }
        anyhow::bail!(
            "pf-vdisplay assigned connector slot {resolved_id} outside {reason}; removed the \
             fallback monitor and refused cross-slot attachment"
        );
    }

    /// Group topology for a member that just activated as `gdi`. The CCD isolate is one
    /// topology for the whole managed GROUP, so the first member's device decides it
    /// (`acquire`) and later members follow. First, outside a live exclusive group, a set that
    /// came up from a dead host's stored isolate lights its panels again. Exclusive: the first
    /// member captures the restore, later ones re-isolate the grown set. Primary/Extend leave
    /// physicals lit.
    fn apply_group_topology_on_add(
        &'static self,
        inner: &mut MgrInner,
        added_key: CcdTargetKey,
        gdi: &str,
        first_member: bool,
    ) {
        use crate::policy::Topology;
        // A live exclusive group's stored isolate is its own; its teardown stores the restore.
        if !inner.group.ccd_exclusive {
            pf_win_display::layout_db::repair();
        }
        match inner.group.topology {
            Topology::Exclusive if first_member => self.isolate_group_first(inner, added_key, gdi),
            Topology::Exclusive => {
                // Re-isolate so the fresh member joins the composited set. Discard the
                // snapshot unless the first member's isolate failed — then adopt this one,
                // or teardown cannot restore the physicals.
                let (snap, _) = isolate_txn(
                    &*self.ccd,
                    "sibling-isolate",
                    &inner.keep_with(added_key),
                    ISOLATE_HOLD,
                    false,
                );
                if inner.group.ccd_saved.is_none() {
                    if let Some(snap) = snap {
                        tracing::warn!(
                            "display isolate (CCD): the first member captured no restore \
                             snapshot (its isolate failed) — adopting this member's, so \
                             teardown can still put the physical displays back"
                        );
                        inner.group.ccd_saved = Some(snap);
                        inner.group.ccd_exclusive = true;
                        self.ensure_exclusive_watch();
                    }
                }
            }
            Topology::Primary if first_member => {
                // force-EXTEND only when the virtual is the SOLE active display (the
                // headless auto-activate): on a lit physical the bare EXTEND preset
                // re-pulls persistence-DB modes and resets a 120 Hz panel to 60; the
                // reposition below re-supplies queried modes verbatim. An UNKNOWN
                // answer (query failed) also skips it — never mutate unverified state.
                let already_extended = match count_other_active(&[added_key]) {
                    Some(n) => n > 0,
                    None => {
                        tracing::warn!(
                            "display topology=primary — CCD query failed; skipping the \
                             force-EXTEND (topology state unknown, mutating nothing extra)"
                        );
                        true
                    }
                };
                if already_extended {
                    tracing::info!(
                        "display topology=primary — a physical display is already active; \
                         skipping force-EXTEND (preserves its refresh) before making the \
                         virtual primary"
                    );
                } else {
                    force_extend_topology();
                    thread::sleep(Duration::from_millis(300));
                }
                inner.group.ccd_saved = set_virtual_primary_ccd(added_key);
            }
            Topology::Primary => {
                // A sibling already holds primary; the new member
                // just extends. Group layout arranges it.
                tracing::info!(
                    "display topology=primary — sibling slot holds primary; new member extends"
                );
            }
            Topology::Extend | Topology::Auto => {
                tracing::info!(
                    "display topology=extend — IDD stays extended (no isolate / no primary)"
                );
            }
        }
    }

    /// Exclusive, first member: darken and pin what the isolate is about to deactivate, isolate
    /// to the keep-set as one topology transaction, then PnP-disable the monitors it
    /// deactivated and start the watchdog that keeps the isolate in place.
    fn isolate_group_first(
        &'static self,
        inner: &mut MgrInner,
        added_key: CcdTargetKey,
        gdi: &str,
    ) {
        // DDC off BEFORE isolate: an HMONITOR (and the DDC channel) exists only while the
        // display is still active. Evidence: `windows/ddc.rs`.
        if crate::policy::prefs().ddc_power_off() {
            inner.group.ddc_panels_off = crate::ddc::panel_off_except(gdi);
        }
        // Pin AMD connector EDID before isolate so an awake sink still answers the live-EDID
        // read. Emulation outlives the process. Evidence: `pf_win_display::adl_emul`.
        if crate::policy::prefs().edid_lock() {
            inner.group.edid_locked = pf_win_display::adl_emul::lock_for_stream();
        }
        // The PnP leases below are stamped with this transaction's generation.
        let (saved, finished) = isolate_txn(
            &*self.ccd,
            "acquire-isolate",
            &inner.keep_with(added_key),
            ISOLATE_HOLD,
            false,
        );
        inner.group.ccd_saved = saved;
        // After isolate, disable deactivated monitor PnP devnodes so standby wake events do
        // not cascade. Evidence: `windows/monitor_devnode.rs`.
        if crate::policy::prefs().pnp_disable_monitors() {
            if let Some(saved) = &inner.group.ccd_saved {
                inner.group.pnp_disabled = pf_win_display::monitor_devnode::disable_for_deactivated(
                    saved,
                    added_key,
                    finished.generation,
                );
            }
        }
        // Verified isolate is not durable — see `ensure_exclusive_watch`.
        inner.group.ccd_exclusive = inner.group.ccd_saved.is_some();
        if inner.group.ccd_exclusive {
            self.ensure_exclusive_watch();
        }
    }

    /// Mid-stream resize on the same monitor: refresh the advertised mode list
    /// (`IOCTL_UPDATE_MODES`, protocol v4), re-enumerate, CCD/GDI force-set.
    /// Target id, GDI name, DPI, swap-chain worker and frame stash survive.
    /// On failure `mon` is untouched and the ladder re-arrives it.
    /// Call inside a transition: CCD/GDI runs inside.
    fn resize_in_place(&self, dev: &ControlDevice, mon: &mut Monitor, mode: Mode) -> Result<()> {
        let gdi = mon
            .gdi_name
            .clone()
            .context("in-place resize needs a resolved GDI name")?;
        let t0 = Instant::now();
        // FAST PATH (driver-independent): the OS already offers this resolution — the monitor's
        // arrival list, which since the driver's mode-history union contains every size this
        // identity ever served — so a plain CCD mode set reaches it with no driver round-trip.
        let mon_key = mon.ccd_key();
        let already = pf_win_display::win_display::wait_mode_advertised(&gdi, mode, Duration::ZERO);
        if !already {
            // Out-of-arrival-list. The OS pins the settable set at arrival,
            // so one bounded UPDATE_MODES attempt per process, then latch
            // futile and fail fast to re-arrival (same-id history then makes
            // this size settable in place).
            if self.update_modes_futile.load(Ordering::Relaxed) {
                anyhow::bail!(
                    "{}x{} is not in the advertised mode set (UPDATE_MODES latched futile — the \
                     OS pins settable modes at monitor arrival; the re-arrival teaches this size \
                     to the identity's history)",
                    mode.width,
                    mode.height
                );
            }
            tracing::info!(
                old = format!(
                    "{}x{}@{}",
                    mon.mode.width, mon.mode.height, mon.mode.refresh_hz
                ),
                new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                target = mon.target_id,
                "virtual-display: updating the live monitor's modes for an in-place resize"
            );
            self.driver.update_modes(dev, &mon.key, mode)?;
            pf_win_display::win_display::force_mode_reenumeration();
            if !pf_win_display::win_display::wait_mode_advertised(
                &gdi,
                mode,
                Duration::from_millis(800),
            ) {
                self.update_modes_futile.store(true, Ordering::Relaxed);
                anyhow::bail!(
                    "OS did not advertise {}x{} within {}ms of the driver mode-list update \
                     (offers: {:?}) — latching UPDATE_MODES off for this process",
                    mode.width,
                    mode.height,
                    t0.elapsed().as_millis(),
                    pf_win_display::win_display::advertised_resolutions(&gdi)
                );
            }
        }
        let advertised_ms = t0.elapsed().as_millis() as u64;
        if !pf_win_display::win_display::set_active_mode_ccd(mon_key, mode)
            && !set_active_mode(&gdi, mode)
        {
            tracing::warn!(
                target = %mon_key,
                mode = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                "neither CCD nor GDI wrote the mode — the display stays on the one it runs"
            );
        }
        // Same committed-state predicate as create. Uncommitted within the
        // ceiling routes to the re-arrival fallback.
        let settle_start = Instant::now();
        let settled = wait_mode_settled(mon_key, mode, ladder::SETTLE);
        if !settled {
            anyhow::bail!(
                "in-place mode set did not commit within 1.5s (advertised after {advertised_ms} ms)"
            );
        }
        // Record what actually COMMITTED, not what was asked for — see [`committed_mode_or`], which
        // the fresh-create and re-arrival paths share with this one so all three store the same
        // truth: `mon.mode` is what `/display/state` reports and what the capturer paces to.
        let landed = committed_mode_or(mon_key, mode);
        tracing::info!(
            advertised_ms,
            settle_ms = settle_start.elapsed().as_millis() as u64,
            mode = format!("{}x{}@{}", landed.width, landed.height, landed.refresh_hz),
            "in-place resize committed (verified-state wait)"
        );
        mon.mode = landed;
        // Also store what was asked: the session re-requests this on the next
        // acquire. Dropping it would make every rebuild re-enter this path.
        mon.requested_mode = mode;
        Ok(())
    }

    /// Re-isolate after a re-arrival put a new target in place of the old one,
    /// without recapturing the group restore snapshot. The old slot is already
    /// gone from the map, so `inner.target_keys()` is surviving siblings.
    ///
    /// Call inside a transition: it commits a new CCD topology, so it must not interleave with
    /// another slot transition's commit. `&mut MgrInner` is the checked-out map, so holding one
    /// proves the turn is held.
    fn reisolate_after_swap(&self, inner: &mut MgrInner, new_target: CcdTargetKey) {
        use crate::policy::Topology;
        // As above: one isolate for the group, so the topology it opened with.
        match inner.group.topology {
            Topology::Exclusive => {
                // Grown-set semantics: isolate to the surviving siblings + the new target. The returned
                // snapshot is DISCARDED — the group keeps the first member's (design §6.1).
                let _ = isolate_txn(
                    &*self.ccd,
                    "swap-isolate",
                    &inner.keep_with(new_target),
                    ISOLATE_HOLD,
                    false,
                );
            }
            Topology::Primary => {
                // Predecessor held primary. The call recaptures a snapshot, so
                // save/restore the group's around it.
                let keep_saved = inner.group.ccd_saved.take();
                let _ = set_virtual_primary_ccd(new_target);
                inner.group.ccd_saved = keep_saved;
            }
            Topology::Extend | Topology::Auto => {}
        }
    }

    /// Put the desktop back the way the group found it: stop the pinger and the exclusivity
    /// watchdog, re-enable the monitors we PnP-disabled, restore the saved topology, clear the
    /// crash journal, wake the panels, release the EDID lock.
    ///
    /// Called for the last slot's teardown AND by a re-arrival that lost its monitor. That path
    /// empties the slot and returns, and without this the desktop stays isolated with the pinger
    /// running, the monitors disabled and the EDID pinned, and nothing left to undo it.
    fn restore_group(&self, inner: &mut MgrInner) {
        // Last slot: stop the pinger first, then restore first-in/last-out.
        if let Some(p) = self.pinger.lock().unwrap().take() {
            p.stop();
        }
        // Watchdog must be gone before restore — it would read the restored
        // topology as "lost exclusivity" and re-fight it.
        if let Some(w) = self.exclusive_watch.lock().unwrap().take() {
            w.stop();
        }
        // Re-enable PnP first and let them re-arrive, so CCD restore
        // finds monitors that exist. Outside the ccd_saved gate: the
        // connected-inactive sweep also runs in Extend/Primary.
        let pnp_disabled = std::mem::take(&mut inner.group.pnp_disabled);
        if !pnp_disabled.is_empty() {
            pf_win_display::monitor_devnode::enable_instances(&pnp_disabled);
            thread::sleep(Duration::from_millis(300));
        }
        // Re-attach detached displays BEFORE REMOVE so the box is never
        // left with zero displays.
        inner.group.ccd_exclusive = false;
        if let Some(saved) = inner.group.ccd_saved.take() {
            // Clears the isolate crash journal, or keeps it when the desk stayed dark.
            restore_displays_ccd(&saved);
        } else {
            // A failed isolate leaves no snapshot. The group is gone, so its journal goes too.
            pf_win_display::win_display::isolate_journal::clear();
        }
        // DDC wake outside the `ccd_saved` gate: panels were commanded
        // dark before isolate, which can return None. Nested in that arm
        // the wake never ran and the live link never DPMS-wakes them.
        // 300 ms lets re-activated paths show up in EnumDisplayMonitors.
        if inner.group.ddc_panels_off > 0 {
            thread::sleep(Duration::from_millis(300));
            let woken = crate::ddc::panel_on_all();
            tracing::info!(
                commanded_off = inner.group.ddc_panels_off,
                woken,
                "DDC/CI: panel wake commands sent after topology restore"
            );
            inner.group.ddc_panels_off = 0;
        }
        // Unlock after CCD restore + DDC wake so the driver does not
        // re-probe sinks mid-restore. Outside `ccd_saved` for the same
        // reason as the DDC wake — lock ran before isolate.
        if inner.group.edid_locked {
            pf_win_display::adl_emul::unlock_after_stream();
            inner.group.edid_locked = false;
        }
    }

    /// Tear down `mons`, already out of `inner.slots`: the group regroups once on what
    /// survives them all ([`Self::regroup`]), then one REMOVE each, then the survivors'
    /// layout. An empty batch is a no-op.
    fn teardown_removed(
        &self,
        dev: Option<&ControlDevice>,
        inner: &mut MgrInner,
        mons: Vec<Monitor>,
    ) {
        if mons.is_empty() {
            return;
        }
        // Runs inside a transition, so a REMOVE/CCD restore that never returns blocks every
        // later one with nothing in the log. One ERROR after 10 s turns that silent wedge
        // into a diagnosis.
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            let targets: Vec<u32> = mons.iter().map(|m| m.target_id).collect();
            thread::Builder::new()
                .name("vdisplay-teardown-watch".into())
                .spawn(move || {
                    thread::sleep(Duration::from_secs(10));
                    if !done.load(Ordering::SeqCst) {
                        tracing::error!(
                            target_ids = ?targets,
                            "virtual-display teardown still running after 10s — the driver \
                             REMOVE/CCD restore looks WEDGED; new sessions will block until it returns"
                        );
                    }
                })
                .ok();
        }
        self.regroup(inner);
        for mon in &mons {
            self.remove(dev, mon);
        }
        // Re-arrange the survivors after the REMOVEs, so the departing paths are gone and no
        // gap stays where they sat. No-op below two members.
        self.apply_group_layout(inner);
        done.store(true, Ordering::SeqCst);
    }

    /// One monitor's REMOVE. A retired handle means the driver died or was cycled, and its
    /// monitors with it: the REMOVE is moot, but the group's restore still owes the operator
    /// their desk.
    fn remove(&self, dev: Option<&ControlDevice>, mon: &Monitor) {
        let removed = match dev {
            Some(dev) => self.driver.remove_monitor(dev, &mon.key),
            None => {
                tracing::info!(
                    target_id = mon.target_id,
                    "virtual-display: driver handle retired — the monitor went with it"
                );
                Ok(())
            }
        };
        if let Err(e) = removed {
            // Device died under this monitor — retire so the next session reopens.
            if is_device_gone(&e) {
                self.invalidate_device(&e);
            }
            tracing::warn!(
                target_id = mon.target_id,
                "virtual-display REMOVE failed: {e:#}"
            );
        } else {
            tracing::info!(
                backend = self.driver.name(),
                "virtual-display monitor removed"
            );
        }
    }

    /// The group's share of members leaving the map: [`ladder::regroup`] on what survives.
    fn regroup(&self, inner: &mut MgrInner) {
        let (exclusive, saved) = (inner.group.ccd_exclusive, inner.group.ccd_saved.is_some());
        match ladder::regroup(inner.slots.len(), exclusive, saved) {
            Regroup::Restore => self.restore_group(inner),
            // Re-issue isolate over the shrunk set. Snapshot discarded; the group keeps the
            // first member's.
            Regroup::Reisolate => {
                let _ = isolate_txn(
                    &*self.ccd,
                    "shrink-isolate",
                    &inner.target_keys(),
                    ISOLATE_HOLD,
                    false,
                );
            }
            // Re-promote a survivor rather than leave primary on a departing target. The call
            // recaptures a snapshot, and the group must keep the first member's.
            Regroup::RepromotePrimary => {
                if let Some(&survivor) = inner.target_keys().first() {
                    let keep_saved = inner.group.ccd_saved.take();
                    let _ = set_virtual_primary_ccd(survivor);
                    inner.group.ccd_saved = keep_saved;
                }
            }
            Regroup::Nothing => {}
        }
    }

    /// Release a session's hold through [`lifecycle::State::release`]. The last one applies the
    /// creating device's `keep_alive` ([`linger_for`]): a window lingers, `forever` pins, off
    /// tears down now. A QUIT (`quit_now`) skips the window so a reconnect finds Idle instead
    /// of the Reuse preempt's REMOVE→ADD, but never a pin: only `/display/release` frees that.
    /// A stale lease, or a release on a kept slot, is a no-op; a `joined` lease also gives
    /// back its joined hold. The device handle is fetched before the transition, as
    /// `force_release` does, so `device` is never waited on inside one.
    fn release(&self, slot: u32, generation: u64, quit_now: bool, joined: bool, slept: Duration) {
        use crate::policy::Linger;
        let dev = self.device_handle();
        let mut inner = self.transition();
        let Some(s) = inner
            .slots
            .get_mut(&slot)
            .filter(|s| s.mon.generation == generation)
        else {
            return;
        };
        if joined {
            s.joined = s.joined.saturating_sub(1);
        }
        let linger = lifecycle::after_sleep(
            lifecycle::effective_linger(quit_now, linger_for(s.mon.client_fp)),
            slept,
        );
        match s.life.release(Instant::now(), linger) {
            Release::Decref | Release::Noop => {}
            Release::Pin => tracing::info!(
                slot,
                "virtual-display: last session left — PINNED (keep_alive=forever); free via /display/release"
            ),
            Release::Linger => {
                if let Linger::For(window) = linger {
                    tracing::info!(
                        slot,
                        linger_ms = window.as_millis() as u64,
                        "virtual-display: last session left — lingering before teardown"
                    );
                }
            }
            // Inside the transition, so a racing `acquire` waits rather than ADD into an
            // in-flight REMOVE. The monitor leaves the map first.
            Release::Teardown => {
                let Some(Slot { mon, .. }) = inner.slots.remove(&slot) else {
                    return;
                };
                tracing::info!(
                    slot,
                    quit_now,
                    "virtual-display: last session left — tearing down now, no linger"
                );
                self.teardown_removed(dev.as_deref(), &mut inner, vec![mon]);
            }
        }
    }

    /// `mode_conflict: join`: a reference on another client's live monitor, the oldest one at
    /// `mode`. A rejoin after the owner changed mode finds none at the mode it was admitted
    /// to, and takes the oldest of any: it follows the display, not the size. The output is
    /// [`DisplayOwnership::External`]: the joiner captures it and owns none of it, and its
    /// lease gives back only the reference, so the monitor outlives whichever of the two
    /// leaves first. `None` when no other client's monitor is live. A slot other clients share
    /// (a seat's, or slot 0) is not the joiner's own: skipping it hands the joiner the owner's
    /// monitor as its own, and a second driver encoder on it starves the owner.
    ///
    /// The joiner's quit is not passed on: a viewer that stops must not tear the display down
    /// under its owner's linger policy.
    pub(crate) fn join(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
    ) -> Option<VirtualOutput> {
        let own = match (
            process_slot_plan(),
            resolve_slot_id(client_fp, (mode.width, mode.height)),
        ) {
            (Ok(plan), Ok(slot)) => plan.own_slot(slot),
            _ => None,
        };
        let mut inner = self.transition();
        let slot = inner
            .slots
            .iter()
            .filter_map(|(&slot, Slot { mon, life, .. })| match life {
                lifecycle::State::Active { .. }
                    if Some(slot) != own && mon.gdi_name.is_some() && wudf_alive(mon.wudf_pid) =>
                {
                    let other_mode = (mon.mode.width, mon.mode.height) != (mode.width, mode.height);
                    Some((other_mode, mon.generation, slot))
                }
                _ => None,
            })
            .min()
            .map(|(.., slot)| slot)?;
        let Slot { mon, life, joined } = inner.slots.get_mut(&slot)?;
        life.acquire();
        *joined += 1;
        tracing::info!(
            slot,
            target = %mon.ccd_key(),
            refs = life.refs(),
            "mode-conflict: JOIN — sharing the live display"
        );
        Some(self.output_for(slot, mon, None, DisplayOwnership::External))
    }

    /// Begins IDD-push setup on the client's connector. The returned guard serializes
    /// pipeline build, and the slot resolves under it, so a second client sees the first
    /// one's monitor live. A same-slot reconnect preempts its prior holder; a loop rebuilding
    /// on its own slot, with its own `stop`, is not its prior holder. `None` when
    /// [`slot_id_for`] rejects the slot configuration.
    pub fn begin_idd_setup(
        &'static self,
        client_fp: Option<[u8; 32]>,
        size: (u32, u32),
        stop: Arc<AtomicBool>,
    ) -> Option<std::sync::MutexGuard<'static, ()>> {
        let guard = self.setup_lock.lock().unwrap();
        let slot = slot_id_for(client_fp, size)?;
        let prev = self
            .idd_session_stops
            .lock()
            .unwrap()
            .insert(slot, stop.clone());
        if let Some(prev_stop) = prev.filter(|p| !Arc::ptr_eq(p, &stop)) {
            prev_stop.store(true, Ordering::SeqCst);
            // A prior session still Active: `acquire` preempts only a kept slot (so
            // build-retries join) and would join this stuck monitor's dead swap-chain. So
            // force-preempt once here, under `setup_lock`, where it cannot re-churn.
            if !self.wait_for_slot_released(slot, Duration::from_secs(3))
                && self.preempt_stuck(slot)
            {
                // Async departure before the next ADD, after the commit: the 400 ms ceiling
                // of acquire's Reuse preempt.
                thread::sleep(Duration::from_millis(400));
            }
        }
        Some(guard)
    }

    /// Tears down `slot`'s monitor in one transition while the stopped session still holds it
    /// ([`ladder::prior_holds`]). `false` when it let go since the wait, or no control device
    /// is open.
    fn preempt_stuck(&self, slot: u32) -> bool {
        let Some(dev) = self.device_handle() else {
            return false;
        };
        let mut inner = self.transition();
        let taken = match inner.slots.get(&slot) {
            Some(s) if ladder::prior_holds(s.life, s.joined) => inner.slots.remove(&slot),
            _ => None,
        };
        let Some(Slot { mon, .. }) = taken else {
            return false;
        };
        tracing::warn!(
            slot,
            old_target = mon.target_id,
            "IDD-push setup: force-preempting the stuck-Active prior monitor (its IddCx swap-chain is dead)"
        );
        self.teardown_removed(Some(&*dev), &mut inner, vec![mon]);
        true
    }

    /// Wait up to `timeout` for the view to show the stopped IDD-push session let go of `slot`
    /// ([`ladder::prior_holds`]). `false` ⇒ caller force-preempts.
    pub(crate) fn wait_for_slot_released(&self, slot: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let (life, joined) = self.view.lock().unwrap().life(slot);
            if !ladder::prior_holds(life, joined) {
                return true;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    slot,
                    "IDD-push preempt: prior session didn't release the monitor within {timeout:?} — force-preempting"
                );
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Background timer: [`Self::expire`] every 500 ms, so a physical-screen user gets their
    /// screen back. Marked started only once the spawn succeeded, as
    /// `registry::linux::ensure_timer`: a `Once` would spend itself on a failed spawn and leave
    /// every kept monitor unreaped.
    fn ensure_linger_timer(&'static self) {
        static STARTED: Mutex<bool> = Mutex::new(false);
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        let spawned = thread::Builder::new()
            .name("vdisplay-linger".into())
            .spawn(move || {
                let mut watch = crate::suspend::Watch::new();
                loop {
                    thread::sleep(Duration::from_millis(500));
                    self.age(watch.missed());
                    self.expire(Instant::now());
                }
            });
        match spawned {
            Ok(_) => *started = true,
            Err(e) => tracing::error!(
                error = %e,
                "virtual display: could not start the linger timer — kept monitors will not \
                 expire until a later session retries"
            ),
        }
    }

    /// `missed` of real time passed that the deadlines did not count. Only a wake takes the
    /// transition: a quiet tick stays as free as [`Self::expire`]'s.
    fn age(&self, missed: Duration) {
        if missed.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut inner = self.transition();
        inner
            .slots
            .values_mut()
            .for_each(|s| s.life.age(now, missed));
    }

    /// One linger tick at `now`. Every slot past its deadline ([`lifecycle::State::poll_expiry`])
    /// leaves the map before any teardown, so the group regroups once, over the survivors. The
    /// view says whether anything expired, so a quiet tick takes neither a transition nor a
    /// device handle: a held handle keeps a retired one open. The teardown runs inside the
    /// transition, so an acquire's ADD and isolate wait for it: a late restore de-isolates a
    /// new session.
    fn expire(&self, now: Instant) {
        if self.view.lock().unwrap().expired(now) {
            self.tear_down_where(|s| s.life.poll_expiry(now));
        }
    }

    /// Tears down every slot `leaves` picks, after moving its life, in one transition: all of
    /// them out of the map before the batch's teardown. The device handle is fetched before
    /// the transition. Returns how many left.
    fn tear_down_where(&self, mut leaves: impl FnMut(&mut Slot) -> bool) -> usize {
        let dev = self.device_handle();
        let mut inner = self.transition();
        let picked: Vec<u32> = inner
            .slots
            .iter_mut()
            .filter_map(|(&slot, s)| leaves(s).then_some(slot))
            .collect();
        let mons: Vec<Monitor> = picked
            .iter()
            .filter_map(|slot| inner.slots.remove(slot))
            .map(|s| s.mon)
            .collect();
        let left = mons.len();
        self.teardown_removed(dev.as_deref(), &mut inner, mons);
        left
    }
}

/// Session refcount handle on its slot. `Drop` releases; a stale lease is a no-op.
struct MonitorLease {
    mgr: &'static VirtualDisplayManager,
    slot: u32,
    generation: u64,
    /// Client closed with QUIT (user stop, not a network drop). Read at drop:
    /// a quit tears the monitor down now instead of lingering. `None` = linger.
    quit: Option<Arc<AtomicBool>>,
    /// Taken by [`VirtualDisplayManager::join`]: the release gives back a joined hold.
    joined: bool,
    /// [`crate::suspend::slept`] when the lease began.
    slept_at: Duration,
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        let quit_now = self.quit.as_ref().is_some_and(|q| q.load(Ordering::SeqCst));
        let slept = crate::suspend::slept_since(self.slept_at);
        self.mgr
            .release(self.slot, self.generation, quit_now, self.joined, slept);
    }
}

fn process_slot_plan(
) -> std::result::Result<super::identity::WindowsSlotPlan, super::identity::WindowsSlotError> {
    static PLAN: OnceLock<
        std::result::Result<super::identity::WindowsSlotPlan, super::identity::WindowsSlotError>,
    > = OnceLock::new();
    *PLAN.get_or_init(|| {
        let seat_override = std::env::var_os("PUNKTFUNK_SEAT_DISPLAY_SLOT");
        super::identity::windows_slot_plan(
            seat_override.as_deref(),
            pf_win_display::seats_addon_reserves_display_slots(),
        )
    })
}

/// Resolves one logical connector through the process-wide seat plan. Seat
/// hosts bypass the identity map. Console hosts use its bounded candidate set,
/// with slot 0 for anonymous/GameStream and shared-identity clients, one
/// device at a time ([`WindowsSlotPlan::connector`](super::identity::WindowsSlotPlan::connector)).
fn resolve_slot_id(
    client_fp: Option<[u8; 32]>,
    mode: (u32, u32),
) -> std::result::Result<u32, super::identity::WindowsSlotError> {
    let plan = process_slot_plan()?;
    let ordinary_slot = match plan.ordinary_max() {
        Some(max_id) => super::identity::resolve_slot_bounded(
            client_fp,
            mode,
            crate::policy::Identity::PerClient,
            max_id,
        )
        .unwrap_or(0),
        None => 0,
    };
    let slot = plan.resolve(ordinary_slot)?;
    let held = VDM
        .get()
        .map(VirtualDisplayManager::held_slots)
        .unwrap_or_default();
    Ok(plan.connector(slot, client_fp, &held, || {
        super::identity::device_slot_bounded(client_fp?, mode, false, plan.ordinary_max()?)
    }))
}

/// Returns the connector used by both IDD setup and monitor acquisition.
/// `None` rejects malformed, unreserved, or out-of-range seat overrides before
/// either path can touch the driver.
pub fn slot_id_for(client_fp: Option<[u8; 32]>, mode: (u32, u32)) -> Option<u32> {
    match resolve_slot_id(client_fp, mode) {
        Ok(slot) => Some(slot),
        Err(error) => {
            tracing::error!(%error, "pf-vdisplay connector-slot configuration rejected");
            None
        }
    }
}

/// Wake a dark console display: IddCx commits a new monitor's path inactive while it is off,
/// and no swap-chain follows. A process outside the console session has nothing to wake.
fn wake_console_display() -> Result<()> {
    use pf_win_display::console_display::{ensure_on, Wake};
    if pf_win_display::console_session_mismatch().is_some() {
        return Ok(());
    }
    match ensure_on(Duration::from_secs(3)) {
        Wake::Woken(t) => tracing::info!(
            wake_ms = t.as_millis() as u64,
            "console display woken for the virtual monitor"
        ),
        Wake::StillOff => return Err(crate::DisplayAsleep.into()),
        Wake::AlreadyOn | Wake::Unknown => {}
    }
    Ok(())
}

/// Render-GPU pin: IDD-push NVENC runs on the render adapter, so it must be
/// the selected encoder GPU. Selection lives in
/// [`pf_gpu::resolve_render_adapter_luid`].
fn resolve_render_pin() -> Option<LUID> {
    tracing::info!("IDD push: pinning the render GPU (SET_RENDER_ADAPTER)");
    pf_gpu::resolve_render_adapter_luid()
}

/// PRE-MUTATION baseline for the standby-sink selector (immunity plan WP3a): the targets
/// active before this acquire mutates the set. `None` is not an empty baseline — a target
/// missing from one is a disable candidate, so a failed read would nominate the operator's
/// own display, and the pass is skipped instead.
fn standby_baseline() -> Option<Vec<CcdTargetKey>> {
    // A FRESH read through the display actor (it runs the query, off this thread).
    // A re-stamped last-known-good carries `failures > 0` and is not a baseline.
    pf_win_display::display_events::refresh_and_wait(Duration::from_millis(500))
        .filter(|s| s.is_fresh())
        .map(|s| {
            s.targets
                .iter()
                .filter(|t| t.active)
                .map(|t| t.key)
                .collect()
        })
        // Actor slow or not running: one direct query on this thread. Its `Err`
        // is the case that must not become an empty baseline.
        .or_else(|| {
            pf_win_display::win_display::target_inventory_checked()
                .ok()
                .map(|ts| ts.iter().filter(|t| t.active).map(|t| t.key).collect())
        })
}

/// PnP-disable connected-but-inactive sinks (a standby TV) whose wake events the
/// deactivated-set selector misses. First member only, and only past the [`ladder::SETTLE`]
/// window from `settle_start`, so force-EXTEND physicals are not still mid-activation.
/// Extend leaves active panels untouched by construction.
fn neutralise_standby_sinks(
    ccd: &dyn CcdOps,
    inner: &mut MgrInner,
    added_key: CcdTargetKey,
    baseline_active: &[CcdTargetKey],
    settle_start: Instant,
) {
    if let Some(rest) = ladder::SETTLE.checked_sub(settle_start.elapsed()) {
        thread::sleep(rest);
    }
    for id in ccd.disable_connected_inactive(
        &inner.keep_with(added_key),
        baseline_active,
        pf_win_display::topology_churn::generation(),
    ) {
        if !inner.group.pnp_disabled.contains(&id) {
            inner.group.pnp_disabled.push(id);
        }
    }
}

/// A reused monitor keeps the ADD-time render pin. If the current pick has
/// moved, say so: the session follows the pin; the new pick takes effect on
/// the next CREATE (`/display/release`). Compare against the pin, not
/// `mon.luid` (that is the IddCx display adapter).
fn warn_if_pick_moved(mon: &Monitor) {
    let Some(pin) = mon.render_pin else { return };
    let Some(sel) = pf_gpu::selected_gpu() else {
        return;
    };
    let pick = sel.info.luid();
    if (pick.LowPart, pick.HighPart) != (pin.LowPart, pin.HighPart) {
        tracing::warn!(
            pinned_adapter = format!("{:08x}:{:08x}", pin.HighPart, pin.LowPart),
            current_pick = format!(
                "{:08x}:{:08x} ({}, {})",
                pick.HighPart,
                pick.LowPart,
                sel.info.name,
                sel.source.tag()
            ),
            "reused virtual monitor is pinned to a different render GPU than the current pick — \
             the session follows the pinned GPU; free the display (mgmt /display/release) to \
             recreate it on the picked one"
        );
    }
}

/// Read-only view of one managed slot for `/display/state`.
pub(crate) struct ManagedInfo {
    pub backend: &'static str,
    pub mode: (u32, u32, u32),
    /// [`lifecycle::State::label`].
    pub state: &'static str,
    /// Milliseconds until linger teardown (`None` when active or pinned).
    pub expires_in_ms: Option<u64>,
    pub sessions: u32,
    /// Generation stamp — the `/display/release` slot arg.
    pub generation: u64,
    pub slot_id: u32,
    pub position: (i32, i32),
}

impl VirtualDisplayManager {
    /// Hold client `fp`'s monitor at `offset` from client `anchor`'s, in this arrangement and
    /// every later one; `None` leaves it where the layout puts it. Where `fp`'s monitor sits
    /// now, `None` without one.
    pub(crate) fn place_beside(
        &self,
        fp: [u8; 32],
        anchor: Option<[u8; 32]>,
        offset: Option<(i32, i32)>,
    ) -> Option<(i32, i32)> {
        let mut inner = self.transition();
        let slot = *inner
            .slots
            .iter()
            .find(|(_, s)| s.mon.client_fp == Some(fp))?
            .0;
        if let Some(offset) = offset {
            if let Some(s) = inner.slots.get_mut(&slot) {
                s.mon.beside = Some(Beside { anchor, offset });
            }
            self.apply_group_layout(&mut inner);
        }
        inner.slots.get(&slot).map(|s| s.mon.position)
    }

    /// Each Active slot and the device on it, as the last commit left them.
    fn held_slots(&self) -> Vec<(u32, Option<[u8; 32]>)> {
        self.view.lock().unwrap().held()
    }

    /// Managed slots in acquire (generation) order, as the last commit left them. Empty when
    /// none live. Never waits on a transition.
    pub(crate) fn snapshot(&self) -> Vec<ManagedInfo> {
        let view = self.view.lock().unwrap();
        view.managed(self.driver.name(), Instant::now())
    }

    /// Tear down kept (Lingering or Pinned) monitors now (`/display/release`).
    /// `slot` is a [`ManagedInfo::generation`]; `None` releases every kept one, all out of the
    /// map before the teardown, so the group regroups once.
    /// [`lifecycle::State::force_release`] refuses Active. Returns the number released.
    pub(crate) fn force_release(&self, slot: Option<u64>) -> usize {
        self.tear_down_where(|s| {
            slot.is_none_or(|g| g == s.mon.generation) && s.life.force_release()
        })
    }
}

/// Snapshot managed slots. Empty when no backend has initialised yet.
/// [`VirtualDisplayManager::place_beside`] on the process manager.
pub fn place_beside(
    fp: [u8; 32],
    anchor: Option<[u8; 32]>,
    offset: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    vdm().place_beside(fp, anchor, offset)
}

pub(crate) fn snapshot() -> Vec<ManagedInfo> {
    VDM.get()
        .map(VirtualDisplayManager::snapshot)
        .unwrap_or_default()
}

/// Force-release kept monitors (`slot` = generation stamp, `None` = all).
pub(crate) fn force_release(slot: Option<u64>) -> usize {
    VDM.get().map(|m| m.force_release(slot)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::ccd::fake::{CcdCall, FakeCcd};
    use super::driver::fake::FakeDriver;
    use super::{
        isolate_txn, lifecycle, needs_resize, topology_churn, topology_reassert_gen, CcdTargetKey,
        ControlDevice, Cycle, IsolateOutcome, MgrInner, Mode, Monitor, MonitorKey, Outcome,
        ReassertBreaker, Slot, VirtualDisplayManager, LUID, REASSERT_BREAKER_ROUNDS,
        REASSERT_WINDOW,
    };
    use pf_win_display::snapshot::{DisplaySnapshot, TargetInventory};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    /// The alternating fight (lost / stable / lost …) reaches the breaker through the rolling
    /// window even though the consecutive count never leaves 1, and a conceded group stays so.
    #[test]
    fn an_alternating_fight_reaches_the_breaker_through_the_window() {
        let t0 = Instant::now();
        let mut b = ReassertBreaker::default();
        for round in 1..REASSERT_BREAKER_ROUNDS {
            let lost = t0 + Duration::from_secs(4 * u64::from(round));
            assert_eq!(b.on_cycle(1, lost), Cycle::Reassert { round });
            let stable = lost + Duration::from_secs(2);
            assert_eq!(b.on_cycle(0, stable), Cycle::Stable { was_fighting: 1 });
        }
        let trip = t0 + Duration::from_secs(4 * u64::from(REASSERT_BREAKER_ROUNDS));
        assert_eq!(
            b.on_cycle(1, trip),
            Cycle::Concede {
                rounds: REASSERT_BREAKER_ROUNDS
            },
            "four rounds in 16 s trip the breaker"
        );
        assert_eq!(
            b.on_cycle(1, trip + Duration::from_secs(2)),
            Cycle::Conceded
        );
    }

    /// A minute later the window has drained and a lone re-add is round 1 again.
    #[test]
    fn old_rounds_age_out_of_the_window() {
        let t0 = Instant::now();
        let mut b = ReassertBreaker::default();
        for i in 0..REASSERT_BREAKER_ROUNDS - 1 {
            let lost = t0 + Duration::from_secs(4 * u64::from(i));
            b.on_cycle(1, lost);
            b.on_cycle(0, lost + Duration::from_secs(2));
        }
        let later = t0 + REASSERT_WINDOW + Duration::from_secs(20);
        assert_eq!(b.on_cycle(1, later), Cycle::Reassert { round: 1 });
    }

    const fn m(width: u32, height: u32, refresh_hz: u32) -> Mode {
        Mode {
            width,
            height,
            refresh_hz,
        }
    }

    /// Session re-asks the negotiated rate on every rebuild; that must join
    /// even when the OS clamped refresh. Keying on committed alone hotplugs
    /// every attempt.
    #[test]
    fn a_reacquire_at_the_negotiated_mode_joins_even_when_the_os_clamped_the_refresh() {
        let requested = m(5120, 1440, 240);
        let committed = m(5120, 1440, 120);
        assert!(
            !needs_resize(requested, committed, requested),
            "re-asking for the negotiated mode must JOIN, not hotplug the monitor"
        );
        assert!(!needs_resize(requested, committed, committed));
        assert!(needs_resize(requested, committed, m(3840, 2160, 120)));
        assert!(needs_resize(requested, committed, m(5120, 1440, 60)));
    }

    /// No clamp: both fields agree, so the gate is plain mode equality.
    #[test]
    fn without_a_clamp_the_gate_is_plain_mode_equality() {
        let mode = m(1920, 1080, 60);
        assert!(!needs_resize(mode, mode, mode));
        assert!(needs_resize(mode, mode, m(2560, 1440, 60)));
    }

    /// The topology and re-assert generations are process globals: their tests take turns.
    static GENERATIONS: Mutex<()> = Mutex::new(());

    fn key(target_id: u32) -> CcdTargetKey {
        CcdTargetKey::from_luid_parts(1, 0, target_id)
    }

    /// A live exclusive group: one monitor per target id, in acquire order.
    fn exclusive_group(targets: &[u32]) -> MgrInner {
        let mut inner = MgrInner::default();
        for (generation, &target_id) in (1u64..).zip(targets) {
            let mon = Monitor {
                key: MonitorKey::Session(generation),
                target_id,
                luid: LUID {
                    LowPart: 1,
                    HighPart: 0,
                },
                render_pin: None,
                hw_cursor: false,
                cursor_excluded: false,
                wudf_pid: 0,
                gdi_name: None,
                mode: m(1920, 1080, 60),
                requested_mode: m(1920, 1080, 60),
                resolved_monitor_id: target_id,
                position: (0, 0),
                generation,
                client_fp: None,
                beside: None,
            };
            let life = lifecycle::State::Active { refs: 1 };
            let joined = 0;
            inner.slots.insert(target_id, Slot { mon, life, joined });
        }
        inner.group.ccd_saved = Some((Vec::new(), Vec::new()));
        inner.group.ccd_exclusive = true;
        inner
    }

    /// A manager on fakes with a control device already cached, so no open runs. Leaked for
    /// the `'static` its threads need.
    fn fake_manager(ccd: &FakeCcd) -> &'static VirtualDisplayManager {
        let mgr = VirtualDisplayManager::new(Box::new(FakeDriver), Box::new(ccd.clone()));
        mgr.device.lock().unwrap().current = Some(Arc::new(ControlDevice::stand_in()));
        Box::leak(Box::new(mgr))
    }

    /// An acquire whose preempt isolate is held mid-ladder leaves `snapshot` and the
    /// connector choice answering from the last commit.
    #[test]
    fn snapshot_answers_while_an_acquire_is_mid_ladder() {
        let _turn = GENERATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let ccd = FakeCcd::new();
        let mgr = fake_manager(&ccd);
        // The anonymous acquire's slot 0 is kept beside a live sibling of an exclusive group:
        // the reconnect preempts it, and the survivor's re-isolate is the act that blocks.
        {
            let mut t = mgr.transition();
            *t = exclusive_group(&[0, 7]);
            t.slots.get_mut(&0).unwrap().life = lifecycle::State::Pinned;
        }
        let hold = ccd.hold_next_isolate();
        let acquire = std::thread::spawn(move || {
            mgr.acquire(m(1920, 1080, 60), None, None, false, None)
                .map(|_| ())
        });
        hold.entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the preempt's isolate starts");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let asked = Instant::now();
            let read = (mgr.snapshot(), mgr.held_slots());
            let _ = tx.send((read, asked.elapsed()));
        });
        let read = rx.recv_timeout(Duration::from_secs(5));
        hold.release.send(()).unwrap();
        let err = acquire
            .join()
            .unwrap()
            .expect_err("the fake driver adds no monitor");
        let ((snapshot, held), waited) = read.expect("snapshot answers while the isolate is held");
        assert!(
            waited < Duration::from_millis(100),
            "snapshot took {waited:?}"
        );
        let rows: Vec<(u32, &str)> = snapshot.iter().map(|i| (i.slot_id, i.state)).collect();
        assert_eq!(rows, [(0, "pinned"), (7, "active")]);
        assert_eq!(held, [(7, None)]);
        assert!(format!("{err:#}").contains("adds no monitors"), "{err:#}");
        let after: Vec<u32> = mgr.snapshot().iter().map(|i| i.slot_id).collect();
        assert_eq!(after, [7]);
        let reisolate = CcdCall::Isolate {
            keep: vec![key(7)],
            store: false,
        };
        assert_eq!(ccd.take_calls(), [reisolate]);
        topology_churn::release();
    }

    /// The owner's reconnect finds its monitor held only by a joiner: the setup's wait sees
    /// the stopped session let go at once, and nothing is preempted.
    #[test]
    fn an_owner_reconnect_leaves_a_joiner_its_display() {
        let mgr = fake_manager(&FakeCcd::new());
        {
            let mut t = mgr.transition();
            *t = exclusive_group(&[0]);
            t.slots.get_mut(&0).unwrap().mon.gdi_name = Some(r"\\.\DISPLAY9".into());
        }
        let joiner = mgr
            .join(m(1920, 1080, 60), None)
            .expect("slot 0 is joinable");
        assert_eq!(joiner.ownership, super::DisplayOwnership::External);
        // The owner's session ends; the joiner's hold is the only one left.
        mgr.release(0, 1, false, false, Duration::ZERO);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        mgr.idd_session_stops
            .lock()
            .unwrap()
            .insert(0, stopped.clone());
        let asked = Instant::now();
        let setup = mgr.begin_idd_setup(None, (1920, 1080), Arc::new(Default::default()));
        let waited = asked.elapsed();
        assert!(setup.is_some());
        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            waited < Duration::from_secs(1),
            "the setup waited {waited:?}"
        );
        let rows: Vec<(u32, &str, u32)> = mgr
            .snapshot()
            .iter()
            .map(|i| (i.slot_id, i.state, i.sessions))
            .collect();
        assert_eq!(rows, [(0, "active", 1)]);
        // Its release would tear the display down under the host's linger policy.
        std::mem::forget(joiner);
    }

    /// Kept monitors that expire in one tick leave the map before any teardown: the group
    /// re-isolates once, over the survivors alone.
    #[test]
    fn expiries_in_one_tick_regroup_once() {
        let _turn = GENERATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let rows: [(&[u32], &[u32]); 3] = [(&[1, 2], &[7]), (&[1, 2, 3], &[7]), (&[1], &[7, 8])];
        for (expiring, surviving) in rows {
            let ccd = FakeCcd::new();
            let mgr = fake_manager(&ccd);
            let t0 = Instant::now();
            {
                let mut t = mgr.transition();
                *t = exclusive_group(&[expiring, surviving].concat());
                for slot in expiring {
                    t.slots.get_mut(slot).unwrap().life = lifecycle::State::Lingering { until: t0 };
                }
            }
            mgr.expire(t0 + Duration::from_secs(1));
            let left: Vec<u32> = mgr.snapshot().iter().map(|i| i.slot_id).collect();
            assert_eq!(left, surviving, "{expiring:?}");
            let keep = surviving.iter().map(|&s| key(s)).collect();
            let reisolate = CcdCall::Isolate { keep, store: false };
            assert_eq!(ccd.take_calls(), [reisolate], "{expiring:?}");
        }
        topology_churn::release();
    }

    #[test]
    fn a_commit_reports_the_rows_it_dropped_then_the_rows_it_added() {
        use super::{display_changes, ManagedInfo};
        use crate::DisplayEvent;
        let row = |generation, width| ManagedInfo {
            backend: "fake",
            mode: (width, 1080, 60),
            state: "active",
            expires_in_ms: None,
            sessions: 1,
            generation,
            slot_id: 0,
            position: (0, 0),
        };
        let said = |before: &[ManagedInfo], after: &[ManagedInfo]| -> Vec<String> {
            let said = display_changes(before, after).into_iter().map(|e| match e {
                DisplayEvent::Released { count } => format!("released {count}"),
                DisplayEvent::Created { width, .. } => format!("created {width}"),
            });
            said.collect()
        };
        // A kept monitor preempted and recreated: its departure first.
        let kept = [row(1, 1920), row(2, 1280)];
        assert_eq!(
            said(&kept, &[row(2, 1280), row(3, 2560)]),
            ["released 1", "created 2560"]
        );
        // Two expiries in one tick are one event.
        assert_eq!(said(&kept, &[]), ["released 2"]);
        // A re-arrival at a new mode keeps its generation, and a join changes no row.
        assert!(said(&[row(1, 1920)], &[row(1, 2560)]).is_empty());
        assert!(said(&kept, &kept).is_empty());
    }

    fn target(key: CcdTargetKey, active: bool) -> TargetInventory {
        TargetInventory {
            key,
            target_id: key.target_id,
            active,
            external_physical: false,
            internal_panel: false,
            tech: "test",
            friendly: String::new(),
            monitor_device_path: String::new(),
            ours: false,
            gdi_name: String::new(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            refresh_mhz: 0,
            primary: false,
            hdr: None,
            wcg: None,
            sdr_white_level: None,
            source_id: 0,
            source_adapter_luid: 0,
        }
    }

    fn snapshot(
        generation: u64,
        failures: u32,
        targets: Vec<TargetInventory>,
    ) -> Arc<DisplaySnapshot> {
        Arc::new(DisplaySnapshot {
            generation,
            taken_at: Instant::now(),
            failures,
            targets: Arc::from(targets),
        })
    }

    /// Only a path the verification read saw switch off moves the generation. A first read
    /// that failed attempted nothing, so it leaves no restore snapshot either.
    #[test]
    fn an_isolate_moves_the_generation_only_on_a_verified_switch_off() {
        let _turn = GENERATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let keep = [key(7), key(8)];
        let verified = |deactivated| IsolateOutcome::Verified {
            attempts: 1,
            deactivated,
        };
        for (observed, outcome) in [
            (Some(verified(2)), Outcome::Changed),
            (Some(verified(0)), Outcome::Unchanged),
            (Some(IsolateOutcome::NothingActive), Outcome::Unchanged),
            (
                Some(IsolateOutcome::Unverified { attempts: 3 }),
                Outcome::Unknown,
            ),
            (None, Outcome::Unknown),
        ] {
            let ccd = FakeCcd::new();
            ccd.script(|s| s.isolates.push_back(observed));
            let before = topology_churn::generation();
            let (saved, finished) = isolate_txn(&ccd, "test", &keep, Duration::from_secs(60), true);
            assert_eq!(finished.outcome, outcome, "{observed:?}");
            assert_eq!(saved.is_some(), observed.is_some(), "{observed:?}");
            let moved = u64::from(outcome == Outcome::Changed);
            assert_eq!(topology_churn::generation(), before + moved, "{observed:?}");
            let keep = keep.to_vec();
            assert_eq!(ccd.take_calls(), [CcdCall::Isolate { keep, store: true }]);
        }
        topology_churn::release();
    }

    /// The watch acts only on a read it can trust: a live exclusive isolate, a snapshot
    /// that is not last-known-good, and one that carries a target of ours.
    #[test]
    fn the_exclusive_watch_counts_survivors_only_on_a_read_it_can_trust() {
        let (ours, tv) = (key(7), key(9));
        let ccd = FakeCcd::new();

        // No live exclusive isolate: the display is not read at all.
        let mut primary = exclusive_group(&[7]);
        primary.group.ccd_exclusive = false;
        let mut unsaved = exclusive_group(&[7]);
        unsaved.group.ccd_saved = None;
        let empty = exclusive_group(&[]);
        for inner in [&primary, &unsaved, &empty] {
            assert!(inner.exclusive_survivors(&ccd).is_none());
        }
        assert!(ccd.take_calls().is_empty());

        let inner = exclusive_group(&[7]);
        let lit = vec![target(ours, true), target(tv, true)];
        let dark = vec![target(ours, true), target(tv, false)];
        for (snap, survivors) in [
            // Last-known-good after two failed refreshes.
            (snapshot(4, 2, lit.clone()), None),
            // None of our targets: an untrustworthy read, not an empty desk.
            (snapshot(4, 0, vec![target(tv, true)]), None),
            (snapshot(4, 0, lit.clone()), Some(1)),
            (snapshot(4, 0, dark), Some(0)),
            // The one-off read before the actor has published.
            (snapshot(0, 0, lit), Some(1)),
        ] {
            ccd.script(|s| s.snapshot = snap);
            let got = inner.exclusive_survivors(&ccd);
            assert_eq!(got, survivors.map(|n| (vec![ours], n)));
        }
    }

    /// One watch cycle that finds a display re-lit re-asserts the isolate and stores it.
    /// Only a verified switch-off bumps the generation the stream heals from and parks the
    /// display's devnode, once.
    #[test]
    fn a_reassert_heals_the_stream_only_after_a_verified_switch_off() {
        let _turn = GENERATIONS.lock().unwrap_or_else(|e| e.into_inner());
        let (ours, tv) = (key(7), key(9));
        let ccd = FakeCcd::new();
        ccd.script(|s| {
            s.snapshot = snapshot(4, 0, vec![target(ours, true), target(tv, true)]);
            s.disabled = vec!["DISPLAY\\TV\\1".into()];
        });
        let mut inner = exclusive_group(&[7]);
        let (keep, survivors) = inner.exclusive_survivors(&ccd).expect("a trusted read");
        let cycle = ReassertBreaker::default().on_cycle(survivors, Instant::now());
        assert_eq!(cycle, Cycle::Reassert { round: 1 });
        ccd.take_calls();
        let isolate = CcdCall::Isolate {
            keep: keep.clone(),
            store: true,
        };
        let hold = Duration::from_secs(60);

        for observed in [
            None,
            Some(IsolateOutcome::Unverified { attempts: 3 }),
            Some(IsolateOutcome::NothingActive),
        ] {
            ccd.script(|s| s.isolates.push_back(observed));
            let before = topology_reassert_gen();
            inner.reassert_isolate(&ccd, &keep, hold);
            assert_eq!(topology_reassert_gen(), before, "{observed:?}");
            assert_eq!(ccd.take_calls(), std::slice::from_ref(&isolate));
        }
        assert!(inner.group.pnp_disabled.is_empty());

        // `PUNKTFUNK_STANDBY_SINK_KEEP` opts the park out, and a test sets no environment; so
        // does the operator's `pnp_disable_monitors: false`, which this path used to ignore.
        let prefs = crate::policy::prefs();
        let parks = prefs.standby_sink_neutralise() && prefs.pnp_disable_monitors();
        for _ in 0..2 {
            let switched_off = IsolateOutcome::Verified {
                attempts: 1,
                deactivated: 1,
            };
            ccd.script(|s| s.isolates.push_back(Some(switched_off)));
            let before = topology_reassert_gen();
            inner.reassert_isolate(&ccd, &keep, hold);
            assert_eq!(topology_reassert_gen(), before + 1);
            let mut want = vec![isolate.clone()];
            if parks {
                want.push(CcdCall::DisableConnectedInactive {
                    keep: keep.clone(),
                    baseline_active: Vec::new(),
                    generation: topology_churn::generation(),
                });
            }
            assert_eq!(ccd.take_calls(), want);
        }
        assert_eq!(inner.group.pnp_disabled.len(), usize::from(parks));
        topology_churn::release();
    }
}
