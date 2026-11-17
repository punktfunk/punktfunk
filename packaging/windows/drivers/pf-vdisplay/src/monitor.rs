//! Virtual monitors: the [`Monitor`] type and the control-plane verbs on it — create + arrive
//! (`IOCTL_ADD`), the in-place mode update, remove, clear, the owner reap and the encode
//! session; the cursor verbs live in [`crate::cursor`]. Every verb takes the calling process as
//! `owner` and reaches only the monitors that process added.
//!
//! Ownership: [`crate::registry`] holds the only strong `Arc<Monitor>`; the drain worker holds a
//! `Weak`. Every field a worker, a DDI callback or an IOCTL can race on sits behind its own
//! mutex, held for a field swap and never across a DDI, a join, or a `Drop` that closes a
//! handle. Lock order is `REGISTRY → Monitor.*`, never reversed; workers never take the registry.
//!
//! Removal is two steps with no lock held between them: the registry hands the `Arc` back, then
//! [`Monitor::teardown`] stops the workers (cursor first, then the encode session, the drain
//! worker, the pool), and only then does the caller run `IddCxMonitorDeparture`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pf_driver_proto::vdisplay;
use wdk_sys::{NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding, iddcx};

use crate::cursor::CursorState;
use crate::cursor_cell::CursorCell;
use crate::encode::section::{EncodeSession, SessionThread};
use crate::modes::target_mode2;
use crate::registry::{self, lock};
use crate::swap_chain_processor::SwapChainProcessor;
use crate::worker::Sendable;

/// The advertised mode list and its flattening (the order the mode DDIs emit). Both live in
/// [`pf_driver_proto::vdisplay`], where they run under `cargo test` on any OS; re-exported so the
/// mode callbacks keep naming them through this module.
pub use pf_driver_proto::vdisplay::{Mode, flatten};

/// The IddCx monitor handle, set once `IddCxMonitorCreate` returns.
type SendMonitor = Sendable<iddcx::IDDCX_MONITOR>;

/// [`Monitor::state`]: the IddCx lifecycle. `IddCxMonitorDeparture` is legal only on an arrived
/// monitor, so exactly one caller wins the `ARRIVED → DEPARTED` swap and departs it.
const CREATING: u8 = 0;
const ARRIVED: u8 = 1;
const DEPARTED: u8 = 2;

/// What `IddCxMonitorArrival` reported: the OS target id — the key the host addresses every
/// later delivery by — and the render-adapter LUID for the ADD reply.
#[derive(Clone, Copy)]
pub struct Arrival {
    pub target_id: u32,
    pub luid_low: u32,
    pub luid_high: i32,
}

/// A live (or pending) virtual monitor.
///
/// The registry holds the only strong `Arc`; the drain worker holds a `Weak` it upgrades per
/// pass, so a monitor is never kept alive by its own worker. Identity is plain fields, the
/// handle and the arrival record are write-once, and everything else has its own mutex, held
/// for a field swap only. Whatever a swap displaces goes back to the caller, who drops it with
/// no lock held: a join or a handle close under a lock head-blocks the control plane and every
/// mode DDI the OS issues during the same topology change.
///
/// `gone` is set first thing in [`teardown`](Self::teardown): an install landing after it gets
/// its value handed back instead of parking a worker on a monitor nobody will join.
pub struct Monitor {
    /// The process whose ADD created this monitor; every later verb must come from it.
    pub owner: u32,
    /// EDID serial / connector index — the key the mode DDIs match on.
    pub id: u32,
    /// The owner's key (ADD/REMOVE); two owners may use the same value.
    pub session_id: u64,
    /// The host asked for an IddCx hardware cursor at ADD.
    pub hw_cursor: bool,
    /// When the entry was created — the watchdog reap skips a still-initializing monitor.
    pub created_at: Instant,
    object: OnceLock<SendMonitor>,
    arrival: OnceLock<Arrival>,
    /// `CREATING`, `ARRIVED` or `DEPARTED`; see [`Monitor::claim_departure`].
    state: AtomicU8,
    /// Advertised modes (requested mode first, then the host's extras and history).
    modes: Mutex<Vec<Mode>>,
    /// The rate the requested mode runs when that is not whole hertz, in millihertz; zero
    /// otherwise. Every listed mode at the same whole rate runs it ([`vdisplay::rate_mhz`]).
    exact_mhz: AtomicU32,
    /// The live swap-chain drain worker; dropping it joins the thread.
    swap: Mutex<Option<SwapChainProcessor>>,
    /// The source sequence: advanced only by frames the drain worker hands the pool, and
    /// monotonic across encode sessions — shared with every pool this monitor gets.
    pub source_seq: Arc<AtomicU64>,
    cursor: Mutex<CursorState>,
    /// The live encode session (`SET_ENCODE`); its thread stops with no lock held.
    encode: Mutex<Option<Arc<EncodeSession>>>,
    /// The encode pool, kept across sessions for its retained slot; dropped at teardown.
    pool: Mutex<Option<Arc<crate::encode::pool::Pool>>>,
    /// The newest surface no pool took, which the next pool opens on.
    seed: Arc<crate::encode::pool::Seed>,
    /// Bumped (Release) by every session install or removal, and by every pool change. The
    /// drain loop compares it with its last-seen value and re-reads the slots only then.
    pub encode_gen: AtomicU32,
    /// The render LUID of the last swap-chain assignment, packed; `0` = none yet. The pool
    /// and the encoder open on this device, the one the drain worker acquires from.
    render_luid: std::sync::atomic::AtomicI64,
    gone: AtomicBool,
    /// DDI calls in flight against `object` or the cursor event, held by [`DdiGuard`].
    /// [`teardown`](Self::teardown) waits for zero before closing the event, and the
    /// departure that follows finds no caller still inside IddCx with the handle.
    ddi_inflight: AtomicU32,
}

/// One DDI call against a monitor's handle or cursor event; see [`Monitor::ddi`].
pub struct DdiGuard<'a>(&'a Monitor);

impl Drop for DdiGuard<'_> {
    fn drop(&mut self) {
        self.0.ddi_inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How long [`Monitor::teardown`] waits for in-flight DDI calls. A DDI can re-enter the mode
/// callbacks, which take the registry lock the caller has already released, so it completes.
const DDI_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Take a slot's value with its guard already released, so the caller drops it lock-free.
fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    lock(slot).take()
}

impl Monitor {
    /// A registered-but-not-created entry; [`create_monitor`] fills the handle and arrival in.
    pub(crate) fn pending(
        owner: u32,
        id: u32,
        session_id: u64,
        hw_cursor: bool,
        exact_mhz: u32,
        modes: Vec<Mode>,
    ) -> Self {
        Self {
            owner,
            id,
            session_id,
            hw_cursor,
            created_at: Instant::now(),
            object: OnceLock::new(),
            arrival: OnceLock::new(),
            state: AtomicU8::new(CREATING),
            modes: Mutex::new(modes),
            exact_mhz: AtomicU32::new(exact_mhz),
            swap: Mutex::new(None),
            source_seq: Arc::new(AtomicU64::new(0)),
            cursor: Mutex::new(CursorState::new()),
            encode: Mutex::new(None),
            pool: Mutex::new(None),
            seed: Arc::new(crate::encode::pool::Seed::new()),
            encode_gen: AtomicU32::new(0),
            render_luid: std::sync::atomic::AtomicI64::new(0),
            gone: AtomicBool::new(false),
            ddi_inflight: AtomicU32::new(0),
        }
    }

    /// Hold while calling a DDI against this monitor's handle or its cursor event. `None`
    /// once [`teardown`](Self::teardown) has started: the increment lands before the `gone`
    /// read, so a teardown that saw zero cannot be passed by a caller that saw `!gone`.
    /// Both sides are sequentially consistent: with acquire/release alone a store may pass
    /// the load after it, and each side could miss the other's write.
    pub fn ddi(&self) -> Option<DdiGuard<'_>> {
        self.ddi_inflight.fetch_add(1, Ordering::SeqCst);
        if self.gone.load(Ordering::SeqCst) {
            self.ddi_inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(DdiGuard(self))
    }

    /// Whether [`teardown`](Self::teardown) has run; a waiter stops rather than block on a
    /// monitor that is going away.
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Record the render adapter a swap-chain was just assigned on.
    pub fn set_render_luid(&self, luid: windows::Win32::Foundation::LUID) {
        let packed = (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart);
        self.render_luid.store(packed, Ordering::Release);
    }

    /// The render adapter of the last assignment; `None` before the first.
    pub fn render_luid(&self) -> Option<windows::Win32::Foundation::LUID> {
        let packed = self.render_luid.load(Ordering::Acquire);
        (packed != 0).then_some(windows::Win32::Foundation::LUID {
            LowPart: packed as u32,
            HighPart: (packed >> 32) as i32,
        })
    }

    /// The hardware-cursor state ([`crate::cursor`]). Read the registry (`any_declared`,
    /// `cursor_forward_desired`) before taking this lock, and never take the registry while
    /// holding it: the lock order is `REGISTRY → Monitor.*`.
    pub(crate) fn cursor(&self) -> &Mutex<CursorState> {
        &self.cursor
    }

    /// The cursor the encode pool blends ([`CursorCell`]); the same `Arc` for the monitor's life.
    pub fn cursor_cell(&self) -> Arc<CursorCell> {
        lock(&self.cursor).cell.clone()
    }

    /// The next publish-token generation: one per `SET_ENCODE`, never 0, and unique for the
    /// driver's life. Per monitor is not enough — a re-arrival keeps the target id and starts a
    /// fresh monitor at zero, so a stale close would match the replacement session's token.
    pub fn next_encode_generation(&self) -> u32 {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Install an encode session and wake the drain worker so an idle display picks it up
    /// now. Returns the session it displaced — `Err(session)` once the monitor is torn down —
    /// for the caller to stop with no lock held.
    pub fn set_encode(
        &self,
        session: Arc<EncodeSession>,
    ) -> Result<Option<Arc<EncodeSession>>, Arc<EncodeSession>> {
        let displaced = {
            let mut slot = lock(&self.encode);
            if self.gone.load(Ordering::Acquire) {
                return Err(session);
            }
            slot.replace(session)
        };
        self.bump_encode_gen();
        Ok(displaced)
    }

    /// The live session, if any.
    pub fn encode(&self) -> Option<Arc<EncodeSession>> {
        lock(&self.encode).clone()
    }

    /// Remove the live session, for the caller to stop with no lock held.
    pub fn take_encode(&self) -> Option<Arc<EncodeSession>> {
        let session = take(&self.encode);
        if session.is_some() {
            self.bump_encode_gen();
        }
        session
    }

    /// Remove the live session only if it IS `generation`, matched under the lock. A proxy
    /// closing the session it owns must never take the one that replaced it: a mid-stream resize
    /// installs the replacement first, so an unconditional take leaves the monitor encoding
    /// nothing while the drain worker keeps filling its pool.
    pub fn take_encode_if(&self, generation: u32) -> Option<Arc<EncodeSession>> {
        let session = {
            let mut slot = lock(&self.encode);
            match slot.as_ref() {
                Some(live) if live.generation == generation => slot.take(),
                _ => None,
            }
        };
        if session.is_some() {
            self.bump_encode_gen();
        }
        session
    }

    /// Hand `session` its reopened thread while it is still this monitor's live session; the
    /// thread comes back otherwise, for the caller to stop. Matched under the `encode` lock,
    /// which the take in [`teardown`](Self::teardown) and a replacing `SET_ENCODE` go through
    /// too, so a thread lands in a session someone still stops, never in nobody's.
    pub fn set_thread_if_live(
        &self,
        session: &Arc<EncodeSession>,
        thread: Box<dyn SessionThread>,
    ) -> Result<Option<Box<dyn SessionThread>>, Box<dyn SessionThread>> {
        let slot = lock(&self.encode);
        if slot.as_ref().is_some_and(|live| Arc::ptr_eq(live, session)) {
            Ok(session.set_thread(thread))
        } else {
            Err(thread)
        }
    }

    /// The encode pool, if one was ever built.
    pub fn pool(&self) -> Option<Arc<crate::encode::pool::Pool>> {
        lock(&self.pool).clone()
    }

    /// The surface the next pool opens on ([`crate::encode::pool::Seed`]).
    pub fn seed(&self) -> Arc<crate::encode::pool::Seed> {
        self.seed.clone()
    }

    /// Install a freshly built pool (the encode thread, once its session's kind is known) and
    /// wake the drain worker. The replaced pool's textures drop after the guard.
    pub fn set_pool(&self, pool: Arc<crate::encode::pool::Pool>) {
        let replaced = lock(&self.pool).replace(pool);
        self.bump_encode_gen();
        drop(replaced);
    }

    /// Bump the encode generation and wake the drain worker (`SetEvent` never blocks, so the
    /// `swap` guard may span it).
    fn bump_encode_gen(&self) {
        self.encode_gen.fetch_add(1, Ordering::Release);
        let swap = lock(&self.swap);
        if let Some(p) = &*swap {
            p.wake();
        }
    }

    /// True once the OS has assigned this monitor a swap chain — the seat bring-up waits on it.
    pub fn has_swap_chain(&self) -> bool {
        lock(&self.swap).is_some()
    }

    /// The IddCx handle — `None` until `IddCxMonitorCreate` returned. It is set before the
    /// arrival, so the mode DDIs the arrival re-enters can match on it.
    pub fn object(&self) -> Option<iddcx::IDDCX_MONITOR> {
        self.object.get().map(|o| o.0)
    }

    /// True between a successful arrival and the departure claim.
    fn arrived(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARRIVED
    }

    /// Mark the monitor departed. `true` only when it had arrived: the caller, alone, then runs
    /// `IddCxMonitorDeparture`. A monitor still creating is left to its creator, which sees the
    /// mark after the arrival and departs it itself.
    fn claim_departure(&self) -> bool {
        self.state.swap(DEPARTED, Ordering::AcqRel) == ARRIVED
    }

    /// The OS target id — 0 until arrival, a value the host never sends (OS target ids are
    /// non-zero), so a pending entry matches no target-keyed lookup.
    pub fn target_id(&self) -> u32 {
        self.arrival.get().map_or(0, |a| a.target_id)
    }

    /// A clone of the advertised list, for a lock-free DDI fill.
    pub fn modes(&self) -> Vec<Mode> {
        lock(&self.modes).clone()
    }

    /// [`Self::exact_mhz`], for the same fill.
    pub fn exact_mhz(&self) -> u32 {
        self.exact_mhz.load(Ordering::Acquire)
    }

    /// Install the drain worker for a fresh swap-chain assignment. Returns the processor it
    /// displaced — or `proc` itself once the monitor is torn down — for the caller to drop with
    /// no lock held: dropping one joins its thread.
    #[must_use]
    pub fn set_swap(&self, proc: SwapChainProcessor) -> Option<SwapChainProcessor> {
        let mut slot = lock(&self.swap);
        if self.gone.load(Ordering::Acquire) {
            return Some(proc);
        }
        slot.replace(proc)
    }

    /// Take the drain worker out (unassign / reassign); the caller drops it with no lock held.
    #[must_use]
    pub fn take_swap(&self) -> Option<SwapChainProcessor> {
        take(&self.swap)
    }

    /// Stop everything whose drop blocks, each taken under its own guard and dropped after it:
    /// the cursor worker (it can be inside a hardware-cursor query against the handle the
    /// caller departs next), then the encode session (its thread stops within a bound or is
    /// detached), the drain worker and the pool. The cursor's data event is not closed here: a
    /// declare may name it until the departure, and it closes with the monitor, after that.
    /// `gone` is set first, so a racing install hands its value back instead of landing here
    /// unjoined. Run by the caller that removed this monitor, with the registry lock released.
    pub fn teardown(&self) {
        self.gone.swap(true, Ordering::SeqCst);
        let started = Instant::now();
        // Every DDI caller took its guard before reading `gone`; wait the live ones out before
        // closing the event they were handed and before the handle departs. SeqCst pairs with
        // [`ddi`](Self::ddi).
        while self.ddi_inflight.load(Ordering::SeqCst) != 0 {
            if started.elapsed() > DDI_DRAIN_BUDGET {
                dbglog!(
                    "[pf-vd] monitor teardown: {} DDI call(s) still in flight after {} ms",
                    self.ddi_inflight.load(Ordering::Acquire),
                    DDI_DRAIN_BUDGET.as_millis()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let worker = lock(&self.cursor).worker.take();
        drop(worker);
        if let Some(session) = take(&self.encode) {
            session.stop();
            drop(session);
        }
        drop(take(&self.swap));
        drop(take(&self.pool));
        let took = started.elapsed();
        if took > Duration::from_millis(250) {
            dbglog!("[pf-vd] monitor teardown took {} ms", took.as_millis());
        }
    }
}

/// Tear every removed monitor down, then depart the ones this call claims
/// ([`Monitor::claim_departure`]): a worker can be inside a DDI against the handle departure
/// destroys, so the joins come first. A monitor still creating is marked, and its creator
/// departs it.
fn depart(removed: Vec<Arc<Monitor>>) {
    for m in &removed {
        m.teardown();
    }
    for m in removed {
        if m.claim_departure()
            && let Some(object) = m.object()
        {
            // SAFETY: `object` arrived, and this call won its one departure claim.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(object) };
        }
    }
}

/// Depart every monitor of `owner` that has existed at least `grace` — the owner-gone reap
/// ([`crate::watchdog`]). The grace skips a just-created monitor (the host adds it, then starts
/// pinging) so a momentarily stale ping sample cannot reap a brand-new one. Returns the count.
pub fn reap_owner(owner: u32, grace: Duration) -> usize {
    let removed = registry::remove(|m| m.owner == owner && m.created_at.elapsed() >= grace);
    let n = removed.len();
    depart(removed);
    n
}

/// The seat placeholder's owner and session. Pid 0 is never a requestor, so the pair cannot
/// collide with a host's. It stays for the session's life: it holds the only display path the
/// remoting stack commits, and a monitor arriving into an empty topology gets none of its own.
pub const SEAT_PLACEHOLDER_OWNER: u32 = 0;
pub const SEAT_PLACEHOLDER_SESSION: u64 = 0;

/// `IOCTL_ADD`: create and arrive `owner`'s monitor at the requested mode plus the host's extra
/// sizes ([`vdisplay::advertised_modes`]), named by `req.preferred_monitor_id` (`0` = lowest
/// free) and advertising the client display's luminance volume in its EDID (all-zero = the
/// built-in defaults). Returns `(monitor_id, target_id, adapter_luid_low, adapter_luid_high)` for
/// the [`AddReply`](pf_driver_proto::control::AddReply), or the status the ADD completes with,
/// one of the reply's three classes: `STATUS_DEVICE_NOT_READY` for no adapter yet or a removal
/// mid-create, `STATUS_NOT_FOUND` for a refused `IddCxMonitorCreate`, and a refused arrival's
/// own status. The caller validates the mode.
///
/// The entry is registered pending before `IddCxMonitorCreate`, so the mode DDIs the create
/// re-enters find it by id; the handle and then the arrival are filled in write-once. A create
/// failure reclaims the id. An arrival failure also deletes the created object: only an arrived
/// monitor may depart, and a leaked object pins its slot against the adapter's monitor budget.
/// A removal that ran during the create is settled by [`after_arrival`].
pub fn create_monitor(
    owner: u32,
    req: &pf_driver_proto::control::AddRequest,
) -> Result<(u32, u32, u32, i32), NTSTATUS> {
    let (session_id, width, height, refresh) =
        (req.session_id, req.width, req.height, req.refresh_hz);
    let adapter = crate::adapter::adapter().ok_or(crate::STATUS_DEVICE_NOT_READY)?;
    // One identity per owner and session: a re-ADD of a still-live `session_id` departs the
    // stale monitor first, so no duplicate EDID/target lingers. Another owner's same key is
    // a different monitor.
    if registry::find(|m| m.owner == owner && m.session_id == session_id).is_some() {
        dbglog!(
            "[pf-vd] create_monitor: owner {owner} session {session_id} already live — departing the stale monitor"
        );
        remove_monitor(owner, session_id);
    }
    let modes = vdisplay::advertised_modes(
        Mode {
            width,
            height,
            refresh_rates: vec![refresh],
        },
        crate::adapter::is_seat_role(),
        &req.extra_modes,
        &[],
    );
    let (hw_cursor, preferred_id) = (req.hw_cursor != 0, req.preferred_monitor_id);
    let exact = exact_of(refresh, req.refresh_mhz);
    let monitor = registry::insert(owner, session_id, hw_cursor, preferred_id, exact, modes);
    let id = monitor.id;
    let Some(object) = create_object(adapter, id, req) else {
        remove_entry(&monitor);
        return Err(crate::STATUS_NOT_FOUND);
    };
    let _ = monitor.object.set(Sendable(object));

    // Tell the OS the monitor is plugged in.
    let mut arrival_out = iddcx::IDARG_OUT_MONITORARRIVAL::default();
    // SAFETY: `object` is the just-created IddCx monitor handle. No one departs it before it
    // arrives: `claim_departure` only marks a monitor still creating.
    let st = unsafe { wdk_iddcx::IddCxMonitorArrival(object, &mut arrival_out) };
    dbglog!("[pf-vd] IddCxMonitorArrival(id={id}) -> {st:#x}");
    if !wdk_iddcx::nt_success(st) {
        dbglog!(
            "[pf-vd] IddCxMonitorArrival(id={id}) FAILED — reclaiming the id + deleting the created monitor"
        );
        monitor.state.store(DEPARTED, Ordering::Release);
        remove_entry(&monitor);
        // SAFETY: `object` never arrived, so no departure ran on it, and only this thread
        // deletes it; `WdfObjectDelete` takes a `WDFOBJECT` (a raw handle cast, as in the
        // swap-chain teardown).
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, object as WDFOBJECT);
        }
        return Err(st);
    }

    let arrived = monitor
        .state
        .compare_exchange(CREATING, ARRIVED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let linked = registry::find(|m| m.id == id).is_some_and(|m| Arc::ptr_eq(&m, &monitor));
    if let AfterArrival::Depart { claim_first } = after_arrival(arrived, linked) {
        dbglog!("[pf-vd] create_monitor(id={id}): cleared mid-create — departing the new monitor");
        monitor.teardown();
        if !claim_first || monitor.claim_departure() {
            // SAFETY: `object` arrived successfully just above, and this is its one departure.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(object) };
        }
        return Err(crate::STATUS_DEVICE_NOT_READY);
    }

    let arrival = Arrival {
        target_id: arrival_out.OsTargetId,
        luid_low: arrival_out.OsAdapterLuid.LowPart,
        luid_high: arrival_out.OsAdapterLuid.HighPart,
    };
    // The render flip survives entry churn: start at the session's desired state, stamped
    // before the arrival makes this entry findable by target. The registry is read first.
    let forward_on = registry::cursor_forward_desired(arrival.target_id);
    lock(&monitor.cursor).forward_on = forward_on;
    let _ = monitor.arrival.set(arrival);
    Ok((id, arrival.target_id, arrival.luid_low, arrival.luid_high))
}

/// What the creator does once `IddCxMonitorArrival` succeeded, from whether it won the
/// `CREATING → ARRIVED` swap and whether the registry still links its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterArrival {
    Keep,
    /// Tear down, then depart, after winning [`Monitor::claim_departure`] if `claim_first`.
    Depart {
        claim_first: bool,
    },
}

/// A lost swap means a removal (clear, reap, remove) marked the monitor while it was creating
/// and left the departure to the creator. An arrived entry the registry no longer links (device
/// cleanup, or a remover between its unlink and its claim) departs through the claim, so
/// exactly one side runs `IddCxMonitorDeparture`; kept, it would stay plugged in for good.
fn after_arrival(arrived: bool, linked: bool) -> AfterArrival {
    match (arrived, linked) {
        (false, _) => AfterArrival::Depart { claim_first: false },
        (true, false) => AfterArrival::Depart { claim_first: true },
        (true, true) => AfterArrival::Keep,
    }
}

/// `IddCxMonitorCreate` for monitor `id`: an HDMI connector at index `id` whose EDID (serial
/// `id`) carries `req`'s luminance volume and, when it fits, its mode as the preferred timing.
/// `None` on a refused create.
fn create_object(
    adapter: iddcx::IDDCX_ADAPTER,
    id: u32,
    req: &pf_driver_proto::control::AddRequest,
) -> Option<iddcx::IDDCX_MONITOR> {
    let client_lum = pf_driver_proto::edid::ClientLuminance {
        max_nits: req.max_luminance_nits,
        max_frame_avg_nits: req.max_frame_avg_nits,
        min_millinits: req.min_luminance_millinits,
    };
    let preferred = Some((req.width, req.height, req.refresh_hz));
    let mut edid = pf_driver_proto::edid::generate(id, client_lum, preferred);
    let desc = iddcx::IDDCX_MONITOR_DESCRIPTION {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_DESCRIPTION>() as u32,
        Type: iddcx::IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
        DataSize: edid.len() as u32,
        // SAFETY: `edid` is a local array that outlives this `create_object` call; IddCxMonitorCreate
        // (below) reads through `pData` SYNCHRONOUSLY, before `edid` drops — the pointer never escapes.
        pData: edid.as_mut_ptr().cast(),
    };
    let mut info = iddcx::IDDCX_MONITOR_INFO {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_INFO>() as u32,
        MonitorContainerId: container_guid(id),
        MonitorType:
            wdk_sys::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: id,
        MonitorDescription: desc,
    };
    let mut attr = wdk_sys::WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32,
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    };
    let create_in = iddcx::IDARG_IN_MONITORCREATE {
        ObjectAttributes: &raw mut attr,
        pMonitorInfo: &raw mut info,
    };
    let mut create_out = iddcx::IDARG_OUT_MONITORCREATE::default();
    // SAFETY: adapter is a valid IddCx adapter; create_in points to valid local storage read synchronously.
    let st = unsafe { wdk_iddcx::IddCxMonitorCreate(adapter, &create_in, &mut create_out) };
    dbglog!("[pf-vd] IddCxMonitorCreate(id={id}) -> {st:#x}");
    wdk_iddcx::nt_success(st).then_some(create_out.MonitorObject)
}

/// `IOCTL_UPDATE_MODES`: lead the live monitor's mode list with a new preferred mode and
/// push the target list to the OS via `IddCxMonitorUpdateModes2` — the in-place mid-stream
/// resize. No departure: OS identity, drain worker and encode session all survive.
///
/// The list is a union — new mode first, every previously advertised mode (the ADD's extras
/// included) kept (deduped, capped
/// at [`vdisplay::MODE_LIST_CAP`]) — because the OS pins a monitor's settable set at arrival, so
/// a replacement could only lose settable modes; the union pays off at the next same-id
/// re-arrival, where the registry's mode history makes every previously used mode settable.
///
/// The stored list changes first, so an OS re-query through the mode DDIs sees the new one, and
/// is reverted if the DDI fails so the two stay coherent. The DDI runs with no lock held: it can
/// re-enter the mode-query callbacks. `STATUS_NOT_FOUND` unless `owner` holds `session_id`.
pub fn update_monitor_modes(
    owner: u32,
    session_id: u64,
    width: u32,
    height: u32,
    refresh: u32,
    refresh_mhz: u32,
) -> NTSTATUS {
    let Some(m) = registry::find(|m| m.owner == owner && m.session_id == session_id) else {
        return crate::STATUS_NOT_FOUND;
    };
    let Some(object) = m.object().filter(|_| m.arrived()) else {
        return crate::STATUS_NOT_FOUND; // not arrived, or departing — nothing to update
    };
    let Some(_ddi) = m.ddi() else {
        return crate::STATUS_NOT_FOUND; // torn down under us
    };
    let (old_modes, new_modes) = {
        let mut modes = lock(&m.modes);
        let new_modes = vdisplay::advertised_modes(
            Mode {
                width,
                height,
                refresh_rates: vec![refresh],
            },
            crate::adapter::is_seat_role(),
            &[],
            &modes,
        );
        (
            core::mem::replace(&mut *modes, new_modes.clone()),
            new_modes,
        )
    };

    let exact = exact_of(refresh, refresh_mhz);
    let old_exact = m.exact_mhz.swap(exact, Ordering::AcqRel);
    // The OS's target-mode list for this monitor (the `*2`/HDR shape, like `monitor_query_modes2`).
    let mut targets: Vec<iddcx::IDDCX_TARGET_MODE2> = flatten(&new_modes)
        .map(|item| {
            let mhz = vdisplay::rate_mhz(exact, item.refresh_rate);
            target_mode2(item.width, item.height, mhz)
        })
        .collect();
    let in_args = iddcx::IDARG_IN_UPDATEMODES2 {
        Reason: iddcx::IDDCX_UPDATE_REASON::IDDCX_UPDATE_REASON_OTHER,
        TargetModeCount: targets.len() as u32,
        pTargetModes: targets.as_mut_ptr(),
    };
    // SAFETY: `object` is a live IddCx monitor handle (arrived — checked above) and `_ddi` holds
    // its teardown and departure off until this call returns. `in_args` points at valid local
    // storage (`targets` outlives the synchronous DDI call).
    let st = unsafe { wdk_iddcx::IddCxMonitorUpdateModes2(object, &in_args) };
    dbglog!(
        "[pf-vd] IddCxMonitorUpdateModes2(session={session_id}, {width}x{height}@{refresh}) -> {st:#x}"
    );
    if !wdk_iddcx::nt_success(st) {
        // Keep the stored list coherent with what the OS actually holds (the old one).
        *lock(&m.modes) = old_modes;
        m.exact_mhz.store(old_exact, Ordering::Release);
        return st;
    }
    crate::STATUS_SUCCESS
}

/// The exact rate a request names beside its whole `hz`: `mhz` where it has a fraction and
/// rounds to `hz`, else zero.
fn exact_of(hz: u32, mhz: u32) -> u32 {
    match vdisplay::rate_mhz(mhz, hz) {
        rate if rate % 1000 != 0 => rate,
        _ => 0,
    }
}

/// `IOCTL_REMOVE`: unlink, tear down and depart `owner`'s monitor for `session_id`, recording
/// its mode list for the next same-id create. Returns true if one was removed.
pub fn remove_monitor(owner: u32, session_id: u64) -> bool {
    let Some(m) = registry::remove_session(owner, session_id) else {
        return false;
    };
    depart(vec![m]);
    true
}

/// `IOCTL_CLEAR_ALL`: tear down and depart every monitor `owner` holds. A crashed
/// predecessor's monitors are not the caller's to clear: [`crate::watchdog`] departs those when
/// the dead process's handles close, so a restarted host finds its connectors free already.
pub fn clear_all(owner: u32) {
    depart(registry::remove(|m| m.owner == owner));
}

/// `EvtCleanupCallback` (device removal, [`crate::callbacks::device_cleanup`]): empty the
/// registry and release every monitor's heavy resources — drain workers, cursor workers and
/// encode sessions — WITHOUT `IddCxMonitorDeparture`: the framework tears the IddCx monitors
/// down with the departing device, and departing here would double-tear.
pub fn cleanup_for_device_removal() {
    for m in registry::remove(|_| true) {
        m.teardown();
    }
}

/// Unlink `monitor` itself after a failed create or arrival — by identity: a removal that ran
/// mid-create freed its id, and another owner's ADD may hold it by now. A pending entry
/// normally holds nothing, but a concurrent delivery may have stocked it, so it is torn down.
fn remove_entry(monitor: &Arc<Monitor>) {
    for m in registry::remove(|m| core::ptr::eq(m, Arc::as_ptr(monitor))) {
        m.teardown();
    }
}

/// Rebuild [`vdisplay::container_guid`]'s fields as the OS `GUID` — the container id that groups a
/// monitor's targets into one physical device. `pf_driver_proto` is `no_std` and has no `GUID` type,
/// so it hands back the four fields.
fn container_guid(id: u32) -> wdk_sys::GUID {
    let (d1, d2, d3, d4) = vdisplay::container_guid(id);
    wdk_sys::GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remover that never claims: device cleanup, or no removal at all.
    const NEVER: u8 = 3;

    /// A request's exact rate counts only where it has a fraction and names the whole rate
    /// beside it; every mode at that whole rate then runs it.
    #[test]
    fn a_monitor_runs_the_fraction_its_request_names() {
        assert_eq!(exact_of(60, 59_940), 59_940);
        assert_eq!(exact_of(120, 119_880), 119_880);
        // No fraction, none sent, or one that rounds to another rate.
        for (hz, mhz) in [(60, 60_000), (60, 0), (120, 59_940), (60, 61_000)] {
            assert_eq!(exact_of(hz, mhz), 0, "{hz} {mhz}");
        }
        let m = Monitor::pending(1, 1, 1, false, 59_940, Vec::new());
        assert_eq!(vdisplay::rate_mhz(m.exact_mhz(), 60), 59_940);
        assert_eq!(vdisplay::rate_mhz(m.exact_mhz(), 120), 120_000);
    }

    /// One create racing one remover on a real entry. The remover's claim lands at step
    /// `remover_at`: 0 before the creator's swap, 1 before the creator's claim, 2 after it.
    /// Returns the creator's decision and how many departures ran in all.
    fn race(remover_at: u8, linked: bool) -> (AfterArrival, u32) {
        let m = Monitor::pending(1, 1, 1, false, 0, Vec::new());
        let remover = |step: u8| step == remover_at && m.claim_departure();
        let mut departures = u32::from(remover(0));
        let arrived = m
            .state
            .compare_exchange(CREATING, ARRIVED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        departures += u32::from(remover(1));
        let decision = after_arrival(arrived, linked);
        if let AfterArrival::Depart { claim_first } = decision {
            departures += u32::from(!claim_first || m.claim_departure());
        }
        departures += u32::from(remover(2));
        (decision, departures)
    }

    #[test]
    fn cleared_mid_create_departs_once_without_a_claim() {
        let want = AfterArrival::Depart { claim_first: false };
        assert_eq!(race(0, false), (want, 1));
    }

    #[test]
    fn unlinked_mid_create_departs_once_through_the_claim() {
        let want = AfterArrival::Depart { claim_first: true };
        for remover_at in [1, 2, NEVER] {
            assert_eq!(
                race(remover_at, false),
                (want, 1),
                "remover at {remover_at}"
            );
        }
    }

    #[test]
    fn an_undisturbed_create_keeps_its_monitor() {
        assert_eq!(race(NEVER, true), (AfterArrival::Keep, 0));
    }
}
