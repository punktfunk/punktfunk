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
//! Evidence: `design/display-management.md`.

use std::collections::BTreeMap;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, LUID, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use super::{DisplayOwnership, Mode, VirtualOutput};
use crate::driver::ControlDevice;
use crate::lifecycle::{self, Acquire, Release};
use pf_win_display::win_display::{
    count_other_active, force_extend_topology, isolate_displays_ccd, isolate_displays_ccd_checked,
    resolve_gdi_name, restore_displays_ccd, set_active_mode, set_virtual_primary_ccd,
    wait_mode_settled, wait_target_departed, CcdTargetKey, IsolateOutcome, SavedConfig,
};

#[path = "manager/driver.rs"]
mod driver;
pub(crate) use driver::{AddedMonitor, MonitorKey, VdisplayDriver};

#[path = "manager/instance.rs"]
mod instance;
use instance::claim_instance;
pub use instance::claim_instance_eagerly;

#[path = "manager/knobs.rs"]
mod knobs;
use knobs::{linger_for, topology_action};

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
}

impl Monitor {
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
}

/// What an acquire on this slot is, asked of the lifecycle without taking a hold: Create when
/// empty, Join when Active, Reuse when kept. Windows answers Reuse with preempt-and-recreate:
/// a reused IddCx swap-chain is dead.
fn acquire_kind(slot: Option<&Slot>) -> Acquire {
    let mut life = slot.map_or_else(lifecycle::State::default, |s| s.life);
    life.acquire()
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
}

/// Outcome of a mid-stream re-arrival ([`VirtualDisplayManager::re_add`]).
///
/// Three-way on purpose: `re_add` REMOVEs the old driver monitor before ADD,
/// so a failed ADD leaves no live monitor. Putting the old `Monitor` back
/// would make later `acquire` join a departed `target_id`.
enum ReAdd {
    Arrived(Box<Monitor>),
    /// ADD failed; the old mode was re-ADDed. The monitor is real but new —
    /// a re-arrival mints a fresh `target_id` — so the caller must store this
    /// one, not the struct it handed in.
    RolledBack {
        mon: Box<Monitor>,
        err: anyhow::Error,
    },
    /// ADD and rollback both failed. Leave the slot empty so the next
    /// `acquire` creates one rather than joining a phantom.
    Lost(anyhow::Error),
}

/// Topology work a non-last-member teardown owes the group.
/// Extracted so the gate is testable without a driver or a desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShrinkAction {
    Reisolate,
    /// Departing member may have been holding primary.
    RepromotePrimary,
    Nothing,
}

/// One stage of [`VirtualDisplayManager::resolve_target_gdi`]'s ladder: poll for the target's GDI name until
/// the 3 s ceiling. 50 ms sampling (latency plan P0.5) — a typical activation resolves on an early
/// poll, so finer sampling shaves ~150 ms off every stage crossing.
///
/// Extracted because the ladder ran this loop three times verbatim, and the 2nd and 3rd copies
/// documented themselves as "SAFETY: as the resolve loop above" — a pointer to a proof rather than
/// a proof, which silently rots the moment the block it points at moves.
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

/// [`isolate_displays_ccd`] with the test seam in front of it. Every call site in this file goes
/// through here so an injected failure exercises the same gates a real one would.
fn isolate_displays_ccd_seam(keep: &[CcdTargetKey]) -> Option<SavedConfig> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        if FAIL_NEXT_ISOLATES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            tracing::warn!(
                keep = ?keep,
                "TEST fault injection: forcing isolate_displays_ccd -> None"
            );
            return None;
        }
    }
    isolate_displays_ccd(keep)
}

/// [`isolate_displays_ccd_checked`] behind the same test seam — the re-assert watchdog's variant,
/// whose recovery generation must follow the OBSERVED outcome (immunity plan WP10 item 4).
fn isolate_displays_ccd_checked_seam(
    keep: &[CcdTargetKey],
) -> Option<(SavedConfig, IsolateOutcome)> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        if FAIL_NEXT_ISOLATES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return None;
        }
    }
    isolate_displays_ccd_checked(keep)
}

/// The transaction outcome an isolate observed (immunity plan WP10 item 4 / WP11): only paths
/// that verifiably switched off count as a change.
fn isolate_txn_outcome(outcome: Option<IsolateOutcome>) -> pf_win_display::topology_churn::Outcome {
    use pf_win_display::topology_churn::Outcome;
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

/// [`ShrinkAction`] a non-last-member teardown owes the group.
///
/// Discriminator is `ccd_exclusive`, not `has_saved`: `Topology::Primary`
/// also stores a `ccd_saved` snapshot. Keying on the snapshot ran the
/// exclusive isolate on a Primary group and blanked the physical displays
/// Primary exists to keep lit.
fn shrink_action(ccd_exclusive: bool, has_saved: bool) -> ShrinkAction {
    if ccd_exclusive {
        ShrinkAction::Reisolate
    } else if has_saved {
        ShrinkAction::RepromotePrimary
    } else {
        ShrinkAction::Nothing
    }
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

/// Slot map plus the one group record. One lock for both: every group
/// mutation happens on a slot transition, so splitting them invites lock-order
/// bugs.
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
    fn exclusive_survivors(&self) -> Option<(Vec<CcdTargetKey>, u32)> {
        if self.group.ccd_saved.is_none() || !self.group.ccd_exclusive {
            return None;
        }
        let keep = self.target_keys();
        if keep.is_empty() {
            return None;
        }
        // Not FRESH: the actor's last query failed, or it never published.
        let snap = pf_win_display::display_events::snapshot_or_query();
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

    /// One re-assert, as a topology transaction (immunity plan WP10): descriptor-following
    /// holds for `hold`, swap-chain bounce included, and the transaction finishes with what
    /// the verification read observed. Only an observed change bumps
    /// [`topology_reassert_gen`] and parks the re-lit display's devnode for the session.
    fn reassert_isolate(&mut self, keep: &[CcdTargetKey], hold: Duration) {
        use pf_win_display::topology_churn::{self, Outcome};
        let txn = topology_churn::begin("exclusive-reassert", hold);
        let outcome = isolate_txn_outcome(isolate_displays_ccd_checked_seam(keep).map(|(_, o)| o));
        let finished = topology_churn::finish(txn, outcome);
        if outcome != Outcome::Changed {
            return;
        }
        // The forced re-commit hands the IDD path a fresh swap-chain: the session
        // re-attaches capture instead of streaming a frozen frame.
        TOPOLOGY_REASSERT_GEN.fetch_add(1, Ordering::Relaxed);
        // A display that re-lit itself while held off would evict again on its next HPD
        // pulse. Leased like the acquire's park, re-enabled at teardown.
        if !crate::policy::prefs().standby_sink_neutralise() {
            return;
        }
        let parked = pf_win_display::monitor_devnode::disable_connected_inactive(
            keep,
            &[],
            finished.generation,
        );
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

/// Device-level watchdog pinger, running while any slot lives (any IOCTL
/// bumps the watchdog, so one thread serves N monitors). Same stop+join
/// shape as the exclusive-topology re-assert watchdog.
struct Pinger {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Pinger {
    /// Signal and join. The pinger's join is bounded by its interval (watchdog/3 s), the
    /// exclusive watch's by its 250 ms slices: it only `try_lock`s the state lock, so stopping
    /// it under that lock cannot deadlock.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
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
    state: Mutex<MgrInner>,
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
    VDM.get_or_init(|| VirtualDisplayManager {
        driver,
        device: Mutex::new(DeviceSlot::default()),
        watchdog_s: AtomicU32::new(3),
        driver_proto: AtomicU32::new(0),
        update_modes_futile: AtomicBool::new(false),
        generation: AtomicU64::new(1),
        state: Mutex::new(MgrInner::default()),
        setup_lock: Mutex::new(()),
        idd_session_stops: Mutex::new(std::collections::HashMap::new()),
        pinger: Mutex::new(None),
        exclusive_watch: Mutex::new(None),
    })
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

impl VirtualDisplayManager {
    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
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
        // The `device` mutex serialises the open itself; `state` was never what protected
        // it, and holding it here stalls every acquire and management read behind the
        // PowerShell the open may spawn.
        self.ensure_device().map(|_| ())
    }

    /// Acquires the process-resolved connector slot, then joins or creates its
    /// monitor. A seat override is resolved before any driver access. The
    /// returned [`MonitorLease`] releases only its matching generation.
    pub(crate) fn acquire(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
        client_hdr: Option<pf_frame::HdrMeta>,
        hw_cursor: bool,
        quit: Option<Arc<AtomicBool>>,
    ) -> Result<VirtualOutput> {
        if let Some((own, console)) = pf_win_display::console_session_mismatch() {
            let seat_session = std::env::var_os("PUNKTFUNK_SEAT_SESSION");
            if super::identity::is_seat_session_marker(seat_session.as_deref()) {
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
        self.ensure_linger_timer();
        let slot =
            resolve_slot_id(client_fp, (mode.width, mode.height)).map_err(anyhow::Error::new)?;
        // Before `state`: the open can spawn PowerShell for an adapter reload, and every
        // release and management read waits on this lock meanwhile.
        let dev = self.ensure_device()?;
        let mut inner = self.state.lock().unwrap();
        let mut kind = acquire_kind(inner.slots.get(&slot));

        // IDD-push: Reuse of a kept (Lingering/Pinned) slot is a reconnect, and a reused IddCx
        // swap-chain is dead, so preempt and create fresh. Never preempt Active here: that is a
        // live lease (build-retry or concurrent session), and REMOVE→ADD ends in 0x80070490.
        if kind == Acquire::Reuse {
            kind = Acquire::Create;
            if let Some(Slot { mon, .. }) = inner.slots.remove(&slot) {
                let old_key = mon.ccd_key();
                tracing::info!(
                    slot,
                    old_target = %old_key,
                    "IDD-push reconnect — preempting the kept (lingering/pinned) monitor, recreating a fresh one"
                );
                self.teardown_removed(Some(&*dev), &mut inner, mon);
                // Let the OS finish the ASYNC monitor departure before the next ADD; a back-to-back
                // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
                // Verified-state wait, ceiling = the old fixed 400 ms settle (latency plan P0.3).
                let departed = wait_target_departed(old_key, Duration::from_millis(400));
                if !departed {
                    tracing::debug!(
                        old_target = %old_key,
                        "preempted monitor still in the active CCD set after the departure ceiling"
                    );
                }
            }
        }

        // Active whose WUDFHost has exited is dead driver-side. The rebuild
        // re-acquires while the old lease is still held, so the slot is Active.
        // Join would hand it a stale target. Preempt; generation-stamped
        // leases no-op on release. WUDFHost death is all-slot shared fate.
        if kind == Acquire::Join
            && inner
                .slots
                .get(&slot)
                .is_some_and(|s| !wudf_alive(s.mon.wudf_pid))
        {
            kind = Acquire::Create;
            if let Some(Slot { mon, .. }) = inner.slots.remove(&slot) {
                let old_key = mon.ccd_key();
                tracing::warn!(
                    slot,
                    old_target = %old_key,
                    wudf_pid = mon.wudf_pid,
                    "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
                );
                self.teardown_removed(Some(&*dev), &mut inner, mon);
                // Same async-departure settle as the reconnect preempt above (verified wait, P0.3).
                let _ = wait_target_departed(old_key, Duration::from_millis(400));
            }
        }

        // Live monitor on this slot — join (refcount++). Covers concurrent
        // same-client sessions and mid-stream Reconfigure overlap.
        if kind == Acquire::Join {
            // A different mode is a mid-stream resize. Diff against both
            // [`needs_resize`] sides: negotiated and committed. `mon.mode`
            // alone is not the discriminator — a clamped refresh disagrees
            // for the monitor's life and every rebuild would look like resize.
            let (req_mode, cur_mode) = match inner.slots.get(&slot) {
                Some(s) => (s.mon.requested_mode, s.mon.mode),
                None => unreachable!("Join means a live slot"),
            };
            if needs_resize(req_mode, cur_mode, mode) {
                // In-place first: an already-advertised resolution is CCD-forced
                // on the same monitor (identity, swap-chain, stash survive).
                // Out-of-list fails fast in `resize_in_place` and falls through
                // to re-arrival.
                {
                    let in_place = {
                        let Some(Slot { mon, life }) = inner.slots.get_mut(&slot) else {
                            unreachable!("Join means a live slot");
                        };
                        match self.resize_in_place(&dev, mon, mode) {
                            Ok(()) => {
                                // One more hold for the new (build-then-drop) lease;
                                // generation untouched so the old lease stays valid.
                                life.acquire();
                                let out = self.output_for(slot, mon, quit.clone());
                                tracing::info!(
                                    slot,
                                    refs = life.refs(),
                                    backend = self.driver.name(),
                                    "virtual monitor resized IN PLACE (identity + swap-chain kept)"
                                );
                                Some(out)
                            }
                            Err(e) => {
                                // First-seen size: OS pins settable modes at
                                // arrival; re-arrival teaches it. Info, not warn.
                                tracing::info!(
                                    slot,
                                    reason = %format!("{e:#}"),
                                    "in-place resize not possible — monitor re-arrival"
                                );
                                None
                            }
                        }
                    };
                    if let Some(out) = in_place {
                        // Width changed — re-arrange so auto-row siblings do
                        // not overlap (no-op for a single member).
                        self.apply_group_layout(&mut inner);
                        return Ok(out);
                    }
                }
                let Some(Slot { mon, mut life }) = inner.slots.remove(&slot) else {
                    unreachable!("Join means a live slot");
                };
                let new_mon = match self.re_add(&dev, &mut inner, slot, &mon, mode, client_hdr) {
                    ReAdd::Arrived(m) => *m,
                    ReAdd::RolledBack {
                        mon: recovered,
                        err,
                    } => {
                        // Store the recovered monitor, not the one handed in:
                        // that one's driver monitor was REMOVEd, so key /
                        // target_id / gdi_name are dead. generation/life kept
                        // so leases stay valid.
                        inner.slots.insert(
                            slot,
                            Slot {
                                mon: *recovered,
                                life,
                            },
                        );
                        return Err(err).context("mid-stream resize re-arrival");
                    }
                    ReAdd::Lost(err) => {
                        // Leave the slot empty so the next acquire ADDs cleanly — but if that
                        // was the last slot, nothing else will ever restore the group: the
                        // desktop stays isolated, the monitors PnP-disabled and the EDID
                        // pinned, with the pinger still running against a monitor that is gone.
                        if inner.slots.is_empty() {
                            self.restore_group(&mut inner);
                        }
                        return Err(err).context("mid-stream resize re-arrival (slot left empty)");
                    }
                };
                // `re_add` preserved generation so both leases match on release.
                // One more hold for the new (build-then-drop) lease.
                life.acquire();
                let out = self.output_for(slot, &new_mon, quit);
                inner.slots.insert(slot, Slot { mon: new_mon, life });
                // Width changed — re-arrange so auto-row siblings do not
                // overlap (no-op for a single member).
                self.apply_group_layout(&mut inner);
                tracing::info!(
                    slot,
                    refs = life.refs(),
                    backend = self.driver.name(),
                    "virtual monitor re-arrived for a mid-stream resize"
                );
                return Ok(out);
            }
            // Same mode — concurrent-session join (refcount++), no re-arrival.
            // A monitor the OS never activated carries no GDI name, and a plain join hands that
            // same dead target back, so the caller's whole retry budget re-reads one failure. The
            // monitor is live, so run the activation ladder again before joining.
            let unresolved = match inner.slots.get(&slot) {
                Some(s) if s.mon.gdi_name.is_none() => Some(s.mon.ccd_key()),
                _ => None,
            };
            if let Some(key) = unresolved {
                if let Some(name) = self.resolve_target_gdi(key) {
                    tracing::info!(
                        slot,
                        target = %key,
                        gdi_name = %name,
                        "virtual-display target activated on a later acquire"
                    );
                    if let Some(s) = inner.slots.get_mut(&slot) {
                        s.mon.gdi_name = Some(name);
                    }
                }
            }
            let Some(Slot { mon, life }) = inner.slots.get_mut(&slot) else {
                unreachable!("Join means a live slot");
            };
            life.acquire();
            tracing::info!(
                slot,
                refs = life.refs(),
                backend = self.driver.name(),
                "virtual monitor reused (concurrent session)"
            );
            warn_if_pick_moved(mon);
            return Ok(self.output_for(slot, mon, quit));
        }

        // Fail-closed backstop if a session got past admission. `max_displays`
        // counts Active+Lingering+Pinned; one live slot can never trip it.
        let max = crate::policy::prefs().get().effective().max_displays;
        if inner.slots.len() as u32 >= max {
            anyhow::bail!(
                "display budget exhausted: {} display(s) live/kept, max_displays = {max} — freeing \
                 one (session end, linger expiry, or /display/release) admits the next",
                inner.slots.len()
            );
        }

        let mut mon = match self.create_monitor(&dev, mode, slot, client_hdr, hw_cursor, &mut inner)
        {
            // Cached device died under us. Retire, reopen, retry once so a
            // reconnect after driver restart does not burn a failed session.
            Err(e) if is_device_gone(&e) => {
                self.invalidate_device(&e);
                let dev = self.ensure_device()?;
                tracing::info!(
                    "virtual-display control device reopened — retrying the monitor create"
                );
                self.create_monitor(&dev, mode, slot, client_hdr, hw_cursor, &mut inner)?
            }
            r => r?,
        };
        mon.client_fp = client_fp;
        let out = self.output_for(slot, &mon, quit);
        let mut life = lifecycle::State::default();
        life.acquire(); // Create: Idle → Active { refs: 1 }
        inner.slots.insert(slot, Slot { mon, life });
        // Arrange live members and commit desktop origins in one CCD apply.
        // A single member sits at the origin — this no-ops.
        self.apply_group_layout(&mut inner);
        Ok(out)
    }

    /// [`VirtualOutput`] for `mon` in `slot`: preferred mode, capture target,
    /// generation-stamped lease. `quit` is read by the lease `Drop` (see
    /// [`Self::release`]).
    fn output_for(
        &'static self,
        slot: u32,
        mon: &Monitor,
        quit: Option<Arc<AtomicBool>>,
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
            }),
            // Manager owns the monitor lifecycle, so the registry treats it
            // as Owned (it delegates via `vd.create`).
            ownership: DisplayOwnership::Owned,
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
                    thread::sleep(interval);
                }
            });
        // Not `thread::spawn`: that panics if the OS refuses the thread, and
        // this holds `pinger` plus (via create_monitor ← acquire) `state`.
        // Unwind poisons both locks. A missing pinger degrades to the driver
        // watchdog; a poisoned manager does not.
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
    /// A verified [`isolate_displays_ccd`] is not durable: the isolated topology
    /// is deliberately not saved to the CCD database (teardown must restore the
    /// user's layout), so a later re-resolution can bring the stored layout back.
    ///
    /// Each cycle, at most every [`knobs::exclusive_reassert_ms`]: wait, `try_lock` the
    /// state, read the survivors, ask the [`ReassertBreaker`], act. A re-assert evicts via full
    /// isolate; the forced re-commit restarts presentation, and the session heals the
    /// swap-chain bounce off [`topology_reassert_gen`]. Teardown stops+joins this thread while
    /// holding the state lock, so a blocking `lock()` here would deadlock, and the wait is
    /// sliced so stop+join is bounded by ~250 ms, not a full cycle.
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
                    let Ok(mut inner) = self.state.try_lock() else {
                        continue;
                    };
                    let Some((keep, survivors)) = inner.exclusive_survivors() else {
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
                            pf_win_display::topology_churn::release();
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
                            pf_win_display::topology_churn::release();
                        }
                        Cycle::Reassert { round } => {
                            tracing::warn!(
                                survivors,
                                round,
                                "exclusive topology lost — a non-managed display re-activated \
                                 after the verified isolate (hybrid-GPU driver / display-poller \
                                 software restoring the saved layout?); re-asserting the isolate"
                            );
                            inner.reassert_isolate(&keep, interval + Duration::from_secs(3));
                        }
                    }
                }
            });
        // Not `.expect()`: this holds `exclusive_watch` and (via the caller)
        // `state`. A panic poisons both. A missing watchdog is a degradation;
        // wedging the manager is not.
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
        let placements = arrange(&members, &layout_policy);
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
    /// return its GDI name — the capture target. Shared by the fresh CREATE and the mid-stream
    /// re-arrival ([`re_add`](Self::re_add)). `None` on a GPU-less box (target added but not
    /// WDDM-activated); the capture backend re-resolves once a GPU is present.
    ///
    /// A three-stage ladder, each stage a real failure mode. Plain poll FIRST — never a forced
    /// topology change: bare `SDC_TOPOLOGY_EXTEND` is ACCESS_DENIED from Session 0 on a headless
    /// box and breaks the auto-activate. force-EXTEND is the integrated-screen FALLBACK: a fresh
    /// IDD there is CLONED onto the panel (shared source, no committed path of its own —
    /// observed on an Optimus laptop, commit 8e87e61), and the EXTEND de-clones it. LAST RESORT
    /// is explicit path activation: a lid-closed laptop defeats both (the clamshell policy
    /// suppresses auto-activation and the EXTEND preset "succeeds" committing nothing) —
    /// `activate_target_path` commits the path directly, which ignores the lid policy.
    ///
    /// CAVEAT (unobserved): textbook CCD also allows a clone with a *shared-source ACTIVE* path
    /// (resolve → `Some`), which the `is_none()` gate would miss; widening needs a
    /// `target_is_cloned` helper plus on-laptop validation.
    ///
    /// Call under the `state` lock — this mutates the live CCD topology, and the manager's
    /// sole-topology-mutator contract keeps two acquires from interleaving path commits (a
    /// serialization requirement, not a soundness one: every helper it calls is a safe fn).
    fn resolve_target_gdi(&self, key: CcdTargetKey) -> Option<String> {
        // 50 ms sampling (latency plan P0.5): the SAME 3 s per-stage ceilings — the 3-stage ladder
        // structure encodes real failure modes (headless auto-activate, integrated-panel clone,
        // lid-closed path activation) and is untouched — but a typical activation resolves on an
        // early poll, so finer sampling shaves ~150 ms off every stage crossing.
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
        None
    }

    /// Wakes a dark console display, adds the exact logical connector, resolves its GDI
    /// path, sets its mode, and applies group topology. A driver's fallback connector is removed and
    /// rejected because it could enter another process's reserved range.
    /// `Monitor.mode` records the committed mode while `requested_mode` retains
    /// the negotiated value for the join/resize gate.
    fn create_monitor(
        &'static self,
        dev: &ControlDevice,
        mut mode: Mode,
        slot: u32,
        client_hdr: Option<pf_frame::HdrMeta>,
        hw_cursor: bool,
        inner: &mut MgrInner,
    ) -> Result<Monitor> {
        wake_console_display()?;
        // Slot id doubles as the driver-preferred monitor id (EDID serial /
        // ConnectorIndex) so Windows reapplies saved DPI on reconnect; `0`
        // (anonymous) = driver auto-allocates.
        let preferred_id = slot;
        let slot_plan = process_slot_plan().map_err(anyhow::Error::new)?;
        // Negotiated mode, before the post-settle read-back overwrites `mode`.
        // The session re-asks for this on every rebuild (see `requested_mode`).
        let requested_mode = mode;
        let render_pin = resolve_render_pin();
        // Gate on an opened device: the version is 0 until the handshake ran, and the capture
        // layer must not create a section nobody will publish into.
        let hw_cursor = hw_cursor && self.driver_proto.load(Ordering::Relaxed) != 0;
        let first_member = inner.slots.is_empty();
        let baseline_active = if first_member && crate::policy::prefs().standby_sink_neutralise() {
            standby_baseline()
        } else {
            None
        };
        let added =
            self.driver
                .add_monitor(dev, mode, render_pin, preferred_id, client_hdr, hw_cursor)?;
        self.refuse_cross_slot(dev, slot_plan, &added)?;
        let added_key =
            CcdTargetKey::from_luid_parts(added.luid.LowPart, added.luid.HighPart, added.target_id);

        // Ping inside the watchdog window or the driver tears all displays
        // down. One device-level pinger; started with the first monitor.
        self.ensure_pinger();

        // Resolve the capture target — wait for Windows to auto-activate the freshly-ADDed IDD into its
        // OWN display path, with the integrated-screen clone fallback (shared by the re-arrival path).
        // Its `state`-lock discipline is satisfied: `acquire` holds the lock across this whole call.
        let gdi_name = self.resolve_target_gdi(added_key);
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    target = %added_key,
                    gdi = %n,
                    "IDD target activated into a display path"
                );
                // ADD only advertises; force the mode so DXGI captures the requested size. CCD
                // takes it as data; GDI can only pick from an enumeration that has not refreshed
                // this soon after arrival.
                if !pf_win_display::win_display::set_active_mode_ccd(added_key, mode)
                    && !set_active_mode(n, mode)
                {
                    tracing::warn!(
                        target = %added_key,
                        mode = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                        "neither CCD nor GDI wrote the mode — the display stays on the one it runs"
                    );
                }
                self.apply_group_topology_on_add(inner, added_key, n, first_member);
                // Verified-state wait before capture opens. Ceiling 1500 ms.
                // A rejected mode burns the ceiling.
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added_key, mode, Duration::from_millis(1500));
                tracing::info!(
                    settle_ms = settle_start.elapsed().as_millis() as u64,
                    verified = settled,
                    "topology settle (verified-state wait)"
                );
                // Record what actually COMMITTED, not what was asked for — the same read-back
                // `resize_in_place` does, for the same reason. `set_active_mode` deliberately falls
                // back to the highest advertised refresh <= requested rather than lose the client's
                // resolution, and `wait_mode_settled` verifies the RESOLUTION only, so `settled`
                // says nothing about the refresh. Storing the request would make `mon.mode` claim a
                // rate the display is not running: `output_for` hands the capturer that as
                // `preferred_mode` (the encoder then paces to a rate the output never reaches),
                // `/display/state` reports it, and the next Reconfigure diffs against it — a client
                // re-requesting the rate it actually has would pay a needless resize, while one
                // re-requesting the phantom rate takes the plain JOIN branch and never tries again.
                mode = committed_mode_or(added_key, mode);
                // No baseline ⇒ no pass: see `standby_baseline`.
                if let Some(baseline) = baseline_active.as_deref()
                    && crate::policy::prefs().standby_sink_neutralise()
                {
                    neutralise_standby_sinks(inner, added_key, baseline, settle_start);
                }
            }
            None => tracing::warn!(
                "virtual-display target {added_key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
            ),
        }

        Ok(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            requested_mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: (0, 0),
            generation: self.generation.fetch_add(1, Ordering::Relaxed),
            hw_cursor,
            cursor_excluded: added.cursor_excluded,
            // `acquire` records the creating device.
            client_fp: None,
        })
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

    /// Host topology for a member that just activated as `gdi`. The CCD isolate is a property
    /// of the whole managed GROUP, not of one member, so a per-device topology has no meaning
    /// here — the first member's answer already governs the group (design §6.1). Exclusive:
    /// the first member captures the restore, later ones re-isolate the grown set.
    /// Primary/Extend leave physicals lit.
    fn apply_group_topology_on_add(
        &'static self,
        inner: &mut MgrInner,
        added_key: CcdTargetKey,
        gdi: &str,
        first_member: bool,
    ) {
        use crate::policy::Topology;
        match topology_action(None) {
            Topology::Exclusive if first_member => self.isolate_group_first(inner, added_key, gdi),
            Topology::Exclusive => {
                // Re-isolate so the fresh member joins the composited set. Discard the
                // snapshot unless the first member's isolate failed — then adopt this one,
                // or teardown cannot restore the physicals.
                let snap = isolate_displays_ccd_seam(&inner.keep_with(added_key));
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
        // The acquire isolate is a topology TRANSACTION (immunity plan WP10/WP11):
        // descriptor-following holds for its deadline, the generation moves only on an
        // OBSERVED change, and the PnP leases below are stamped with it.
        let txn = pf_win_display::topology_churn::begin("acquire-isolate", Duration::from_secs(3));
        let isolated = isolate_displays_ccd_checked_seam(&inner.keep_with(added_key));
        let outcome = isolated.as_ref().map(|(_, o)| *o);
        inner.group.ccd_saved = isolated.map(|(saved, _)| saved);
        let finished = pf_win_display::topology_churn::finish(txn, isolate_txn_outcome(outcome));
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
    /// On failure `mon` is untouched and the caller falls back to [`re_add`].
    /// Call under the `state` lock: CCD/GDI runs inside.
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
        let settled = wait_mode_settled(mon_key, mode, Duration::from_millis(1500));
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

    /// Mid-stream resize by monitor re-arrival.
    ///
    /// The driver freezes the advertised mode list at ADD, so an out-of-list
    /// `ChangeDisplaySettingsExW` returns `DISP_CHANGE_BADMODE`. REMOVE then ADD
    /// at the new mode, reusing the slot's stable id so the OS keeps identity and
    /// saved DPI. Visible cost is one hotplug.
    ///
    /// The rebuilt `Monitor` keeps the old `generation` so outstanding leases
    /// still match. Group restore snapshot is preserved (not a first-member
    /// create). Caller owns the slot's `Monitor` + `refs` across this call.
    fn re_add(
        &'static self,
        dev: &ControlDevice,
        inner: &mut MgrInner,
        slot: u32,
        old: &Monitor,
        mode: Mode,
        client_hdr: Option<pf_frame::HdrMeta>,
    ) -> ReAdd {
        tracing::info!(
            slot,
            old = format!(
                "{}x{}@{}",
                old.mode.width, old.mode.height, old.mode.refresh_hz
            ),
            new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
            old_target = old.target_id,
            "virtual-display: re-arriving monitor for a mid-stream resize (exact mode)"
        );
        // Bare REMOVE (no topology restore, pinger stays up). Frees the
        // preferred id so ADD can reuse it. Best-effort: ADD still proceeds
        // on REMOVE failure (driver reaps a stale same-id monitor anyway).
        if let Err(e) = self.driver.remove_monitor(dev, &old.key) {
            tracing::warn!(
                old_target = old.target_id,
                "re-arrival REMOVE failed (continuing to ADD): {e:#}"
            );
        }
        // Async departure before ADD. Ceiling 400 ms; the driver's ghost-reap
        // ADD retry is the backstop if CCD reports departed early.
        let depart_start = std::time::Instant::now();
        let departed = wait_target_departed(old.ccd_key(), Duration::from_millis(400));
        tracing::info!(
            depart_ms = depart_start.elapsed().as_millis() as u64,
            verified = departed,
            "re-arrival: old monitor departure settle"
        );
        let render_pin = resolve_render_pin();
        let first = self
            .driver
            .add_monitor(dev, mode, render_pin, slot, client_hdr, old.hw_cursor);
        let (added, mut mode, rollback_err) = match first {
            Ok(a) => (a, mode, None),
            Err(e) => {
                // Old monitor already REMOVEd. Re-ADD at its requested mode
                // (not committed): those differ when refresh was clamped, and
                // re-ADDing the clamp would make the next acquire at the
                // negotiated rate look like another resize.
                let e = e.context("re-arrival ADD at the new mode");
                tracing::warn!(
                    slot,
                    error = %format!("{e:#}"),
                    "re-arrival ADD failed — rolling back to the previous mode"
                );
                match self.driver.add_monitor(
                    dev,
                    old.requested_mode,
                    render_pin,
                    slot,
                    client_hdr,
                    old.hw_cursor,
                ) {
                    Ok(a) => (a, old.requested_mode, Some(e)),
                    Err(e2) => {
                        tracing::error!(
                            slot,
                            error = %format!("{e2:#}"),
                            "re-arrival rollback ADD also failed — the slot now has NO monitor"
                        );
                        return ReAdd::Lost(e.context(format!("rollback also failed: {e2:#}")));
                    }
                }
            }
        };
        // What this ADD asked for, before the post-settle read-back overwrites
        // `mode`. Join/resize gate's other side (see `Monitor::requested_mode`).
        let requested_mode = mode;
        self.ensure_pinger();
        let added_key =
            CcdTargetKey::from_luid_parts(added.luid.LowPart, added.luid.HighPart, added.target_id);
        // 3. Resolve the NEW target's GDI name (target_id changes across a re-arrival). Under the
        //    `state` lock, as its topology-mutator discipline requires.
        let gdi_name = self.resolve_target_gdi(added_key);
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    "re-arrival target {added_key} -> {n}"
                );
                // ADD only advertises; force the mode so DXGI/IDD capture the new size. CCD first:
                // it takes the size as data, while GDI can only pick from an enumeration that has
                // not refreshed this soon after the arrival — which left the path on the old mode,
                // and on a seat with no swap chain at all.
                if !pf_win_display::win_display::set_active_mode_ccd(added_key, mode)
                    && !set_active_mode(n, mode)
                {
                    tracing::warn!(
                        target = %added_key,
                        mode = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                        "neither CCD nor GDI wrote the mode — the display stays on the one it runs"
                    );
                }
                // 4. Re-isolate the composited set with the NEW target replacing the old — preserving
                //    the group's first-member restore snapshot. Under the `state` lock (the caller
                //    holds it and lent us `inner`), as its topology-mutator discipline requires.
                self.reisolate_after_swap(inner, added_key);
                // Topology settle before capture reopens: verified-state wait, ceiling = the old
                // fixed 1500 ms sleep (latency plan P0.2 — the re-arrival twin).
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added_key, mode, Duration::from_millis(1500));
                tracing::info!(
                    settle_ms = settle_start.elapsed().as_millis() as u64,
                    verified = settled,
                    "re-arrival topology settle (verified-state wait)"
                );
                // Store what COMMITTED, not what was asked for — the settle above verifies the
                // resolution only, so it is no evidence about the refresh (see
                // [`committed_mode_or`]). Doing this here rather than at the `Monitor` construction
                // below keeps it on the arm where a path actually exists: with no GDI name there is
                // no committed mode to read, and the request stands.
                mode = committed_mode_or(added_key, mode);
            }
            None => tracing::warn!(
                "re-arrival target {added_key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
            ),
        }
        // Preserve `generation` (lease continuity) and group-layout `position`.
        // A fresh generation would strand the old session's release.
        let mon = Box::new(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            requested_mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: old.position,
            generation: old.generation,
            hw_cursor: old.hw_cursor,
            // Fresh from this reply, not `old`: the driver's per-target
            // declare registry is ground truth.
            cursor_excluded: added.cursor_excluded,
            client_fp: old.client_fp,
        });
        match rollback_err {
            None => ReAdd::Arrived(mon),
            Some(err) => ReAdd::RolledBack { mon, err },
        }
    }

    /// Re-isolate after [`re_add`] put a new target in place of the old one,
    /// without recapturing the group restore snapshot. The old slot is already
    /// gone from the map, so `inner.target_ids()` is surviving siblings.
    ///
    /// Call under the `state` lock — it commits a new CCD topology, so it must not interleave with
    /// another slot transition's commit. A *serialization* requirement, not a soundness one: every
    /// helper it reaches (`isolate_displays_ccd_seam`, `set_virtual_primary_ccd`) is a safe fn, so
    /// this body performs no unsafe operation. (`&mut MgrInner` already proves the lock is held.)
    fn reisolate_after_swap(&self, inner: &mut MgrInner, new_target: CcdTargetKey) {
        use crate::policy::Topology;
        // As above: one isolate for the group, so the host's answer.
        match topology_action(None) {
            Topology::Exclusive => {
                // Grown-set semantics: isolate to the surviving siblings + the new target. The returned
                // snapshot is DISCARDED — the group keeps the first member's (design §6.1).
                let _ = isolate_displays_ccd_seam(&inner.keep_with(new_target));
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

    /// Tear down `mon`, already removed from `inner.slots`. Last member: stop
    /// the pinger and restore group topology. Non-last: re-issue isolate over
    /// the shrunk set. Then REMOVE. Consumes `mon`.
    fn teardown_removed(&self, dev: Option<&ControlDevice>, inner: &mut MgrInner, mon: Monitor) {
        // Runs under the `state` lock, so a REMOVE/CCD-restore that never
        // returns blocks every future `acquire` with nothing in the log.
        // One ERROR after 10 s turns that silent wedge into a diagnosis.
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            let target = mon.target_id;
            thread::Builder::new()
                .name("vdisplay-teardown-watch".into())
                .spawn(move || {
                    thread::sleep(Duration::from_secs(10));
                    if !done.load(Ordering::SeqCst) {
                        tracing::error!(
                            target_id = target,
                            "virtual-display teardown still running after 10s — the driver \
                             REMOVE/CCD restore looks WEDGED; new sessions will block until it returns"
                        );
                    }
                })
                .ok();
        }
        let last_member = inner.slots.is_empty();
        if last_member {
            self.restore_group(inner);
        } else {
            match shrink_action(inner.group.ccd_exclusive, inner.group.ccd_saved.is_some()) {
                // Re-issue isolate over the shrunk set. Snapshot discarded;
                // the group keeps the first member's.
                ShrinkAction::Reisolate => {
                    let keep = inner.target_keys();
                    let _ = isolate_displays_ccd_seam(&keep);
                }
                // Re-promote a survivor rather than leave primary on a target
                // about to be REMOVEd. Save/restore the snapshot: the call
                // recaptures one and the group must keep the first member's.
                ShrinkAction::RepromotePrimary => {
                    if let Some(&survivor) = inner.target_keys().first() {
                        let keep_saved = inner.group.ccd_saved.take();
                        let _ = set_virtual_primary_ccd(survivor);
                        inner.group.ccd_saved = keep_saved;
                    }
                }
                ShrinkAction::Nothing => {}
            }
        }
        // A retired handle means the driver died or was cycled, and its monitors with it: the
        // REMOVE is moot, but the group's restore above still owes the operator their desk.
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
        // Re-arrange survivors. Leaving never called `apply_group_layout`,
        // so CCD origins kept a gap where the departing monitor sat. After
        // REMOVE so the departing path is gone. No-op below two members.
        self.apply_group_layout(inner);
        done.store(true, Ordering::SeqCst);
    }

    /// Release a session's hold through [`lifecycle::State::release`]. The last one applies the
    /// creating device's `keep_alive` ([`linger_for`]): a window lingers, `forever` pins, off
    /// tears down now. A QUIT (`quit_now`) skips the window so a reconnect finds Idle instead
    /// of the Reuse preempt's REMOVE→ADD, but never a pin: only `/display/release` frees that.
    /// A stale lease, or a release on a kept slot, is a no-op.
    fn release(&self, slot: u32, generation: u64, quit_now: bool) {
        use crate::policy::Linger;
        let mut inner = self.state.lock().unwrap();
        let Some(s) = inner
            .slots
            .get_mut(&slot)
            .filter(|s| s.mon.generation == generation)
        else {
            return;
        };
        let linger = lifecycle::effective_linger(quit_now, linger_for(s.mon.client_fp));
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
            // Under the state lock, so a racing `acquire` waits rather than ADD
            // into an in-flight REMOVE. The monitor leaves the map first.
            Release::Teardown => {
                let Some(Slot { mon, .. }) = inner.slots.remove(&slot) else {
                    return;
                };
                tracing::info!(
                    slot,
                    quit_now,
                    "virtual-display: last session left — tearing down now, no linger"
                );
                self.teardown_removed(self.device_handle().as_deref(), &mut inner, mon);
            }
        }
    }

    /// `mode_conflict: join`: a reference on another client's live monitor, the oldest one at
    /// `mode`. A rejoin after the owner changed mode finds none at the mode it was admitted
    /// to, and takes the oldest of any: it follows the display, not the size. The output is
    /// [`DisplayOwnership::External`]: the joiner captures it and owns none of it, and its
    /// lease gives back only the reference, so the monitor outlives whichever of the two
    /// leaves first. `None` when no other client's monitor is live. A seat's clients all
    /// resolve to its one slot, so there the joiner's own slot is the owner's.
    ///
    /// The joiner's quit is not passed on: a viewer that stops must not tear the display down
    /// under its owner's linger policy.
    pub(crate) fn join(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
    ) -> Option<VirtualOutput> {
        let own = match process_slot_plan() {
            Ok(plan) if plan.seat_slot().is_some() => None,
            _ => resolve_slot_id(client_fp, (mode.width, mode.height)).ok(),
        };
        let mut inner = self.state.lock().unwrap();
        let slot = inner
            .slots
            .iter()
            .filter_map(|(&slot, Slot { mon, life })| match life {
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
        let Slot { mon, life } = inner.slots.get_mut(&slot)?;
        life.acquire();
        tracing::info!(
            slot,
            target = %mon.ccd_key(),
            refs = life.refs(),
            "mode-conflict: JOIN — sharing the live display"
        );
        let mut out = self.output_for(slot, mon, None);
        out.ownership = DisplayOwnership::External;
        Some(out)
    }

    /// Begins IDD-push setup on an already-resolved connector, the one
    /// [`slot_id_for`] handed the caller. The returned guard serializes pipeline
    /// build, and a same-slot reconnect preempts its prior holder.
    pub fn begin_idd_setup(
        &'static self,
        slot: u32,
        stop: Arc<AtomicBool>,
    ) -> std::sync::MutexGuard<'static, ()> {
        let guard = self.setup_lock.lock().unwrap();
        let prev = self.idd_session_stops.lock().unwrap().insert(slot, stop);
        if let Some(prev_stop) = prev {
            prev_stop.store(true, Ordering::SeqCst);
            if !self.wait_for_slot_released(slot, Duration::from_secs(3)) {
                // Prior session still Active. `acquire` preempts only a kept
                // slot (so build-retries join), and would JOIN this stuck
                // monitor's dead swap-chain. Force-preempt once here under
                // `setup_lock` — not inside `acquire`, which would re-churn.
                if let Some(dev) = self.device_handle() {
                    let mut inner = self.state.lock().unwrap();
                    let taken = match inner.slots.get(&slot) {
                        Some(s) if matches!(s.life, lifecycle::State::Active { .. }) => {
                            inner.slots.remove(&slot)
                        }
                        // Raced to Lingering/empty between the wait and here.
                        _ => None,
                    };
                    if let Some(Slot { mon, .. }) = taken {
                        tracing::warn!(
                            slot,
                            old_target = mon.target_id,
                            "IDD-push setup: force-preempting the stuck-Active prior monitor (its IddCx swap-chain is dead)"
                        );
                        self.teardown_removed(Some(&*dev), &mut inner, mon);
                        // Async departure before the next ADD (same 400 ms
                        // ceiling as acquire's Reuse preempt).
                        thread::sleep(Duration::from_millis(400));
                    }
                }
            }
        }
        guard
    }

    /// Wait up to `timeout` for `slot` to leave Active. Used after signalling
    /// the old IDD-push session to stop. `false` ⇒ caller force-preempts.
    pub(crate) fn wait_for_slot_released(&self, slot: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !matches!(
                self.state.lock().unwrap().slots.get(&slot),
                Some(Slot {
                    life: lifecycle::State::Active { .. },
                    ..
                })
            ) {
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

    /// Background timer: tear down a monitor whose [`lifecycle::State::poll_expiry`] fires, so a
    /// physical-screen user gets their screen back. Marked started only once the spawn
    /// succeeded, as `registry::linux::ensure_timer`: a `Once` would spend itself on a failed
    /// spawn and leave every kept monitor unreaped.
    fn ensure_linger_timer(&'static self) {
        static STARTED: Mutex<bool> = Mutex::new(false);
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        let spawned = thread::Builder::new()
            .name("vdisplay-linger".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(500));
                    let dev = self.device_handle();
                    let mut g = self.state.lock().unwrap();
                    let now = Instant::now();
                    let expired: Vec<u32> = g
                        .slots
                        .iter_mut()
                        .filter_map(|(slot, s)| s.life.poll_expiry(now).then_some(*slot))
                        .collect();
                    for slot in expired {
                        if let Some(Slot { mon, .. }) = g.slots.remove(&slot) {
                            // Teardown under the state lock. Dropping it
                            // first let a concurrent acquire ADD + isolate
                            // while this REMOVE/restore was in flight; the
                            // late restore then de-isolated the new session.
                            self.teardown_removed(dev.as_deref(), &mut g, mon);
                        }
                    }
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
}

/// Session refcount handle on its slot. `Drop` releases; a stale lease is a no-op.
struct MonitorLease {
    mgr: &'static VirtualDisplayManager,
    slot: u32,
    generation: u64,
    /// Client closed with QUIT (user stop, not a network drop). Read at drop:
    /// a quit tears the monitor down now instead of lingering. `None` = linger.
    quit: Option<Arc<AtomicBool>>,
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        let quit_now = self.quit.as_ref().is_some_and(|q| q.load(Ordering::SeqCst));
        self.mgr.release(self.slot, self.generation, quit_now);
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
/// with slot 0 retained for anonymous/GameStream clients.
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
    plan.resolve(ordinary_slot)
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
/// deactivated-set selector misses. First member only, and only past the 1500 ms settle
/// window from `settle_start`, so force-EXTEND physicals are not still mid-activation.
/// Extend leaves active panels untouched by construction.
fn neutralise_standby_sinks(
    inner: &mut MgrInner,
    added_key: CcdTargetKey,
    baseline_active: &[CcdTargetKey],
    settle_start: Instant,
) {
    if let Some(rest) = Duration::from_millis(1500).checked_sub(settle_start.elapsed()) {
        thread::sleep(rest);
    }
    for id in pf_win_display::monitor_devnode::disable_connected_inactive(
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
    /// Snapshot managed slots in acquire (generation) order. Empty when none live.
    pub(crate) fn snapshot(&self) -> Vec<ManagedInfo> {
        let inner = self.state.lock().unwrap();
        let mut out: Vec<ManagedInfo> = inner
            .slots
            .iter()
            .map(|(slot, Slot { mon, life })| {
                let expires_in_ms = match life {
                    lifecycle::State::Lingering { until } => {
                        Some(until.saturating_duration_since(Instant::now()).as_millis() as u64)
                    }
                    _ => None,
                };
                ManagedInfo {
                    backend: self.driver.name(),
                    mode: (mon.mode.width, mon.mode.height, mon.mode.refresh_hz),
                    state: life.label(),
                    expires_in_ms,
                    sessions: life.refs(),
                    generation: mon.generation,
                    slot_id: *slot,
                    position: mon.position,
                }
            })
            .collect();
        out.sort_by_key(|i| i.generation);
        out
    }

    /// Tear down kept (Lingering or Pinned) monitors now (`/display/release`).
    /// `slot` is a [`ManagedInfo::generation`]; `None` releases every kept one.
    /// [`lifecycle::State::force_release`] refuses Active. Returns the number released.
    pub(crate) fn force_release(&self, slot: Option<u64>) -> usize {
        let dev = self.device_handle();
        let mut inner = self.state.lock().unwrap();
        let kept: Vec<u32> = inner
            .slots
            .iter_mut()
            .filter_map(|(k, s)| {
                (slot.is_none_or(|g| g == s.mon.generation) && s.life.force_release()).then_some(*k)
            })
            .collect();
        let mut released = 0usize;
        for k in kept {
            if let Some(Slot { mon, .. }) = inner.slots.remove(&k) {
                self.teardown_removed(dev.as_deref(), &mut inner, mon);
                released += 1;
            }
        }
        released
    }
}

/// Snapshot managed slots. Empty when no backend has initialised yet.
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
    use super::{
        needs_resize, shrink_action, Cycle, Mode, ReassertBreaker, ShrinkAction,
        REASSERT_BREAKER_ROUNDS, REASSERT_WINDOW,
    };
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

    /// Non-last-member teardown keys off `ccd_exclusive`, not `has_saved`.
    /// Exclusive re-isolates even without a snapshot; Primary never isolates.
    #[test]
    fn a_primary_group_shrink_repromotes_instead_of_isolating() {
        // Snapshot without exclusivity: Primary, not isolate.
        assert_eq!(
            shrink_action(false, true),
            ShrinkAction::RepromotePrimary,
            "a Primary group must never run the exclusive isolate on a shrink"
        );
        assert_eq!(shrink_action(true, true), ShrinkAction::Reisolate);
        assert_eq!(shrink_action(true, false), ShrinkAction::Reisolate);
        assert_eq!(shrink_action(false, false), ShrinkAction::Nothing);
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
}
