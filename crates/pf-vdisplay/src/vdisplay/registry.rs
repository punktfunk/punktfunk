//! Host-lifetime virtual-display registry (`design/display-management.md`).
//!
//! Owns display lifecycle so a display can outlive the session that created it
//! (keep-alive) and the management API can list and release kept displays.
//!
//! Windows: [`super::manager::VirtualDisplayManager`] already leases one IddCx
//! monitor; [`acquire`] is `vd.create`, [`snapshot`]/[`release`] read it.
//!
//! Linux: a per-session pool driven by [`super::lifecycle`]. Capture on the
//! default PipeWire daemon (`remote_fd == None`) stays alive with the keepalive;
//! reconnect re-attaches to the same `node_id`. Hyprland and sway linger the
//! named head and recast ScreenCast by name. A `mode_conflict: join` session holds
//! a live display with its own cast (`VirtualDisplay::join_cast`).
//!
//! [`acquire`] returns a `VirtualOutput` whose `keepalive` is a generation-stamped
//! `DisplayLease`. Dropping it releases the registry refcount; the lifecycle
//! machine decides linger vs teardown.

use anyhow::Result;

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// Generation stamp used as the `/display/release` slot argument.
    pub slot: u64,
    pub backend: String,
    pub mode: (u32, u32, u32),
    /// `"active"` | `"lingering"` | `"pinned"`.
    pub state: String,
    /// Milliseconds until linger teardown. `None` when active or pinned.
    pub expires_in_ms: Option<u64>,
    pub sessions: u32,
    /// Cert-fp prefix / peer, when the owner tracks one.
    pub client: Option<String>,
    /// Shared-desktop group. Linux: one per backend session. Windows: always `1`.
    pub group: u32,
    /// Ordinal within the group, acquire order, 0-based.
    pub display_index: u32,
    /// Desktop-space top-left. Auto-row, or the console's manual arrangement.
    pub position: (i32, i32),
    /// Persistent-config / manual-layout key. `None` = shared/anonymous.
    pub identity_slot: Option<u32>,
    /// `"extend"` | `"primary"` | `"exclusive"`.
    pub topology: String,
}

/// Live display set for `/display/state`.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub displays: Vec<DisplayInfo>,
}

/// Snapshot topology string. `effective_topology` resolves `Auto`; the arm is defensive.
///
/// The HOST's answer, not any one device's: this is the snapshot `/display/state` serves,
/// and a per-device topology belongs on that device's row rather than on the whole list.
pub fn topology_str() -> String {
    use super::policy::Topology;
    match super::effective_topology(None) {
        Topology::Extend => "extend",
        Topology::Primary => "primary",
        Topology::Exclusive => "exclusive",
        Topology::Auto => "auto",
    }
    .to_string()
}

/// Lease a virtual display: reuse a matching kept output or create one. The
/// returned [`VirtualOutput`](super::VirtualOutput) holds the session capture
/// and a registry lease; the registry retains the compositor output.
///
/// Windows calls `vd.create` through [`manager`](super::manager). Linux uses
/// the pool below and splits Hyprland's ScreenCast from its named output.
///
/// `quit` set means a deliberate stop — teardown now, skip linger. A network
/// drop leaves it false.
///
/// `supersedes` is the pool generation this acquire replaces (create-before-drop
/// on a mode switch). Without it the predecessor still counts as a live sibling
/// and Primary/Exclusive becomes Extend. `None` everywhere else.
pub fn acquire(
    vd: &mut Box<dyn super::VirtualDisplay>,
    mode: super::Mode,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    supersedes: Option<u64>,
) -> Result<super::VirtualOutput> {
    let backend = vd.name();
    #[cfg(target_os = "linux")]
    let out = linux::acquire(vd, mode, quit, supersedes, false);
    #[cfg(not(target_os = "linux"))]
    let out = {
        // Windows reads quit off the backend (`VirtualDisplay::set_quit_flag`). Set here too,
        // so a caller that never set it (GameStream's Quit App) still skips linger.
        // Supersede is Linux-pool-only; the manager resizes in place.
        let _ = supersedes;
        vd.set_quit_flag(quit);
        vd.create(mode)
    };
    // `Created` is existence, not reuse. Linux has `reused_gen`; Windows does
    // not, so JOIN/linger reuse still reports Created until the manager
    // surfaces its acquire outcome. Linger expiry has the matching missing
    // `Released` — see `release`.
    #[cfg(target_os = "linux")]
    let created = matches!(&out, Ok(o) if o.reused_gen.is_none());
    #[cfg(not(target_os = "linux"))]
    let created = out.is_ok();
    if created {
        crate::emit_display_event(crate::DisplayEvent::Created {
            backend: backend.to_string(),
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
        });
    }
    out
}

/// Pre-warm a display for a seat and keep it until a session claims it
/// (`design/steam-seats-warm-launch-implementation-plan.md` WP-S2).
///
/// Same create path as [`acquire`], so the entry carries the reuse key a session then asks for.
/// A parked display outlives the operator's `keep_alive`, does not count against `max_displays`
/// at admission, and is the first thing a create at the cap evicts.
///
/// `false` when this seat holds a display a pre-warm must not take — a live session's, another
/// colourimetry or cursor mode, or another mode on a backend that cannot be resized — and
/// nothing was warmed.
#[cfg(target_os = "linux")]
pub fn park(vd: &mut Box<dyn super::VirtualDisplay>, mode: super::Mode) -> Result<bool> {
    let backend = vd.name();
    let (width, height, refresh_hz) = (mode.width, mode.height, mode.refresh_hz);
    let parked = linux::park(vd, mode)?;
    // Only a fresh compositor is `Created`; adopting a kept display was counted at its create.
    if parked == Some(true) {
        crate::emit_display_event(crate::DisplayEvent::Created {
            backend: backend.to_string(),
            width,
            height,
            refresh_hz,
        });
    }
    Ok(parked.is_some())
}

/// Isolation keys of the seats parked right now ([`park`]).
#[cfg(target_os = "linux")]
pub fn parked_isolations() -> Vec<String> {
    linux::parked_isolations()
}

/// Cheap lock-read of the host's managed virtual displays.
pub fn snapshot() -> Snapshot {
    #[cfg(target_os = "windows")]
    {
        // One shared-desktop group. `identity_slot` is None for anonymous slot 0.
        let displays = super::manager::snapshot()
            .into_iter()
            .enumerate()
            .map(|(idx, i)| DisplayInfo {
                slot: i.generation,
                backend: i.backend.to_string(),
                mode: i.mode,
                state: i.state.to_string(),
                expires_in_ms: i.expires_in_ms,
                sessions: i.sessions,
                client: None,
                group: 1,
                display_index: idx as u32,
                position: i.position,
                identity_slot: (i.slot_id != 0).then_some(i.slot_id),
                topology: topology_str(),
            })
            .collect();
        Snapshot { displays }
    }
    #[cfg(target_os = "linux")]
    {
        Snapshot {
            displays: linux::snapshot(),
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Snapshot::default()
    }
}

/// `/display/release`: force-release kept (lingering/pinned) displays. `slot`
/// selects one by [`DisplayInfo::slot`]; `None` releases every kept display.
/// Active displays are refused. Returns the count released.
pub fn release(slot: Option<u64>) -> usize {
    #[cfg(target_os = "windows")]
    let released = super::manager::force_release(slot);
    #[cfg(target_os = "linux")]
    let released = linux::force_release(slot);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let released = {
        let _ = slot;
        0
    };
    // Linux already emits from every pool teardown; emitting here would
    // double-count. Windows has no other hook, so this endpoint is the only
    // `Released` the console sees.
    #[cfg(not(target_os = "linux"))]
    if released > 0 {
        crate::emit_display_event(crate::DisplayEvent::Released {
            count: released as u32,
        });
    }
    released
}

/// Host stopping: run every display's topology restore and drop every output, active ones
/// included. Windows keeps its own manager. Returns the count torn down.
pub fn teardown_all() -> usize {
    #[cfg(target_os = "linux")]
    {
        linux::teardown_all()
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Tear down a reused-but-dead pool entry by generation. The pipeline builder
/// calls this when the first frame fails on a REUSED [`acquire`] so the next
/// acquire creates fresh. No-op off Linux, if already gone (the later
/// stale-generation lease drop no-ops too), or while another session holds it.
pub fn mark_failed(generation: u64) {
    #[cfg(target_os = "linux")]
    linux::mark_failed(generation);
    #[cfg(not(target_os = "linux"))]
    let _ = generation;
}

/// Force-release a superseded kept display by generation
/// (`design/midstream-resolution-resize.md`). A mode-switch lease drop looks
/// like a disconnect, so linger/forever would keep stale-mode monitors. Called
/// once the new pipeline is up. Active entries are refused; already-gone is a
/// no-op. No-op off Linux (Windows resizes in place).
pub fn retire(generation: u64) {
    #[cfg(target_os = "linux")]
    linux::retire(generation);
    #[cfg(not(target_os = "linux"))]
    let _ = generation;
}

/// Seat key of the pooled display with this `pool_gen` ([`crate::VirtualOutput::seat`]).
///
/// The session already carries its pool generation, so this hands the seat to the launch, the
/// exit watch and the cursor source without threading it through the pipeline. `None` for a
/// non-poolable output — discovery then falls back to unscoped, as it was before seats.
#[cfg(target_os = "linux")]
pub fn seat_for(pool_gen: u64) -> Option<String> {
    linux::seat_for(pool_gen)
}

/// The compositor process of the pooled display with this `pool_gen`
/// ([`crate::VirtualOutput::pid`]). A nested launch's whole tree descends from it, so a scan can
/// tell this seat's game from another seat's copy of the same title. `None` where we spawned none.
#[cfg(target_os = "linux")]
pub fn compositor_pid_for(pool_gen: u64) -> Option<u32> {
    linux::compositor_pid_for(pool_gen)
}

/// Reap every kept display of `backend` whose compositor is gone
/// (`design/gamemode-and-dedicated-sessions.md`). Called from the session-switch
/// watcher. No-op off Linux.
pub fn invalidate_backend(backend: &str) {
    #[cfg(target_os = "linux")]
    linux::invalidate_backend(backend);
    #[cfg(not(target_os = "linux"))]
    let _ = backend;
}

/// Identity slots driving a pooled display — eviction guard for
/// [`identity::live_slot_ids`](crate::identity). Lingering/pinned count: the
/// output still exists. Linux-only.
#[cfg(target_os = "linux")]
pub(crate) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
    linux::live_identity_slots()
}

/// Linux displays [`admission`](crate::admission) counts against `max_displays`: live and
/// pinned. A lingering display is left out: [`acquire`] reuses it or evicts it, so it never
/// holds a new session out.
#[cfg(target_os = "linux")]
pub(crate) fn budget_display_count() -> u32 {
    linux::budget_display_count()
}

/// Pure pool rules (entry, group, reuse, expiry, snapshot). No OS API — `mod
/// linux` owns the global and the backend calls — so the sweep is unit-tested
/// on every host this crate builds on.
///
/// Off Linux nothing calls it; the `dead_code` allow is cfg-gated so a truly
/// dead helper still fails on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod pool {
    use std::time::Instant;

    use super::DisplayInfo;
    use crate::lifecycle;
    use crate::policy::{Layout, Linger};
    use crate::Mode;

    /// One pooled display. The backend keepalive lives here so the compositor
    /// output (and its PipeWire `node_id`) outlives the session.
    pub(super) struct Entry {
        pub(super) life: lifecycle::State,
        /// Backend keepalive. Never read: holding it is the behaviour; `Drop`
        /// releases the compositor output. Deleting the field as unused would
        /// tear every pooled display down at create.
        #[allow(dead_code)]
        pub(super) keepalive: Box<dyn Send>,
        pub(super) node_id: u32,
        pub(super) preferred_mode: Option<(u32, u32, u32)>,
        /// Compositor output name ([`VirtualOutput::output_name`]). Kept across
        /// reuse so the reused head answers with the same name as a fresh create.
        pub(super) output_name: Option<String>,
        /// [`VirtualOutput::input_output`], kept across reuse like `output_name`.
        pub(super) input_output: Option<String>,
        /// What a `mode_conflict: join` session casts to share this display
        /// (`VirtualDisplay::join_cast`). Unset until the backend knows it.
        pub(super) join_name: Option<crate::backend::JoinName>,
        pub(super) mode: Mode,
        pub(super) backend: &'static str,
        /// Identity slot at create (`None` = anonymous). Kept across reuse; keys
        /// group arrangement and `/display/state`.
        pub(super) identity_slot: Option<u32>,
        /// Per-group topology restore: re-enable physicals that `exclusive`
        /// disabled. At most one entry per group holds it; teardown hands it to
        /// a sibling, and it runs only when the last member drops.
        pub(super) topology_restore: Option<Restore>,
        /// Isolation identity at create (`design/gamescope-multiuser.md`). Reuse
        /// requires an exact match (EIS path + Pulse sinks baked into env).
        /// `None` = not isolated.
        pub(super) isolation: Option<String>,
        /// Session epoch at create. Reuse requires a match; linger reaps a
        /// stale-epoch entry (compositor replaced under it).
        pub(super) epoch: u64,
        /// `DisplayLease` releases only if this still matches; a stale lease
        /// (reused + re-stamped) is a no-op.
        pub(super) generation: u64,
        /// Cursor mode at create (metadata-pointer vs compositor-embedded).
        /// Reuse requires an exact match or the pointer is missing or unforwardable.
        pub(super) hw_cursor: bool,
        /// Seat key at create ([`crate::VirtualOutput::seat`]). Kept across reuse: the reuse path
        /// is exactly the one that launches into a live compositor, so losing it there is what
        /// would deliver a launch to another seat.
        pub(super) seat: Option<String>,
        /// Our child compositor at create ([`crate::VirtualOutput::pid`]). Reuse requires it
        /// alive. Its unreaped `Child` in `keepalive` pins the pid, so it cannot be recycled.
        pub(super) pid: Option<u32>,
        /// Colourimetry at create (HDR vs SDR). Reuse requires an exact match:
        /// a kept SDR gamescope has no `--hdr-enabled`, and the reverse would
        /// negotiate 8-bit off a PQ composite.
        pub(super) hdr: bool,
        /// Pre-warmed for a seat, with no session yet ([`super::park`]). It outlives the
        /// operator's keep-alive policy, counts as spare capacity rather than a held display,
        /// and is evicted first. Cleared the moment a session claims it.
        pub(super) parked: bool,
    }

    /// Gamescope is the only virtual output that offers HDR. Its teardown ends the compositor
    /// a failed offer latched against, so the next spawn tries HDR again.
    impl Drop for Entry {
        fn drop(&mut self) {
            #[cfg(target_os = "linux")]
            if self.backend == "gamescope" {
                pf_frame::hdr::clear_virtual_output_hdr_latch();
            }
        }
    }

    pub(super) type Restore = Box<dyn FnOnce() + Send>;

    /// Kept generations of `backend` sharing `isolation`: what a sole-instance acquire must
    /// retire before it creates, so the backend never runs two. Active entries are never
    /// selected — a live session keeps its own compositor and this acquire creates beside it.
    pub(super) fn kept_to_retire(
        entries: &[Entry],
        backend: &str,
        isolation: &Option<String>,
    ) -> Vec<u64> {
        entries
            .iter()
            .filter(|e| {
                e.backend == backend
                    && e.isolation == *isolation
                    && matches!(
                        e.life,
                        lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
                    )
            })
            .map(|e| e.generation)
            .collect()
    }

    /// Displays that count against `max_displays` at admission: everything but Lingering and
    /// parked. Both are spare capacity — [`super::linux::acquire`] reuses one or evicts it, so
    /// neither holds a new session out.
    pub(super) fn budget_count(entries: &[Entry]) -> u32 {
        entries
            .iter()
            .filter(|e| !e.parked && !matches!(e.life, lifecycle::State::Lingering { .. }))
            .count() as u32
    }

    /// Kept display a create evicts so the pool stays within `max`: a parked seat first — it has
    /// no session behind it — else the lingering one nearest expiry. `None` below the cap.
    /// `supersedes` does not count; its successor replaces it.
    pub(super) fn kept_to_evict(
        entries: &[Entry],
        max: u32,
        supersedes: Option<u64>,
    ) -> Option<u64> {
        let counted = entries
            .iter()
            .filter(|e| Some(e.generation) != supersedes)
            .count() as u32;
        if counted < max {
            return None;
        }
        let kept = |e: &Entry| {
            matches!(
                e.life,
                lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
            )
        };
        entries
            .iter()
            .find(|e| e.parked && kept(e))
            .map(|e| e.generation)
            .or_else(|| {
                entries
                    .iter()
                    .filter_map(|e| match e.life {
                        lifecycle::State::Lingering { until } => Some((until, e.generation)),
                        _ => None,
                    })
                    .min()
                    .map(|(_, generation)| generation)
            })
    }

    /// The create slot a display's seat owns: one create at a time per backend and isolation.
    /// `None` for a shared-plane backend, which has no seat to collide on and never waits.
    pub(super) fn slot_key(backend: &str, isolation: &Option<String>) -> Option<String> {
        Some(format!("{backend}#{}", isolation.as_deref()?))
    }

    /// What a create does about its [`slot_key`] ([`super::linux::create_slot`]).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Slot {
        /// Free, or not a seat at all: create now.
        Free,
        /// This thread holds it already. A pre-warm takes the slot across its own seat check and
        /// the acquire beneath it is that same create, not a second one.
        Mine,
        /// Another thread is standing this seat up: wait, then reuse what it publishes.
        Wait,
    }

    pub(super) fn slot_state(
        busy: &[(String, std::thread::ThreadId)],
        key: &str,
        me: std::thread::ThreadId,
    ) -> Slot {
        match busy.iter().find(|(k, _)| k == key) {
            Some((_, holder)) if *holder == me => Slot::Mine,
            Some(_) => Slot::Wait,
            None => Slot::Free,
        }
    }

    /// Live display a `mode_conflict: join` session shares: Active, same backend, mode,
    /// isolation, colourimetry and epoch, and its join name known. The first match is the
    /// oldest, which is the session admission named. The cursor mode rides each session's
    /// own cast, so it is not a key.
    pub(super) fn join_target(
        entries: &[Entry],
        backend: &str,
        mode: Mode,
        isolation: &Option<String>,
        hdr: bool,
        cur_epoch: u64,
    ) -> Option<usize> {
        entries.iter().position(|e| {
            matches!(e.life, lifecycle::State::Active { .. })
                && e.backend == backend
                && e.mode == mode
                && e.isolation == *isolation
                && e.hdr == hdr
                && epoch_matches(e.backend, e.epoch, cur_epoch)
                && e.join_name.as_ref().is_some_and(|n| n.get().is_some())
        })
    }

    /// Display group: one per desktop compositor backend. Each gamescope spawn
    /// is its own group — never auto-rowed or restore-grouped with another.
    pub(super) fn group_key(backend: &str, generation: u64) -> String {
        if backend == "gamescope" {
            format!("gamescope#{generation}")
        } else {
            backend.to_string()
        }
    }

    /// Group membership for restore hand-off, first-in-group, and layout.
    ///
    /// `supersedes` is the display a mode switch is replacing (create-before-drop).
    /// It is still Active but leaving; counting it made the newcomer defer
    /// topology and auto-row one width to the right on every resize.
    ///
    /// Lifecycle is ignored: a lingering/pinned entry still occupies desktop
    /// space. Only first-in-group adds a liveness term (live sessions, not outputs).
    pub(super) fn in_group(
        e_backend: &str,
        e_gen: u64,
        backend: &str,
        generation: u64,
        supersedes: Option<u64>,
    ) -> bool {
        Some(e_gen) != supersedes && group_key(e_backend, e_gen) == group_key(backend, generation)
    }

    /// Move a departing display's topology restore onto a same-[group](in_group)
    /// sibling, or return it if the group is empty so the caller runs it
    /// before dropping the keepalive (the compositor must not see zero outputs).
    ///
    /// Both `backend` and `generation` identify the departing display: keyed on
    /// backend alone, a gamescope restore floated onto another client's spawn.
    pub(super) fn hand_off_restore(
        remaining: &mut [Entry],
        backend: &'static str,
        generation: u64,
        restore: Option<Restore>,
    ) -> Option<Restore> {
        let action = restore?;
        // At most one restore per group, so any surviving sibling has `None`.
        match remaining
            .iter_mut()
            .find(|e| in_group(e.backend, e.generation, backend, generation, None))
        {
            Some(sibling) => {
                sibling.topology_restore = Some(action);
                None
            }
            None => Some(action), // group empty: caller runs it now
        }
    }

    /// Whether this entry's session epoch still matches for reuse/expiry.
    /// Epoch tracks the desktop compositor; a stale-epoch kept output is a
    /// corpse. Gamescope is exempt: its node lives with its child, unrelated
    /// to the desktop compositor. Liveness is `kept_display_alive` + `mark_failed`.
    pub(super) fn epoch_matches(backend: &str, entry_epoch: u64, cur_epoch: u64) -> bool {
        backend == "gamescope" || entry_epoch == cur_epoch
    }

    /// Keys a kept display must match for an acquire to take it, beside its lifecycle and
    /// whatever the backend itself says. `resizable` drops the mode from the set: a display the
    /// host can move to another mode under a live compositor is not *at* a mode, it is *set to*
    /// one. Colourimetry and cursor mode stay keys — both are baked before the display exists.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reuse_keys_match(
        e: &Entry,
        backend: &str,
        mode: Mode,
        isolation: &Option<String>,
        hw_cursor: bool,
        hdr: bool,
        cur_epoch: u64,
        resizable: bool,
    ) -> bool {
        e.backend == backend
            && (e.mode == mode || resizable)
            && e.isolation == *isolation
            && e.hw_cursor == hw_cursor
            && e.hdr == hdr
            && epoch_matches(e.backend, e.epoch, cur_epoch)
    }

    /// What an acquire does with the kept display it picked.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Kept {
        Reuse,
        /// Sound, just not at this mode: leave it kept and let the create below retire it.
        Spawn,
        /// Its compositor is gone. Drop it and hand its topology restore on.
        Dead,
    }

    /// `resized` is the compositor's own answer to a mode it was asked to take; a refusal or a
    /// timeout is `false`, and then this is the retire-and-spawn a mode mismatch always got.
    pub(super) fn kept_verdict(alive: bool, resized: bool) -> Kept {
        match (alive, resized) {
            (false, _) => Kept::Dead,
            (true, false) => Kept::Spawn,
            (true, true) => Kept::Reuse,
        }
    }

    /// One display a seat already holds, as a pre-warm reads it: live session, (HDR, hardware
    /// cursor), mode.
    pub(super) type Held = (bool, (bool, bool), Mode);

    /// The mode a pre-warm parks this seat at, or `None` to leave the seat alone.
    ///
    /// A live session's compositor is never taken: a second spawn fights its socket lock and its
    /// Steam. Another colourimetry or cursor mode would be retired, trading a warm Steam for a
    /// colder one. A kept one at another mode is the seat's warm Steam already once the mode can
    /// be changed under it, so it is adopted where it stands instead.
    pub(super) fn park_mode_for(
        held: &[Held],
        shape: (bool, bool),
        mode: Mode,
        resizable: bool,
    ) -> Option<Mode> {
        if held
            .iter()
            .any(|(live, kept_shape, _)| *live || *kept_shape != shape)
        {
            return None;
        }
        match held.iter().find(|(_, _, kept)| *kept != mode) {
            Some(_) if !resizable => None,
            Some((_, _, kept)) => Some(*kept),
            None => Some(mode),
        }
    }

    /// Entries taken out of the pool, and the topology restores of the groups they emptied.
    /// The caller runs the restores, then drops the entries, both outside the pool lock.
    pub(super) struct Drained {
        pub(super) entries: Vec<Entry>,
        pub(super) restores: Vec<Restore>,
    }

    /// Remove every entry `pred` selects. Each removal [hands its restore
    /// off](hand_off_restore) to a sibling still in the pool, so this stays one in-place pass:
    /// the heir is picked from what remains at that removal.
    pub(super) fn drain_where(
        entries: &mut Vec<Entry>,
        mut pred: impl FnMut(&mut Entry) -> bool,
    ) -> Drained {
        let mut out = Drained {
            entries: Vec::new(),
            restores: Vec::new(),
        };
        let mut i = 0;
        while i < entries.len() {
            if pred(&mut entries[i]) {
                let mut e = entries.remove(i);
                let (backend, generation) = (e.backend, e.generation);
                if let Some(r) =
                    hand_off_restore(entries, backend, generation, e.topology_restore.take())
                {
                    out.restores.push(r);
                }
                out.entries.push(e);
            } else {
                i += 1;
            }
        }
        out
    }

    /// Take entries past their linger deadline, and kept (non-Active) desktop displays whose
    /// epoch is stale: the compositor was replaced, so the node id is a corpse. Gamescope is
    /// exempt (`epoch_matches`); Active stays for its session's rebuild.
    pub(super) fn take_expired(entries: &mut Vec<Entry>, now: Instant, cur_epoch: u64) -> Drained {
        drain_where(entries, |e| {
            let dead_epoch = !epoch_matches(e.backend, e.epoch, cur_epoch)
                && !matches!(e.life, lifecycle::State::Active { .. });
            e.life.poll_expiry(now) || dead_epoch
        })
    }

    /// Linger a release applies. A parked seat has no owner yet, so it outlives a `keep_alive`
    /// that may be Off — the budget, `/display/release`, a dead compositor or the host stopping
    /// are what end it.
    pub(super) fn release_linger(parked: bool, force_immediate: bool, policy: Linger) -> Linger {
        if parked {
            Linger::Forever
        } else {
            lifecycle::effective_linger(force_immediate, policy)
        }
    }

    /// Flattened live/kept row so group/layout math runs outside the pool lock.
    pub(super) struct Row {
        pub(super) generation: u64,
        pub(super) backend: &'static str,
        pub(super) mode: Mode,
        pub(super) identity_slot: Option<u32>,
        pub(super) state: &'static str,
        pub(super) expires_in_ms: Option<u64>,
        pub(super) sessions: u32,
    }

    /// Desktop position for a display just appended to its group: existing
    /// members plus `new`, ordered by acquire `generation`, arranged by
    /// [`layout`](crate::layout). Pure so tests do not need the pool lock.
    pub(super) fn position_for_new(
        mut existing: Vec<(u64, crate::layout::Member)>,
        new: crate::layout::Member,
        layout_policy: &Layout,
    ) -> crate::layout::Placement {
        existing.sort_by_key(|(g, _)| *g);
        let mut members: Vec<crate::layout::Member> =
            existing.into_iter().map(|(_, m)| m).collect();
        members.push(new);
        *crate::layout::arrange(&members, layout_policy)
            .last()
            .expect("members is non-empty (just pushed `new`)")
    }

    /// Assign stable group ids: known keys keep their id, new ones take `next`,
    /// gone keys are dropped. Do not use sorted-list index — a `gamescope#N`
    /// key sorts ahead of `"kwin"` and would renumber the desktop. Prune live
    /// keys or per-spawn ids accumulate for the process lifetime.
    pub(super) fn assign_group_ids(
        known: &mut std::collections::BTreeMap<String, u32>,
        next: &mut u32,
        keys: &[String],
    ) {
        known.retain(|k, _| keys.iter().any(|live| live == k));
        for k in keys {
            if !known.contains_key(k) {
                known.insert(k.clone(), *next);
                *next += 1;
            }
        }
    }

    /// `/display/state` view: rows grouped by [`group_key`], ordered by acquire
    /// `generation`, positions from [`layout`](crate::layout). Pure for tests.
    pub(super) fn assemble_displays(
        rows: Vec<Row>,
        layout_policy: &Layout,
        topology: &str,
        ids: &std::collections::BTreeMap<String, u32>,
    ) -> Vec<DisplayInfo> {
        use crate::layout::{self, Member};

        let mut keys: Vec<String> = rows
            .iter()
            .map(|r| group_key(r.backend, r.generation))
            .collect();
        keys.sort();
        keys.dedup();

        let mut out: Vec<DisplayInfo> = Vec::new();
        for key in keys.iter() {
            let mut idx: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| &group_key(row.backend, row.generation) == key)
                .map(|(i, _)| i)
                .collect();
            idx.sort_by_key(|&i| rows[i].generation);
            let members: Vec<Member> = idx
                .iter()
                .map(|&i| Member {
                    identity_slot: rows[i].identity_slot,
                    width: rows[i].mode.width as i32,
                })
                .collect();
            let places = layout::arrange(&members, layout_policy);
            for (ord, &i) in idx.iter().enumerate() {
                let row = &rows[i];
                let p = places[ord];
                out.push(DisplayInfo {
                    slot: row.generation,
                    backend: row.backend.to_string(),
                    mode: (row.mode.width, row.mode.height, row.mode.refresh_hz),
                    state: row.state.to_string(),
                    expires_in_ms: row.expires_in_ms,
                    sessions: row.sessions,
                    client: None,
                    // 0 = ungrouped. The caller derives `ids` from these rows;
                    // panicking a mgmt read would be worse than a missing key.
                    group: ids.get(key).copied().unwrap_or(0),
                    display_index: ord as u32,
                    position: (p.x, p.y),
                    identity_slot: row.identity_slot,
                    topology: topology.to_string(),
                });
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::policy::{Layout, LayoutMode, Position};
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// Dummy keepalive; `hand_off_restore` only reads backend, generation, restore.
        fn test_entry(backend: &'static str, generation: u64, restore: Option<Restore>) -> Entry {
            Entry {
                life: lifecycle::State::default(),
                keepalive: Box::new(()),
                node_id: 0,
                preferred_mode: None,
                output_name: None,
                input_output: None,
                join_name: None,
                mode: Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                },
                backend,
                identity_slot: None,
                topology_restore: restore,
                isolation: None,
                seat: None,
                pid: None,
                epoch: 0,
                generation,
                hw_cursor: false,
                hdr: false,
                parked: false,
            }
        }

        fn flag_restore(flag: &Arc<AtomicBool>) -> Restore {
            let f = flag.clone();
            Box::new(move || f.store(true, Ordering::SeqCst))
        }

        /// Snapshot-style group ids from an empty map so tests do not share state.
        fn ids_for(rows: &[Row]) -> BTreeMap<String, u32> {
            let mut known = BTreeMap::new();
            let mut next = 1;
            ids_into(&mut known, &mut next, rows);
            known
        }

        /// Like `ids_for` but against a carried map (stability across two assemblies).
        fn ids_into(known: &mut BTreeMap<String, u32>, next: &mut u32, rows: &[Row]) {
            let mut keys: Vec<String> = rows
                .iter()
                .map(|r| group_key(r.backend, r.generation))
                .collect();
            keys.sort();
            keys.dedup();
            assign_group_ids(known, next, &keys);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn tearing_down_gamescope_rearms_its_hdr_but_not_the_portal_latch() {
            use pf_frame::hdr::{hdr_capture_failed, note_hdr_capture_failed, HdrSource};
            note_hdr_capture_failed(HdrSource::VirtualOutput);
            note_hdr_capture_failed(HdrSource::PortalMonitor);
            drop(test_entry("gamescope", 1, None));
            assert!(!hdr_capture_failed(HdrSource::VirtualOutput));
            assert!(hdr_capture_failed(HdrSource::PortalMonitor));
        }

        /// A parked seat survives `keep_alive: off`, which is what every other display on that
        /// host tears down on. Once a session claims it the policy applies again.
        #[test]
        fn a_parked_seat_outlives_a_keep_alive_that_is_off() {
            for quit in [false, true] {
                assert_eq!(
                    release_linger(true, quit, Linger::Immediate),
                    Linger::Forever
                );
            }
            assert_eq!(
                release_linger(false, false, Linger::Immediate),
                Linger::Immediate
            );
        }

        #[test]
        fn join_shares_only_a_live_named_display_at_the_same_mode() {
            let m = Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            };
            let live = |generation, name: Option<&str>| {
                let mut e = test_entry("hyprland", generation, None);
                e.life.acquire();
                e.join_name = name.map(|n| Arc::new(std::sync::OnceLock::from(n.to_string())));
                e
            };
            let mut kept = live(1, Some("PF-1-1"));
            kept.life.release(Instant::now(), Linger::Forever);
            // Mutter names its monitor after `create` returns.
            let mut naming = live(3, None);
            naming.join_name = Some(Arc::new(std::sync::OnceLock::new()));
            let pool = vec![
                kept,
                live(2, None),
                naming,
                live(4, Some("PF-1-4")),
                live(5, Some("PF-1-5")),
            ];
            assert_eq!(join_target(&pool, "hyprland", m, &None, false, 0), Some(3));
            pool[2]
                .join_name
                .as_ref()
                .unwrap()
                .set("Meta-0".into())
                .unwrap();
            assert_eq!(join_target(&pool, "hyprland", m, &None, false, 0), Some(2));
            let other = Mode { width: 1280, ..m };
            assert_eq!(join_target(&pool, "hyprland", other, &None, false, 0), None);
            let iso = Some("seat-2".to_string());
            assert_eq!(join_target(&pool, "hyprland", m, &iso, false, 0), None);
            assert_eq!(join_target(&pool, "hyprland", m, &None, true, 0), None);
            assert_eq!(join_target(&pool, "hyprland", m, &None, false, 1), None);
            assert_eq!(join_target(&pool, "kwin", m, &None, false, 0), None);
        }

        #[test]
        fn topology_restore_floats_to_a_sibling_then_runs_on_the_last_teardown() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![
                test_entry("kwin", 1, Some(flag_restore(&ran))),
                test_entry("kwin", 2, None),
            ];

            let mut e1 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take());
            assert!(out.is_none(), "transferred, not run");
            assert!(!ran.load(Ordering::SeqCst));
            assert!(pool[0].topology_restore.is_some());

            let mut e2 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take());
            let action = out.expect("group empty → run the restore");
            assert!(!ran.load(Ordering::SeqCst), "not run yet");
            action();
            assert!(ran.load(Ordering::SeqCst), "runs on the last drop");
        }

        #[test]
        fn single_session_topology_restore_runs_on_its_own_teardown() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("kwin", 1, Some(flag_restore(&ran)))];
            let mut e = pool.remove(0);
            let action = hand_off_restore(&mut pool, "kwin", 1, e.topology_restore.take())
                .expect("last (only) member → run");
            action();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn tearing_down_a_non_carrier_first_leaves_the_restore_for_last() {
            let ran = Arc::new(AtomicBool::new(false));
            // Gen 1 has no restore: a later exclusive session found physicals already off.
            let mut pool = vec![
                test_entry("kwin", 1, None),
                test_entry("kwin", 2, Some(flag_restore(&ran))),
            ];
            let mut e1 = pool.remove(0);
            assert!(hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take()).is_none());
            assert!(pool[0].topology_restore.is_some());
            let mut e2 = pool.remove(0);
            hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take())
                .expect("last member → run")();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn restore_never_floats_across_backends() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("mutter", 2, None)];
            let out = hand_off_restore(&mut pool, "kwin", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "no same-backend sibling → return to run");
            assert!(
                pool[0].topology_restore.is_none(),
                "restore must not cross into another backend's group"
            );
        }

        /// S1: a kept spawn is retired so the acquire that follows never runs a second one.
        /// Two live gamescopes lose the `gamescope-N` lock, and Steam then hands the URL to the
        /// older instance and exits, killing the new spawn's primary child.
        #[test]
        fn a_sole_instance_acquire_retires_every_kept_sibling() {
            let mut pinned = test_entry("gamescope", 1, None);
            pinned.life = lifecycle::State::Pinned;
            let mut lingering = test_entry("gamescope", 2, None);
            lingering.life = lifecycle::State::Lingering {
                until: Instant::now() + std::time::Duration::from_secs(60),
            };
            let pool = vec![pinned, lingering, test_entry("kwin", 3, None)];
            assert_eq!(kept_to_retire(&pool, "gamescope", &None), vec![1, 2]);
        }

        /// An Active gamescope belongs to a live session — never retired under it.
        #[test]
        fn a_sole_instance_acquire_never_retires_a_live_session() {
            let mut active = test_entry("gamescope", 1, None);
            active.life.acquire();
            let pool = vec![active];
            assert!(kept_to_retire(&pool, "gamescope", &None).is_empty());
        }

        /// A lingering display never holds a session out: admission skips it, and a create at
        /// the cap evicts the one nearest expiry. Live and pinned displays are never evicted.
        #[test]
        fn a_create_at_the_cap_evicts_the_lingering_display_nearest_expiry() {
            use std::time::Duration;
            let now = Instant::now();
            let mut live = test_entry("hyprland", 1, None);
            live.life.acquire();
            let mut late = test_entry("hyprland", 2, None);
            late.life = lifecycle::State::Lingering {
                until: now + Duration::from_secs(60),
            };
            let mut soon = test_entry("hyprland", 3, None);
            soon.life = lifecycle::State::Lingering {
                until: now + Duration::from_secs(5),
            };
            let pool = vec![live, late, soon];
            assert_eq!(budget_count(&pool), 1);
            assert_eq!(kept_to_evict(&pool, 4, None), None);
            assert_eq!(kept_to_evict(&pool, 3, None), Some(3));
            // A mode switch replacing gen 1 does not push the pool to the cap.
            assert_eq!(kept_to_evict(&pool, 3, Some(1)), None);

            let mut pinned = test_entry("hyprland", 4, None);
            pinned.life = lifecycle::State::Pinned;
            let full = vec![pool.into_iter().next().unwrap(), pinned];
            assert_eq!(budget_count(&full), 2);
            assert_eq!(kept_to_evict(&full, 2, None), None);
        }

        /// A create waits out another thread's on the same seat — the pre-warm and that device's
        /// own connect, which would otherwise leave two Steams under one home. Its own thread is
        /// never waited on (the pre-warm re-enters through `acquire`), a different seat is never
        /// delayed, and a shared-plane backend is not keyed at all.
        #[test]
        fn a_create_waits_only_for_another_thread_on_the_same_seat() {
            let cafe = slot_key("gamescope", &Some("cafe0123@/seats/cafe0123".into()))
                .expect("a seat is keyed");
            let beef =
                slot_key("gamescope", &Some("beef4567@/seats/beef4567".into())).expect("keyed");
            assert_ne!(cafe, beef);
            assert_eq!(slot_key("kwin", &None), None, "shared planes never wait");

            let me = std::thread::current().id();
            // `me` is still running, so its id cannot have been handed to that thread.
            let other = std::thread::spawn(|| std::thread::current().id())
                .join()
                .expect("the probe thread");
            let busy = vec![(cafe.clone(), other)];
            assert_eq!(slot_state(&busy, &cafe, me), Slot::Wait);
            assert_eq!(slot_state(&busy, &beef, me), Slot::Free);
            assert_eq!(slot_state(&[], &cafe, me), Slot::Free);
            assert_eq!(slot_state(&[(cafe.clone(), me)], &cafe, me), Slot::Mine);
        }

        /// A parked seat is spare capacity: it holds no session out at admission, and a create
        /// at the cap takes it before any lingering display.
        #[test]
        fn a_create_at_the_cap_evicts_a_parked_seat_first() {
            use std::time::Duration;
            let mut live = test_entry("gamescope", 1, None);
            live.life.acquire();
            let mut lingering = test_entry("gamescope", 2, None);
            lingering.life = lifecycle::State::Lingering {
                until: Instant::now() + Duration::from_secs(5),
            };
            let mut parked = test_entry("gamescope", 3, None);
            parked.life = lifecycle::State::Pinned;
            parked.parked = true;
            let pool = vec![live, lingering, parked];
            assert_eq!(budget_count(&pool), 1, "only the live session is held");
            assert_eq!(kept_to_evict(&pool, 3, None), Some(3));
            // Parked but not yet released by its own acquire: the force-release would refuse
            // it, so the lingering display is what a create at the cap can actually take.
            let mut warming = test_entry("gamescope", 4, None);
            warming.life.acquire();
            warming.parked = true;
            let pool = vec![
                pool.into_iter().nth(1).expect("the lingering entry"),
                warming,
            ];
            assert_eq!(kept_to_evict(&pool, 2, None), Some(2));
        }

        /// The whole of what runtime resize changes about reuse: the mode stops being a key.
        /// Everything baked before the display exists still retires a kept display.
        #[test]
        fn only_the_mode_stops_being_a_reuse_key_when_a_display_can_be_resized() {
            let want = Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            };
            let other = Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            };
            let mut e = test_entry("gamescope", 1, None);
            e.mode = other;
            e.isolation = Some("seat-a".into());
            let keys = |e: &Entry, mode, resizable| {
                reuse_keys_match(
                    e,
                    "gamescope",
                    mode,
                    &Some("seat-a".into()),
                    false,
                    false,
                    0,
                    resizable,
                )
            };
            assert!(
                !keys(&e, want, false),
                "today: another mode is another display"
            );
            assert!(keys(&e, want, true), "resize: the mode is set, not fixed");
            assert!(
                keys(&e, other, false),
                "the same mode never needed a resize"
            );

            for spoil in [
                |e: &mut Entry| e.hdr = true,
                |e: &mut Entry| e.hw_cursor = true,
                |e: &mut Entry| e.isolation = Some("seat-b".into()),
            ] {
                let mut wrong = test_entry("gamescope", 1, None);
                wrong.mode = other;
                wrong.isolation = Some("seat-a".into());
                spoil(&mut wrong);
                assert!(
                    !keys(&wrong, want, true) && !keys(&wrong, other, true),
                    "resize moves a display, it does not re-plan one"
                );
            }
        }

        /// The compositor's own answer decides. A refusal or a timeout is the retire-and-spawn a
        /// mode mismatch always got; only a dead one is dropped where it stands.
        #[test]
        fn a_kept_display_that_will_not_take_the_mode_is_spawned_past_not_buried() {
            assert_eq!(kept_verdict(true, true), Kept::Reuse);
            assert_eq!(kept_verdict(true, false), Kept::Spawn);
            assert_eq!(kept_verdict(false, true), Kept::Dead);
            assert_eq!(kept_verdict(false, false), Kept::Dead);
        }

        /// A pre-warm parks at the record's mode, adopts a kept seat where it stands once the
        /// mode can be changed under it, and never touches a live session or another shape.
        #[test]
        fn a_pre_warm_adopts_a_kept_seat_only_where_the_mode_can_still_change() {
            let record = Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            };
            let kept = Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 120,
            };
            let sdr = (false, false);
            assert_eq!(park_mode_for(&[], sdr, record, false), Some(record));
            assert_eq!(park_mode_for(&[], sdr, record, true), Some(record));
            assert_eq!(
                park_mode_for(&[(false, sdr, kept)], sdr, record, true),
                Some(kept),
                "its Steam is up: adopt it, and let the connect move the mode"
            );
            assert_eq!(
                park_mode_for(&[(false, sdr, kept)], sdr, record, false),
                None,
                "without resize this mode would retire it — a warm Steam for a colder one"
            );
            assert_eq!(
                park_mode_for(&[(true, sdr, record)], sdr, record, true),
                None,
                "a second spawn fights the live session's socket lock and Steam"
            );
            assert_eq!(
                park_mode_for(&[(false, (true, false), record)], sdr, record, true),
                None,
                "colourimetry is baked at spawn; resize does not reach it"
            );
        }

        /// Isolated multi-user spawns are deliberately concurrent: the singleton is per
        /// isolation identity, not per host.
        #[test]
        fn a_sole_instance_acquire_leaves_another_isolation_alone() {
            let mut theirs = test_entry("gamescope", 1, None);
            theirs.life = lifecycle::State::Pinned;
            theirs.isolation = Some("seat-b".into());
            let mut mine = test_entry("gamescope", 2, None);
            mine.life = lifecycle::State::Pinned;
            mine.isolation = Some("seat-a".into());
            let pool = vec![theirs, mine];
            assert_eq!(
                kept_to_retire(&pool, "gamescope", &Some("seat-a".into())),
                vec![2]
            );
        }

        #[test]
        fn restore_never_floats_between_gamescope_spawns() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("gamescope", 2, None)];
            let out = hand_off_restore(&mut pool, "gamescope", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "another client's spawn is not a sibling");
            assert!(pool[0].topology_restore.is_none());
        }

        #[test]
        fn group_membership_splits_spawns_and_excludes_the_superseded() {
            assert!(in_group("kwin", 1, "kwin", 2, None));
            assert!(!in_group("mutter", 1, "kwin", 2, None));
            assert!(!in_group("gamescope", 1, "gamescope", 2, None));
            assert!(in_group("gamescope", 7, "gamescope", 7, None));
            assert!(!in_group("kwin", 1, "kwin", 2, Some(1)));
            assert!(in_group("kwin", 3, "kwin", 2, Some(1)));
        }

        fn row(generation: u64, backend: &'static str, w: u32, slot: Option<u32>) -> Row {
            Row {
                generation,
                backend,
                mode: Mode {
                    width: w,
                    height: 1080,
                    refresh_hz: 60,
                },
                identity_slot: slot,
                state: "active",
                expires_in_ms: None,
                sessions: 1,
            }
        }

        #[test]
        fn groups_by_backend_and_auto_rows_in_acquire_order() {
            // Acquired gen 5 then 2 (vec order is not acquire order) plus one Mutter.
            let rows = vec![
                row(5, "kwin", 2560, Some(1)),
                row(2, "kwin", 1920, Some(7)),
                row(9, "mutter", 3840, None),
            ];
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "exclusive", &ids);

            let kwin: Vec<&DisplayInfo> = out.iter().filter(|d| d.backend == "kwin").collect();
            assert_eq!(kwin.len(), 2);
            assert_eq!(kwin[0].slot, 2);
            assert_eq!(kwin[0].display_index, 0);
            assert_eq!(kwin[0].position, (0, 0));
            assert_eq!(kwin[1].slot, 5);
            assert_eq!(kwin[1].display_index, 1);
            assert_eq!(kwin[1].position, (1920, 0));
            assert_eq!(kwin[0].topology, "exclusive");

            let mutter = out.iter().find(|d| d.backend == "mutter").unwrap();
            assert_ne!(mutter.group, kwin[0].group);
            assert_eq!(mutter.display_index, 0);
            assert_eq!(mutter.position, (0, 0));
        }

        /// `gamescope#3` sorts before `"kwin"`; that must not renumber the desktop.
        #[test]
        fn a_new_group_never_renumbers_an_existing_one() {
            let mut known = BTreeMap::new();
            let mut next = 1;
            let desktop = vec![row(1, "kwin", 1920, None)];
            ids_into(&mut known, &mut next, &desktop);
            let before = assemble_displays(desktop, &Layout::default(), "extend", &known);
            let kwin_group = before[0].group;

            let both = vec![row(1, "kwin", 1920, None), row(3, "gamescope", 1280, None)];
            ids_into(&mut known, &mut next, &both);
            let after = assemble_displays(both, &Layout::default(), "extend", &known);
            let kwin_after = after.iter().find(|d| d.backend == "kwin").unwrap();
            let gs = after.iter().find(|d| d.backend == "gamescope").unwrap();
            assert_eq!(
                kwin_after.group, kwin_group,
                "the untouched desktop keeps its group id"
            );
            assert_ne!(gs.group, kwin_group);
        }

        #[test]
        fn position_for_new_appends_right_in_acquire_order() {
            use crate::layout::{Member, Placement};
            let m = |slot, w| Member {
                identity_slot: slot,
                width: w,
            };
            // Gen 8 @ 1920 acquired after gen 3 @ 2560 (vec is not acquire order).
            let existing = vec![(8, m(Some(2), 1920)), (3, m(Some(1), 2560))];
            let pos = position_for_new(existing, m(Some(5), 1280), &Layout::default());
            assert_eq!(pos, Placement { x: 4480, y: 0 });
            // Origin so the registry skips apply_position.
            let first = position_for_new(vec![], m(None, 3840), &Layout::default());
            assert_eq!(first, Placement { x: 0, y: 0 });
        }

        #[test]
        fn position_for_new_honors_a_manual_pin() {
            use crate::layout::{Member, Placement};
            let mut positions = BTreeMap::new();
            positions.insert("5".to_string(), Position { x: 100, y: 200 });
            let layout = Layout {
                mode: LayoutMode::Manual,
                positions,
            };
            let new = Member {
                identity_slot: Some(5),
                width: 1280,
            };
            let pos = position_for_new(vec![(1, new)], new, &layout);
            assert_eq!(pos, Placement { x: 100, y: 200 });
        }

        #[test]
        fn gamescope_spawns_are_separate_groups() {
            let rows = vec![
                row(1, "gamescope", 1920, None),
                row(2, "gamescope", 1280, None),
            ];
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "extend", &ids);
            assert_eq!(out.len(), 2);
            assert_ne!(out[0].group, out[1].group, "distinct groups");
            assert_eq!(out[0].display_index, 0);
            assert_eq!(out[1].display_index, 0);
            assert_eq!(out[0].position, (0, 0));
            assert_eq!(out[1].position, (0, 0));
        }

        #[test]
        fn manual_layout_keys_positions_by_identity_slot() {
            // Slot 7 left of slot 1 — reversed vs auto-row.
            let rows = vec![row(1, "kwin", 2560, Some(1)), row(2, "kwin", 1920, Some(7))];
            let mut positions = BTreeMap::new();
            positions.insert("1".to_string(), Position { x: 1920, y: 0 });
            positions.insert("7".to_string(), Position { x: 0, y: 0 });
            let layout = Layout {
                mode: LayoutMode::Manual,
                positions,
            };
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &layout, "extend", &ids);
            let by_slot = |s: u32| out.iter().find(|d| d.identity_slot == Some(s)).unwrap();
            assert_eq!(by_slot(1).position, (1920, 0));
            assert_eq!(by_slot(7).position, (0, 0));
        }

        /// Expiry is pure over `(entries, now, epoch)` so stale-epoch and Active exemption pin here.
        #[test]
        fn expiry_reaps_deadlines_and_stale_epoch_corpses_but_never_an_active_entry() {
            use std::time::Duration;
            let t0 = Instant::now();
            let mut es = Vec::new();
            let mut e1 = test_entry("kwin", 1, None);
            e1.life = lifecycle::State::Lingering {
                until: t0 - Duration::from_millis(1),
            };
            es.push(e1);
            let mut e2 = test_entry("kwin", 2, None);
            e2.life = lifecycle::State::Lingering {
                until: t0 + Duration::from_secs(60),
            };
            e2.epoch = 5;
            es.push(e2);
            let mut e3 = test_entry("kwin", 3, None);
            e3.life = lifecycle::State::Pinned;
            e3.epoch = 4;
            es.push(e3);
            let mut e4 = test_entry("kwin", 4, None);
            e4.life = lifecycle::State::Active { refs: 1 };
            e4.epoch = 4;
            es.push(e4);
            let mut e5 = test_entry("gamescope", 5, None);
            e5.life = lifecycle::State::Pinned;
            e5.epoch = 1;
            es.push(e5);

            let drained = take_expired(&mut es, t0, 5);
            assert!(drained.restores.is_empty());
            let gone: Vec<u64> = drained.entries.iter().map(|e| e.generation).collect();
            assert_eq!(gone, vec![1, 3]);
            let left: Vec<u64> = es.iter().map(|e| e.generation).collect();
            assert_eq!(left, vec![2, 4, 5]);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use super::pool::{
        assemble_displays, assign_group_ids, budget_count, drain_where, group_key,
        hand_off_restore, in_group, join_target, kept_to_evict, kept_to_retire, kept_verdict,
        park_mode_for, position_for_new, release_linger, reuse_keys_match, slot_key, slot_state,
        take_expired, Drained, Entry, Held, Kept, Row, Slot,
    };
    use super::DisplayInfo;
    use crate::lifecycle::{self, Release};
    use crate::policy::{self, Layout, Linger};
    use crate::{Mode, VirtualDisplay, VirtualOutput};

    enum ReuseOutcome {
        Reused(VirtualOutput),
        /// Dead kept display, already removed with its group restore. Caller creates fresh.
        Dead(Drained),
        Miss,
    }

    struct Reg {
        entries: Mutex<Vec<Entry>>,
        generation: AtomicU64,
    }

    static REG: OnceLock<Reg> = OnceLock::new();

    fn reg() -> &'static Reg {
        REG.get_or_init(|| Reg {
            entries: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
        })
    }

    /// Bound on waiting out another create for the same seat. A create publishes its entry within
    /// its own budget — gamescope's node wait is 15 s — and a waiter past this creates anyway: a
    /// second compositor is recoverable, a connect that never returns is not.
    const SEAT_CREATE_WAIT: Duration = Duration::from_secs(30);

    /// Creates in flight, [`slot_key`] and the thread running each.
    static CREATING: Mutex<Vec<(String, std::thread::ThreadId)>> = Mutex::new(Vec::new());
    static CREATE_DONE: std::sync::Condvar = std::sync::Condvar::new();

    /// Hold this seat's create slot for the length of a create.
    ///
    /// A create publishes no pool entry for about a second, so two on one seat — a pre-warm and
    /// that device's own connect — leave two compositors under one Steam home, each killing the
    /// other's. The waiter takes the slot only once the entry it wanted exists, so the reuse
    /// probe under the same slot finds it. Shared-plane backends are never keyed.
    ///
    /// LOCK ORDER: this slot, then the pool lock. Never the reverse.
    fn create_slot(backend: &'static str, isolation: &Option<String>) -> SeatCreate {
        let Some(key) = slot_key(backend, isolation) else {
            return SeatCreate(None);
        };
        let me = std::thread::current().id();
        let mut busy = CREATING.lock().unwrap_or_else(|e| e.into_inner());
        match slot_state(&busy, &key, me) {
            Slot::Mine => return SeatCreate(None),
            Slot::Free => {}
            Slot::Wait => {
                let since = Instant::now();
                let (guard, wait) = CREATE_DONE
                    .wait_timeout_while(busy, SEAT_CREATE_WAIT, |b| {
                        slot_state(b, &key, me) == Slot::Wait
                    })
                    .unwrap_or_else(|e| e.into_inner());
                busy = guard;
                if wait.timed_out() {
                    tracing::warn!(
                        seat = %key,
                        secs = SEAT_CREATE_WAIT.as_secs(),
                        "virtual display: a create on this seat never finished — creating beside it"
                    );
                } else {
                    tracing::info!(
                        seat = %key,
                        waited_ms = since.elapsed().as_millis() as u64,
                        "virtual display: waited out the create already standing this seat up"
                    );
                }
            }
        }
        busy.push((key.clone(), me));
        SeatCreate(Some(key))
    }

    /// Releases the seat's create slot ([`create_slot`]). `None` held nothing.
    struct SeatCreate(Option<String>);

    impl Drop for SeatCreate {
        fn drop(&mut self) {
            let Some(key) = self.0.take() else { return };
            let mut busy = CREATING.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = busy.iter().position(|(k, _)| *k == key) {
                busy.swap_remove(i);
            }
            drop(busy);
            CREATE_DONE.notify_all();
        }
    }

    /// Pre-warm `vd` at `mode` and keep it until a session claims it. `Some(true)` created the
    /// compositor; `Some(false)` adopted a display already kept.
    ///
    /// `None` when this seat holds a display a pre-warm must not take, and the mode it parks at
    /// otherwise: [`park_mode_for`] decides both.
    pub(super) fn park(vd: &mut Box<dyn VirtualDisplay>, mode: Mode) -> Result<Option<bool>> {
        let (backend, isolation) = (vd.name(), vd.isolation_key());
        let shape = (vd.hdr(), vd.hw_cursor());
        let resizable = vd.can_resize_kept();
        // Taken before the check and held past the lease drop below, so a connect arriving while
        // this seat is being stood up waits and then reuses it instead of spawning its own.
        let _seat = create_slot(backend, &isolation);
        // Under the pool lock, so a session that already registered its display wins the race.
        let held: Vec<Held> = REG
            .get()
            .map(|r| {
                r.entries
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| e.backend == backend && e.isolation == isolation)
                    .map(|e| {
                        (
                            matches!(e.life, lifecycle::State::Active { .. }),
                            (e.hdr, e.hw_cursor),
                            e.mode,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(mode) = park_mode_for(&held, shape, mode, resizable) else {
            return Ok(None);
        };
        let out = acquire(vd, mode, Arc::new(AtomicBool::new(false)), None, true)?;
        let created = out.reused_gen.is_none();
        // Dropping the output releases the lease, which parks the entry; the compositor
        // keepalive stays in the pool.
        drop(out);
        Ok(Some(created))
    }

    /// Isolation keys of the seats parked right now — the pre-warm's own cap and its
    /// already-parked check.
    pub(super) fn parked_isolations() -> Vec<String> {
        let Some(r) = REG.get() else {
            return Vec::new();
        };
        let es = r.entries.lock().unwrap();
        es.iter()
            .filter(|e| e.parked)
            .filter_map(|e| e.isolation.clone())
            .collect()
    }

    /// Identity slots currently in the pool. Takes the pool lock — never call
    /// while holding it.
    pub(super) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
        let Some(r) = REG.get() else {
            return Default::default();
        };
        let es = r.entries.lock().unwrap();
        es.iter().filter_map(|e| e.identity_slot).collect()
    }

    /// [`budget_count`] of the pool. `0` before the first acquire (registry not initialised).
    pub(super) fn budget_display_count() -> u32 {
        REG.get()
            .map(|r| budget_count(&r.entries.lock().unwrap()))
            .unwrap_or(0)
    }

    /// Linger from console `keep_alive`, for the device that owns this display.
    ///
    /// A per-device overlay is the reason this takes a slot rather than reading the host
    /// policy flat: the TV keeps its screen forever while the tablet's goes at once, and
    /// teardown is where that difference has to land (§6.1). A slot with no recorded owner
    /// is shared or anonymous and follows the host, which is the right answer.
    ///
    /// Absent config is still `Immediate` — an unconfigured host tears down on disconnect.
    fn linger_for(identity_slot: Option<u32>) -> Linger {
        let fp = crate::identity::slot_owner(identity_slot);
        policy::prefs()
            .configured()
            .map(|p| p.effective_for(fp.as_deref()).keep_alive.linger())
            .unwrap_or(Linger::Immediate)
    }

    /// Do not wrap `spawn` in `Once` and discard `Result`: a failed spawn
    /// (EAGAIN / RLIMIT_NPROC) would consume the Once and leave kept displays
    /// unreaped for the process lifetime. Set the flag only on success; the
    /// mutex makes check-and-spawn atomic.
    fn ensure_timer() {
        static STARTED: Mutex<bool> = Mutex::new(false);
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        match std::thread::Builder::new()
            .name("vdisplay-linger".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_millis(500));
                reap(crate::session_epoch());
            }) {
            Ok(_) => *started = true,
            Err(e) => tracing::error!(
                error = %e,
                "virtual display: could not start the keep-alive linger reaper — kept displays \
                 will not expire until a later session retries"
            ),
        }
    }

    impl Drained {
        /// Run the restores, then drop the entries: outside the pool lock (a keepalive `Drop`
        /// can block), restore first so the compositor never sees zero outputs. Returns the
        /// count torn down.
        fn finish(self, why: &str) -> usize {
            for restore in self.restores {
                restore();
            }
            let n = self.entries.len();
            for e in self.entries {
                tracing::info!(backend = e.backend, "virtual display: {why}");
                drop(e);
            }
            emit_released(n);
            n
        }
    }

    /// Emit `Released` for `n` displays. Every Linux teardown path goes through
    /// here so linger expiry and compositor-gone invalidation leave the console.
    fn emit_released(n: usize) {
        if n > 0 {
            crate::emit_display_event(crate::DisplayEvent::Released { count: n as u32 });
        }
    }

    /// Session-facing output: kept node + generation-stamped lease. Pooled
    /// backends reach here with `remote_fd` None; Hyprland recast may fill it.
    #[allow(clippy::too_many_arguments)]
    fn output_for(
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        (output_name, input_output): (Option<String>, Option<String>),
        seat: Option<String>,
        generation: u64,
        quit: Arc<AtomicBool>,
        reused: bool,
    ) -> VirtualOutput {
        let mut out = VirtualOutput::owned(
            node_id,
            preferred_mode,
            Box::new(DisplayLease { generation, quit }),
        );
        // Same head as at create, so it answers with the same name.
        out.output_name = output_name;
        out.input_output = input_output;
        // Same compositor as at create, so its launches and watches stay on this seat.
        out.seat = seat;
        // First-frame failure on reuse can `mark_failed` instead of re-wedging.
        out.reused_gen = reused.then_some(generation);
        // Mode-switch rebuild `retire`s the entry this output's successor supersedes.
        out.pool_gen = Some(generation);
        out
    }

    /// Tear down the kept displays whose linger ran out or whose session epoch ended.
    fn reap(cur_epoch: u64) {
        let expired = {
            let mut es = reg().entries.lock().unwrap();
            take_expired(&mut es, Instant::now(), cur_epoch)
        };
        expired.finish("linger expired — torn down");
    }

    /// `park` marks what this acquire creates as pre-warmed: it is a display with no session
    /// behind it, so the lease drop keeps it instead of applying the keep-alive policy.
    pub(super) fn acquire(
        vd: &mut Box<dyn VirtualDisplay>,
        mode: Mode,
        quit: Arc<AtomicBool>,
        supersedes: Option<u64>,
        park: bool,
    ) -> Result<VirtualOutput> {
        ensure_timer();
        let backend = vd.name();
        // Reuse keys: isolation must match; epoch is current. The launch command is deliberately
        // NOT one — a kept spawn serves any title and the session launches into it. Keying on it
        // spawned a second compositor per game, which broke the socket lock and Steam.
        let isolation = vd.isolation_key();
        let cur_epoch = crate::session_epoch();
        let r = reg();
        reap(cur_epoch);

        // Before linger reuse: admission named a live session, so share its display.
        if vd.join_live() {
            if let Some(out) = join_live(vd, backend, mode, &isolation, cur_epoch, &quit) {
                return Ok(out);
            }
        }

        // Held across the reuse probe and the create below: a second create on this seat is a
        // second compositor under one Steam home, and a waiter must see what the first publishes.
        let _seat = create_slot(backend, &isolation);
        if let Some(out) = try_reuse(vd, mode, &isolation, cur_epoch, park, &quit) {
            return Ok(out);
        }

        // Nothing reusable, so this acquire creates. A sole-instance backend must not end up
        // with two: retire the kept one first. `Entry`'s Drop kills and waits, so the socket name
        // is free by the time `create` runs. Active entries are refused by `force_release`.
        if vd.sole_instance() {
            retire_incompatible(backend, &isolation);
        }

        // Never refuse on `max_displays` here: `acquire` reruns on rebuild while the old lease
        // still counts. Admission skips lingering displays, so at the cap this create evicts one.
        let max = policy::prefs().get().effective().max_displays;
        let evict = kept_to_evict(&r.entries.lock().unwrap(), max, supersedes);
        if let Some(g) = evict {
            release_kept(Some(g), "evicted (max_displays reached)");
        }

        // Stamp generation before group questions: a gamescope spawn's group
        // IS its generation. A burned stamp on failed create is fine (opaque,
        // monotonic, never an index).
        let generation = r.generation.fetch_add(1, Ordering::Relaxed);

        // First-in-group excludes `supersedes` (still Active); kept leftovers have no session
        // to clobber.
        let first_in_group = {
            let es = r.entries.lock().unwrap();
            !es.iter().any(|e| {
                in_group(e.backend, e.generation, backend, generation, supersedes)
                    && matches!(e.life, lifecycle::State::Active { .. })
            })
        };
        vd.set_first_in_group(first_in_group);

        // Not under the lock: `vd.create` blocks and spawns threads. Hyprland and
        // sway park the fresh ScreenCast for the session attachment below; direct
        // `create` callers still receive a complete output.
        vd.set_session_cast_handoff(true);
        let real = vd.create(mode);
        vd.set_session_cast_handoff(false);
        let real = real?;

        // Pool only `Owned` with no portal fd on the output. Pass through
        // `External`/`SessionManaged` (gamescope owns those; pooling wedges on
        // a stale node) and `remote_fd = Some` (a portal fd cannot be reopened).
        // Hyprland and sway leave the fd off so this arm pools the named head.
        if real.ownership != crate::DisplayOwnership::Owned || real.remote_fd.is_some() {
            tracing::debug!(
                backend,
                ownership = ?real.ownership,
                "virtual display not registry-poolable — keep-alive off (owner keeps it / portal fd)"
            );
            return Ok(real);
        }
        pool_created(
            vd, real, generation, mode, isolation, cur_epoch, park, supersedes, quit,
        )
    }

    /// Reuse a kept display that matches this acquire, resizing it in place when that is the
    /// only mismatch. `None` means create: nothing matched, the kept one refused the mode, its
    /// compositor was dead (torn down here, restore handed on), or the recast failed.
    ///
    /// Liveness and resize may block (`pw-dump`, the compositor), so they run outside the pool
    /// lock: snapshot the candidate, probe, then re-find it by generation. A concurrent reuse
    /// or remove just misses.
    fn try_reuse(
        vd: &mut Box<dyn VirtualDisplay>,
        mode: Mode,
        isolation: &Option<String>,
        cur_epoch: u64,
        park: bool,
        quit: &Arc<AtomicBool>,
    ) -> Option<VirtualOutput> {
        // Gamescope managed/attach shares the `"gamescope"` name with a bare spawn and must
        // not reuse it.
        if !vd.poolable_now() {
            return None;
        }
        let backend = vd.name();
        let r = reg();
        // A backend that can move a kept display to another mode is offered one whose only
        // mismatch is the mode; every other key still has to match exactly.
        let resizable = vd.can_resize_kept();
        let kept = |e: &Entry| {
            matches!(
                e.life,
                lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
            )
        };
        let (cand_gen, node_id, pid, cand_mode, cand_seat) = {
            let es = r.entries.lock().unwrap();
            es.iter()
                .find(|e| {
                    kept(e)
                        && reuse_keys_match(
                            e,
                            backend,
                            mode,
                            isolation,
                            vd.hw_cursor(),
                            vd.hdr(),
                            cur_epoch,
                            resizable,
                        )
                        && vd.accepts_kept(e.identity_slot, e.output_name.as_deref())
                })
                .map(|e| (e.generation, e.node_id, e.pid, e.mode, e.seat.clone()))
        }?;
        // A dead compositor is dead whatever PipeWire still lists under its node id.
        let alive = pid.is_none_or(crate::proc::pid_alive) && vd.kept_display_alive(node_id);
        // A kept display whose only mismatch is the mode moves to it instead of being retired,
        // so the Steam a pre-warm booted inside it survives the change. A refusal falls through
        // to the create, which is the retire-and-spawn this always did.
        let resized = cand_mode == mode || (alive && vd.resize_kept(cand_seat.as_deref(), mode));
        if alive && !resized {
            tracing::info!(
                backend,
                node_id,
                seat = cand_seat.as_deref().unwrap_or("-"),
                "virtual display: the kept compositor did not take the new mode — \
                 retiring it and spawning"
            );
        }
        let reuse = {
            let mut es = r.entries.lock().unwrap();
            let idx = es.iter().position(|e| e.generation == cand_gen && kept(e));
            match (idx, kept_verdict(alive, resized)) {
                (Some(idx), Kept::Reuse) => {
                    let generation = r.generation.fetch_add(1, Ordering::Relaxed);
                    ReuseOutcome::Reused(claim_kept(
                        &mut es[idx],
                        generation,
                        mode,
                        park,
                        isolation,
                        quit,
                    ))
                }
                (Some(idx), Kept::Dead) => {
                    let g = es[idx].generation;
                    ReuseOutcome::Dead(drain_where(&mut es, |e| e.generation == g))
                }
                // `None`: adopted or removed by another thread.
                (Some(_), Kept::Spawn) | (None, _) => ReuseOutcome::Miss,
            }
        };
        match reuse {
            ReuseOutcome::Reused(out) => {
                let pool_gen = out.pool_gen;
                match attach_session_cast(vd, out) {
                    Ok(out) => return Some(out),
                    Err(e) => {
                        if let Some(g) = pool_gen {
                            mark_failed(g);
                        }
                        tracing::info!(
                            backend,
                            error = %format!("{e:#}"),
                            "virtual display: recast of kept head failed — recreating"
                        );
                    }
                }
            }
            ReuseOutcome::Dead(dead) => {
                dead.finish("kept display was dead — recreating (validated reuse)");
            }
            ReuseOutcome::Miss => {}
        }
        None
    }

    /// Hand kept entry `e` to this acquire under the pool lock: re-stamp it with `generation`,
    /// settle its parked flag, and record `mode` once the compositor has confirmed it.
    fn claim_kept(
        e: &mut Entry,
        generation: u64,
        mode: Mode,
        park: bool,
        isolation: &Option<String>,
        quit: &Arc<AtomicBool>,
    ) -> VirtualOutput {
        let (backend, node_id) = (e.backend, e.node_id);
        e.life.acquire();
        e.generation = generation;
        // A session claiming a parked seat makes it an ordinary display, ending by the linger
        // rules. A re-park keeps it parked.
        let claimed = e.parked && !park;
        e.parked = park;
        // The compositor has confirmed the new mode, so the entry is at it: its reuse key, and
        // what the capture is told to expect.
        if e.mode != mode {
            let from = e.mode;
            e.mode = mode;
            e.preferred_mode = Some((mode.width, mode.height, mode.refresh_hz));
            tracing::info!(
                backend,
                node_id,
                seat = e.seat.as_deref().unwrap_or("-"),
                from = %format!("{}x{}@{}", from.width, from.height, from.refresh_hz),
                to = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                "virtual display: kept compositor resized for this session — \
                 nothing inside it restarts"
            );
        }
        tracing::info!(
            backend,
            node_id,
            seat = e.seat.as_deref().unwrap_or("-"),
            "virtual display reused (keep-alive reconnect)"
        );
        if claimed {
            tracing::info!(
                backend,
                node_id,
                isolation = isolation.as_deref().unwrap_or("-"),
                "virtual display: this session claimed its parked seat — its \
                 Steam is already up"
            );
        }
        output_for(
            node_id,
            e.preferred_mode,
            (e.output_name.clone(), e.input_output.clone()),
            e.seat.clone(),
            generation,
            quit.clone(),
            true,
        )
    }

    /// File a fresh poolable display in the pool, place it in its group, and attach the
    /// session cast. A failed attach marks the entry failed so the next acquire recreates.
    #[allow(clippy::too_many_arguments)]
    fn pool_created(
        vd: &mut Box<dyn VirtualDisplay>,
        real: VirtualOutput,
        generation: u64,
        mode: Mode,
        isolation: Option<String>,
        cur_epoch: u64,
        park: bool,
        supersedes: Option<u64>,
        quit: Arc<AtomicBool>,
    ) -> Result<VirtualOutput> {
        let r = reg();
        let backend = vd.name();
        let identity_slot = vd.last_identity_slot();
        let node_id = real.node_id;
        let preferred_mode = real.preferred_mode;
        let output_name = real.output_name.clone();
        // KWin and Mutter report their cast name apart from `output_name`, which drives
        // input aiming and direct capture. The rest cast by the output name.
        let join_name = vd
            .last_join_name()
            .or_else(|| output_name.clone().map(|n| Arc::new(OnceLock::from(n))));
        // Fresh create may start at a sacrificial mode (KWin >60 Hz) that must
        // renegotiate before frames count. Reuse already did; leave the flag off.
        let expect_exact_dims = real.expect_exact_dims;
        // Group restore: run once when the last member drops, not at this
        // session's teardown. `None` for non-exclusive / non-first / auto-revert.
        let topology_restore = vd.take_topology_restore();
        let mut life = lifecycle::State::default();
        life.acquire(); // Idle → Active{refs:1} (Acquire::Create)
        let entry = Entry {
            life,
            keepalive: real.keepalive,
            node_id,
            preferred_mode,
            output_name: output_name.clone(),
            input_output: real.input_output.clone(),
            join_name,
            mode,
            backend,
            identity_slot,
            topology_restore,
            isolation,
            seat: real.seat.clone(),
            pid: real.pid,
            epoch: cur_epoch,
            generation,
            hw_cursor: vd.hw_cursor(),
            hdr: vd.hdr(),
            parked: park,
        };

        // Position then push under the same lock (I/O-free). Apply is below,
        // outside the lock.
        let position = {
            use crate::layout::Member;
            let layout_policy = policy::prefs()
                .configured_effective()
                .map(|e| e.layout)
                .unwrap_or_default();
            let mut es = r.entries.lock().unwrap();
            // Same-group, excluding `supersedes` — else a resize auto-rows past
            // the predecessor and walks one width right on every mode switch.
            let existing: Vec<(u64, Member)> = es
                .iter()
                .filter(|e| in_group(e.backend, e.generation, backend, generation, supersedes))
                .map(|e| {
                    (
                        e.generation,
                        Member {
                            identity_slot: e.identity_slot,
                            width: e.mode.width as i32,
                        },
                    )
                })
                .collect();
            let new_member = Member {
                identity_slot,
                width: mode.width as i32,
            };
            let pos = position_for_new(existing, new_member, &layout_policy);
            es.push(entry);
            pos
        };
        // Apply position outside the lock (kscreen blocks). Skip (0, 0): that
        // is the compositor default, so first-of-group and non-KWin (no-op
        // `apply_position`) issue no positioning.
        if (position.x, position.y) != (0, 0) {
            vd.apply_position(position.x, position.y);
        }
        let mut out = output_for(
            node_id,
            preferred_mode,
            (output_name, real.input_output.clone()),
            real.seat.clone(),
            generation,
            quit,
            false,
        );
        out.expect_exact_dims = expect_exact_dims;
        match attach_session_cast(vd, out) {
            Ok(out) => Ok(out),
            Err(e) => {
                mark_failed(generation);
                Err(e)
            }
        }
    }

    /// `mode_conflict: join`: take a hold on the [`join_target`] and give this session its
    /// own cast of it (`VirtualDisplay::join_cast`). The generation is not re-stamped, so the
    /// owner's lease still releases it. `None` means nothing to join or no cast, and the
    /// caller creates. Dropping the output on that path gives the hold back.
    fn join_live(
        vd: &mut Box<dyn VirtualDisplay>,
        backend: &'static str,
        mode: Mode,
        isolation: &Option<String>,
        cur_epoch: u64,
        quit: &Arc<AtomicBool>,
    ) -> Option<VirtualOutput> {
        let (out, name, node_id) = {
            let mut es = reg().entries.lock().unwrap();
            let idx = join_target(&es, backend, mode, isolation, vd.hdr(), cur_epoch)?;
            let e = &mut es[idx];
            let name = e.join_name.as_ref()?.get()?.clone();
            e.life.acquire();
            tracing::info!(
                backend,
                output = %name,
                node_id = e.node_id,
                "mode-conflict: JOIN — sharing the live display"
            );
            let out = output_for(
                e.node_id,
                e.preferred_mode,
                (e.output_name.clone(), e.input_output.clone()),
                e.seat.clone(),
                e.generation,
                quit.clone(),
                true,
            );
            (out, name, e.node_id)
        };
        match vd.join_cast(&name, node_id) {
            Ok(Some(parts)) => Some(with_cast(out, parts)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    backend,
                    error = %format!("{e:#}"),
                    "live display join cast did not start — creating this session's own display"
                );
                None
            }
        }
    }

    /// Hyprland and sway: attach a session-scoped ScreenCast (pending from `create`, or a
    /// recast on reconnect). Other backends return `None` and leave `out`.
    fn attach_session_cast(
        vd: &mut Box<dyn VirtualDisplay>,
        out: VirtualOutput,
    ) -> Result<VirtualOutput> {
        let Some(name) = out.output_name.clone() else {
            return Ok(out);
        };
        Ok(match vd.session_cast_for(&name)? {
            Some(parts) => with_cast(out, parts),
            None => out,
        })
    }

    /// Put a session cast on `out`: its node and fd, and a keepalive that closes the cast
    /// before the lease drops.
    fn with_cast(
        mut out: VirtualOutput,
        (node_id, fd, cast): crate::backend::SessionCastParts,
    ) -> VirtualOutput {
        out.node_id = node_id;
        out.remote_fd = fd;
        let lease = std::mem::replace(&mut out.keepalive, Box::new(()));
        out.keepalive = Box::new(CastAndLease {
            _cast: cast,
            _lease: lease,
        });
        out
    }

    /// Drop order: close ScreenCast, then the registry lease (linger vs teardown).
    struct CastAndLease {
        _cast: Box<dyn Send>,
        _lease: Box<dyn Send>,
    }

    /// [`DisplayLease`] drop: lifecycle decides linger / pin / teardown.
    /// Torn-down keepalive drops after the lock is released.
    fn release(generation: u64, force_immediate: bool) {
        let Some(r) = REG.get() else { return };
        let (torn_down, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.generation == generation) else {
                return; // stale lease (entry reused + re-stamped, or already gone) — no-op
            };
            // Resolved here, not before the lookup: the answer belongs to the display's
            // OWNER, and the entry is what names it.
            let linger = release_linger(
                es[idx].parked,
                force_immediate,
                linger_for(es[idx].identity_slot),
            );
            match es[idx].life.release(Instant::now(), linger) {
                Release::Teardown => {
                    let mut e = es.remove(idx);
                    let (backend, g) = (e.backend, e.generation);
                    let restore = hand_off_restore(&mut es, backend, g, e.topology_restore.take());
                    (Some(e), restore)
                }
                // No live hold: do nothing. Do not treat this as Teardown — a
                // stale/duplicate drop would kill the display. Unreachable
                // (lookup is by unique generation) but must match Windows.
                Release::Noop => (None, None),
                Release::Linger => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — lingering (keep-alive)"
                    );
                    (None, None)
                }
                Release::Pin if es[idx].parked => {
                    tracing::info!(
                        backend = es[idx].backend,
                        isolation = es[idx].isolation.as_deref().unwrap_or("-"),
                        w = es[idx].mode.width,
                        h = es[idx].mode.height,
                        hz = es[idx].mode.refresh_hz,
                        "virtual display: parked for its seat until a session claims it"
                    );
                    (None, None)
                }
                Release::Pin => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — pinned (keep-alive forever)"
                    );
                    (None, None)
                }
                // A JOIN session left a shared display; another session still holds it.
                Release::Decref => (None, None),
            }
        };
        // Restore physicals (group emptied) before dropping the output, outside the lock.
        if let Some(restore) = restore {
            restore();
        }
        if let Some(e) = torn_down {
            if force_immediate {
                tracing::info!(
                    backend = e.backend,
                    "virtual display torn down (deliberate quit — keep-alive skipped)"
                );
            } else {
                tracing::info!(
                    backend = e.backend,
                    "virtual display torn down (keep-alive off / released)"
                );
            }
            drop(e); // outside the lock — the keepalive Drop may block
            emit_released(1);
        }
    }

    pub(super) fn snapshot() -> Vec<DisplayInfo> {
        let Some(r) = REG.get() else {
            return Vec::new();
        };
        let now = Instant::now();

        // Flatten under the lock. Skip Idle — never stored, but the match is exhaustive.
        let rows: Vec<Row> = {
            let es = r.entries.lock().unwrap();
            es.iter()
                .filter_map(|e| {
                    let expires_in_ms = match e.life {
                        lifecycle::State::Idle => return None,
                        lifecycle::State::Lingering { until } => {
                            Some(until.saturating_duration_since(now).as_millis() as u64)
                        }
                        lifecycle::State::Active { .. } | lifecycle::State::Pinned => None,
                    };
                    Some(Row {
                        generation: e.generation,
                        backend: e.backend,
                        mode: e.mode,
                        identity_slot: e.identity_slot,
                        state: e.life.label(),
                        expires_in_ms,
                        sessions: e.life.refs(),
                    })
                })
                .collect()
        };

        let topology = super::topology_str();
        let layout_policy: Layout = policy::prefs()
            .configured_effective()
            .map(|e| e.layout)
            .unwrap_or_default();
        // Process-lifetime group ids. Lives here, not in the pure core: a new
        // group must never renumber an existing one under the console.
        let mut keys: Vec<String> = rows
            .iter()
            .map(|r| group_key(r.backend, r.generation))
            .collect();
        keys.sort();
        keys.dedup();
        static GROUP_IDS: Mutex<Option<(std::collections::BTreeMap<String, u32>, u32)>> =
            Mutex::new(None);
        let ids = {
            let mut g = GROUP_IDS.lock().unwrap_or_else(|e| e.into_inner());
            let (known, next) = g.get_or_insert_with(|| (Default::default(), 1));
            assign_group_ids(known, next, &keys);
            known.clone()
        };

        assemble_displays(rows, &layout_policy, &topology, &ids)
    }

    pub(super) fn force_release(slot: Option<u64>) -> usize {
        release_kept(slot, "released (mgmt /display/release)")
    }

    /// Host stopping: every entry, active ones included. The process exits without running a
    /// destructor, so a restore or output left here stays on the box.
    pub(super) fn teardown_all() -> usize {
        let Some(r) = REG.get() else { return 0 };
        let drained = {
            let mut es = r.entries.lock().unwrap();
            drain_where(&mut es, |_| true)
        };
        drained.finish("torn down (host stopping)")
    }

    /// Force-release a display superseded by a mid-stream mode switch. Same as
    /// [`force_release`] (kept only; Active refused; already-gone no-op); distinct log.
    pub(super) fn retire(generation: u64) {
        release_kept(Some(generation), "retired (superseded by a mode switch)");
    }

    /// Tear down every kept display of `backend` sharing `isolation`, so a sole-instance
    /// backend never runs two. Active entries are left alone (`force_release` refuses them):
    /// a live session keeps its own compositor, and this acquire creates alongside it.
    pub(super) fn seat_for(pool_gen: u64) -> Option<String> {
        let r = REG.get()?;
        let es = r.entries.lock().unwrap();
        es.iter()
            .find(|e| e.generation == pool_gen)
            .and_then(|e| e.seat.clone())
    }

    pub(super) fn compositor_pid_for(pool_gen: u64) -> Option<u32> {
        let r = REG.get()?;
        let es = r.entries.lock().unwrap();
        es.iter()
            .find(|e| e.generation == pool_gen)
            .and_then(|e| e.pid)
    }

    pub(super) fn retire_incompatible(backend: &'static str, isolation: &Option<String>) {
        let Some(r) = REG.get() else { return };
        let doomed = {
            let es = r.entries.lock().unwrap();
            kept_to_retire(&es, backend, isolation)
        };
        for g in doomed {
            release_kept(
                Some(g),
                "retired (a sole-instance backend must not run two)",
            );
        }
    }

    /// Tear down kept (lingering/pinned) entries — all, or one by generation —
    /// with keepalive drops outside the lock. Shared by [`force_release`] and [`retire`].
    fn release_kept(slot: Option<u64>, why: &'static str) -> usize {
        let Some(r) = REG.get() else { return 0 };
        let released = {
            let mut es = r.entries.lock().unwrap();
            drain_where(&mut es, |e| {
                slot.is_none_or(|s| e.generation == s) && e.life.force_release()
            })
        };
        released.finish(why)
    }

    /// Tear down a reused-but-dead pool entry by generation. Drops keepalive
    /// outside the lock. Idempotent (already gone → no-op).
    pub(super) fn mark_failed(generation: u64) {
        let Some(r) = REG.get() else { return };
        let (torn, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.generation == generation) else {
                return; // already gone — the subsequent stale-generation lease drop no-ops too
            };
            // Another session still streams it (JOIN), so it is not dead. The lease drop decrefs.
            if matches!(es[idx].life, lifecycle::State::Active { refs } if refs > 1) {
                return;
            }
            let mut e = es.remove(idx);
            let (backend, g) = (e.backend, e.generation);
            let restore = hand_off_restore(&mut es, backend, g, e.topology_restore.take());
            (e, restore)
        };
        if let Some(rst) = restore {
            rst(); // outside the lock, before the keepalive drops
        }
        tracing::warn!(
            backend = torn.backend,
            "virtual display: reused kept display was dead on first frame — torn down (A2 mark_failed)"
        );
        drop(torn); // keepalive Drop outside the lock (may block)
        emit_released(1);
    }

    /// Invalidate every display of `backend` (compositor gone). Any lifecycle,
    /// including Active — those sessions rebuild. Drops keepalives outside the
    /// lock (dead sockets fail fast). Selects by backend, not slot/state.
    pub(super) fn invalidate_backend(backend: &str) {
        let Some(r) = REG.get() else { return };
        let removed = {
            let mut es = r.entries.lock().unwrap();
            drain_where(&mut es, |e| e.backend == backend)
        };
        removed.finish("invalidated — compositor instance gone (A4 session switch)");
    }

    /// Session keepalive. Drop releases the registry hold; a stale lease
    /// (reused + re-stamped, or torn down) is a no-op.
    struct DisplayLease {
        generation: u64,
        /// Deliberate stop, not a network drop. Drop tears down immediately
        /// when set; false on a bare disconnect → normal linger.
        quit: Arc<AtomicBool>,
    }

    impl Drop for DisplayLease {
        fn drop(&mut self) {
            release(self.generation, self.quit.load(Ordering::SeqCst));
        }
    }
}
