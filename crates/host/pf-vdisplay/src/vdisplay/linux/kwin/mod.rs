//! KWin virtual-output backend via the privileged `zkde_screencast_unstable_v1` protocol.
//!
//! `stream_virtual_output` creates an output at `width`×`height` (native, unscaled) and returns
//! a PipeWire node on the user's default daemon — [`VirtualOutput::remote_fd`] is therefore
//! `None`. The host must run inside the KWin session (`$WAYLAND_DISPLAY`).
//!
//! The global is restricted: KWin advertises it only to a client whose `.desktop` lists it
//! under `X-KDE-Wayland-Interfaces` (matched by `/proc/<pid>/exe` → `Exec=`). Packages ship
//! `io.unom.punktfunk.host.desktop` for that. The host binary must carry no file capability and
//! must still be the file at its path ([`screencast_withheld`] names which one broke).
//! Headless tests use `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. `createVirtualOutput` needs the
//! DRM backend, or VirtualBackend since KWin 6.5.6.

use super::{Mode, VirtualDisplay, VirtualOutput};
use crate::proc::StopFlag;
use crate::topology_journal::{self as journal, Head, Undo};
use crate::wl_pump::{pump_until, sync_barrier, Pumped, SyncDone};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// In-process `kde_output_management_v2` topology. Avoids a stuck
// libkscreen/kscreen-KDED `kscreen-doctor`. `kwin` consumes it, kscreen fallback.
pub(crate) mod mgmt;

// The `kscreen-doctor` shell-outs: the half that runs when output management declines.
mod kscreen;
pub(crate) use kscreen::kscreen_verdict;

// The seam: which half answers each output operation.
pub(crate) mod outputs;
use mgmt::TopologyKind;
use outputs::Resolved;

// Bindings for the vendored protocol XML, generated inline (no build.rs). Path is relative
// to CARGO_MANIFEST_DIR.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod zkde {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use zkde::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1 as ScreencastStream,
};
use zkde::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1 as Screencast;

/// Protocol `pointer` modes. Cursor-channel sessions use METADATA (`SPA_META_Cursor`);
/// everyone else uses EMBEDDED so KWin composites the pointer. METADATA with no host
/// blend leaves both sides cursor-less (the mutter trap).
const POINTER_METADATA: u32 = 4;
const POINTER_EMBEDDED: u32 = 2;

/// Phrase that marks a repaired KWin refusal. The opener keys on it to skip the
/// `KWin virtual output failed` wrapper — that prefix is what `is_permanent_build_error`
/// matches, so wrapping a repaired refusal would kill the retry that consumes the repair.
/// Keep it a phrase, not a code.
const REPAIRED_HINT: &str = "enabled it over output management";

/// Lead of the missing-grant refusal ([`screencast_withheld`]); the opener keys on it.
/// The docs quote this text, so it stays verbatim.
const WITHHELD: &str = "KWin does not expose zkde_screencast_unstable_v1 to this client";

/// KWin exposes the created output to output-management as `Virtual-<name>`.
const VOUT_NAME: &str = "punktfunk";

/// Bind cap. KWin advertises 5; we need the `created` event (deprecated since v6) for the node id.
const MAX_VERSION: u32 = 5;

/// KWin virtual-display driver. A paired client's cert fingerprint (set before
/// [`create`](VirtualDisplay::create)) yields a stable per-slot name
/// (`Virtual-punktfunk-<id>`); KWin keys `kwinoutputconfig.json` by name, so reconnect
/// reapplies that client's scale/mode. Each `create` owns its own Wayland connection.
#[derive(Default)]
pub struct KwinDisplay {
    client_fp: Option<[u8; 32]>,
    /// The session's topology ([`VirtualDisplay::set_topology`]).
    topology: Option<crate::policy::Topology>,
    /// Slot the last [`create`](VirtualDisplay::create) resolved, so the registry keys
    /// arrangement and `/display/state` to the same id this backend named the output with.
    last_slot: Option<u32>,
    /// Resolved kscreen address of the last `create` (numeric id, else `Virtual-<name>`).
    /// [`apply_position`](VirtualDisplay::apply_position) must use this, not the shared name:
    /// a superseded sibling is still alive and would take a name-addressed command.
    last_name: Option<String>,
    /// `kde_output_device_v2` UUID of the last `create`, when in-process topology resolved it.
    /// A same-named predecessor can carry it too, so it addresses ours only while it is unique.
    our_uuid: Option<String>,
    /// The last `create`'s [`mgmt::watch`] connection, kept for
    /// [`apply_position`](VirtualDisplay::apply_position): on it the newest output with our
    /// name is ours, even while a predecessor shares that name and UUID.
    watch: Option<mgmt::Session>,
    /// Journal seqs of the heads `exclusive` disabled, across every `create` of this instance.
    /// The registry takes them after a pooled `create` and runs them when the group empties;
    /// [`Drop`] runs what a create that never reached the pool left.
    pending_undo: Vec<u64>,
    /// Cursor-channel session: METADATA at creation; otherwise EMBEDDED.
    hw_cursor: bool,
    /// `mode_conflict: join` admitted this session.
    join_live: bool,
    /// `wl_output.name` of the last `create` (`Virtual-<name>`): what a joiner streams and
    /// what [`apply_position`](VirtualDisplay::apply_position) resolves.
    join_name: Option<String>,
}

impl Drop for KwinDisplay {
    fn drop(&mut self) {
        let seqs = std::mem::take(&mut self.pending_undo);
        journal::apply(&seqs, self);
    }
}

impl KwinDisplay {
    pub fn new() -> Result<Self> {
        Ok(KwinDisplay::default())
    }

    /// Apply topology for the just-created output `our_prefix` (size `dims`) through the
    /// seam ([`outputs::topology`]), recording the UUID or kscreen address the half that
    /// answered resolved for [`apply_position`](VirtualDisplay::apply_position). `Exclusive`
    /// journals the heads it darkens onto `pending_undo` before the disable; the other
    /// topologies darken nothing.
    ///
    /// `pre_enabled` is the non-managed outputs lit before create ([`enabled_physicals`]).
    /// KWin may apply a stored setup for the new monitor set that already has physicals
    /// disabled — so a post-create enumerate can miss them. The snapshot is the only
    /// unpolluted read: `Exclusive` journals it with the plan; other topologies
    /// [`reenable_stranded`]. `sess` is this create's [`mgmt::watch`].
    fn apply_topology(
        &mut self,
        sess: Option<&mut mgmt::Session>,
        name: &str,
        our_prefix: &str,
        dims: (u32, u32),
        pre_enabled: &[(String, String)],
    ) {
        use crate::policy::Topology;
        // The session's topology, else this display's device's: both are set before `create`.
        let kind = match crate::create_topology(self.topology, self.client_fp) {
            Topology::Exclusive => TopologyKind::Exclusive,
            Topology::Primary => TopologyKind::Primary,
            Topology::Extend | Topology::Auto => TopologyKind::Extend,
        };
        let mut seqs = Vec::new();
        let journal_plan =
            |plan: &[(String, String)]| seqs.extend(journal_heads(plan, pre_enabled));
        match outputs::topology(sess, name, our_prefix, dims, kind, journal_plan) {
            Resolved::InProcess(uuid) => self.our_uuid = uuid,
            Resolved::Kscreen(addr) => self.last_name = Some(addr),
            Resolved::Neither(_) => {}
        }
        // Extend and Primary promise the physicals stay lit; undo KWin switching them off
        // in reaction to our output appearing.
        if kind != TopologyKind::Exclusive {
            reenable_stranded(pre_enabled.to_vec());
        }
        self.pending_undo.extend(seqs);
    }
}

/// Serializes `create`, process-wide, as wlroots' `CREATE_LOCK` and Mutter's `TOPOLOGY_LOCK`
/// do: concurrent sessions can share an output name, and only creation order tells them apart.
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl VirtualDisplay for KwinDisplay {
    fn name(&self) -> &'static str {
        "kwin"
    }

    /// Also reports the slot `create` will resolve for this device, read without taking one, so
    /// a reuse and the same-client retire know whose display this is before any create.
    /// `per-client-mode` keys on a mode only `create` has; the 0x0 peek finds none there.
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
        let default = crate::policy::Identity::Shared;
        self.last_slot = crate::identity::peek_slot(fingerprint, (0, 0), default);
    }

    fn set_topology(&mut self, topology: Option<crate::policy::Topology>) {
        self.topology = topology;
    }

    fn topology(&self) -> Option<crate::policy::Topology> {
        Some(crate::create_topology(self.topology, self.client_fp))
    }

    fn accepts_kept(&self, identity_slot: Option<u32>, _output_name: Option<&str>) -> bool {
        crate::identity::may_reuse(identity_slot, self.client_fp)
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn take_undo_seqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_undo)
    }

    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        outputs::list()
    }

    /// Heads come back at their journaled mode, and leave [`HELD_DARK`] first so the
    /// post-teardown pass may re-light them. KWin journals no output: ours goes with the
    /// Wayland connection that made it.
    // A dead host's entry runs too, though KWin re-lit the heads when that host's output went:
    // a head turned off since comes back on. A mark on the entry would skip it.
    fn undo(&mut self, undo: &Undo) -> Result<()> {
        let Undo::EnableHeads { heads } = undo else {
            bail!("KWin journals no {undo:?}");
        };
        let heads: Vec<(String, String)> = heads
            .iter()
            .map(|h| (h.name.clone(), h.mode.clone().unwrap_or_default()))
            .collect();
        let mut held = HELD_DARK.lock().unwrap_or_else(|e| e.into_inner());
        for (name, _) in &heads {
            held.remove(name);
        }
        drop(held);
        restore_heads(&heads);
        Ok(())
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    fn last_join_name(&self) -> Option<crate::backend::JoinName> {
        self.join_name
            .clone()
            .map(|n| Arc::new(std::sync::OnceLock::from(n)))
    }

    /// A second `zkde_screencast` stream of the owner's output. The output lives as long as
    /// the owner's stream, which the registry entry holds.
    fn join_cast(
        &mut self,
        name: &str,
        _node_id: u32,
    ) -> Result<Option<crate::backend::SessionCastParts>> {
        Ok(Some(
            stream_existing_output(name, self.hw_cursor)?.into_cast(),
        ))
    }

    // In-process on the kept watch; kscreen-doctor by the kscreen id the create resolved.
    fn apply_position(&mut self, x: i32, y: i32) {
        let Some(ours) = self.join_name.as_deref() else {
            return;
        };
        let uuid = self.our_uuid.as_deref();
        let kscreen_addr = self.last_name.as_deref();
        outputs::position(self.watch.as_mut(), ours, uuid, kscreen_addr, x, y);
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // One create at a time: a shared identity names every output `Virtual-punktfunk`, and
        // resolving by name and size would otherwise hand a concurrent sibling's output to us.
        let _create = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Per-slot name: a resolved identity becomes `punktfunk-<id>` (KWin exposes
        // `Virtual-punktfunk-<id>` and keys config by name). Shared/anonymous stays
        // `punktfunk`. Two concurrent sessions must not share one name.
        let slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        self.last_slot = slot;
        let name = match slot {
            Some(id) => format!("{VOUT_NAME}-{id}"),
            None => VOUT_NAME.to_string(),
        };
        // Seed `last_name` with `Virtual-<name>` — the only spelling kscreen-doctor
        // resolves. The bare `name` we give KWin matches no output.
        let our_prefix = format!("Virtual-{name}");
        self.last_name = Some(our_prefix.clone());
        self.join_name = Some(our_prefix.clone());
        // A supersede keeps this `KwinDisplay` while the predecessor is still alive: a stale
        // UUID or watch would move the old output. Both are re-set below for this create.
        self.our_uuid = None;
        self.watch = None;
        let (width, height) = (mode.width, mode.height);
        let pointer_mode = if self.hw_cursor {
            POINTER_METADATA
        } else {
            POINTER_EMBEDDED
        };
        let spawn_vout = |w: u32, h: u32| -> Result<(u32, StopFlag)> {
            let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let name_thread = name.clone();
            thread::Builder::new()
                .name("punktfunk-kwin-vout".into())
                .spawn(move || {
                    virtual_output_thread(w, h, name_thread, pointer_mode, setup_tx, stop_thread)
                })
                .context("spawn KWin virtual-output thread")?;
            // Built before the wait so every error arm stops the worker, which on a timeout
            // is still inside `await_created` holding a half-built output.
            let stop = StopFlag(stop);
            match setup_rx.recv_timeout(OPENER_BUDGET) {
                Ok(Ok(v)) => Ok((v, stop)),
                // Report as-is. The wrapper below prepends the "permanent, do not retry"
                // phrase; this is the one refusal whose retry is the point — the repair
                // only fixes the NEXT request.
                Ok(Err(e)) if e.contains(REPAIRED_HINT) => bail!("{e}"),
                // Still permanent, but KWin never saw a request: the text below would mislead.
                Ok(Err(e)) if e.contains(WITHHELD) => bail!("KWin virtual output failed: {e}"),
                // KWin's reason is translated; log the compositor-side cause once here.
                Ok(Err(e)) => bail!(
                    "KWin virtual output failed: {e} — KWin declined to create the output. It \
                     needs a Plasma WAYLAND session on KWin's DRM backend; a nested or \
                     `kwin_wayland --virtual` KWin can only do this since 6.5.6, and on KWin 6.6+ \
                     an output KWin creates but leaves DISABLED (stored \
                     ~/.config/kwinoutputconfig.json, or a display config it refused to apply) \
                     reports the same. kwin_wayland's own journal says which"
                ),
                Err(_) => bail!("timed out creating the KWin virtual output"),
            }
        };
        // `stream_virtual_output` has no refresh; the PipeWire offer (including the
        // `maxFramerate` throttle) is built once at 60 Hz. Only a source resize rebuilds
        // it. Above 60 Hz: birth at a sacrificial height, then install the real custom
        // mode so the first recorded buffers trigger renegotiation (`expect_exact_dims`).
        let want_high = mode.refresh_hz > 60;
        let birth_h = if want_high { height + 16 } else { height };
        // Snapshot enabled physicals before our output exists: create changes the
        // monitor set and KWin may disable them. Later reads can already be polluted.
        let pre_enabled = enabled_physicals();
        // Bound before our output exists: a mode switch's predecessor shares our name,
        // UUID and restored mode, and only this connection tells the two apart.
        let mut mgmt = crate::kwin::mgmt::watch();
        let (mut node_id, mut stop) = spawn_vout(width, birth_h)?;
        // `requested_*`: `spawn_vout` returns a node id, not a size. `width`/`height`
        // here would look like a KWin readback; the real size is below.
        tracing::info!(
            node_id,
            requested_w = width,
            requested_h = height,
            birth_h,
            embedded_pointer = !self.hw_cursor,
            "KWin virtual output ready"
        );
        // `final_dims` ends up as the request unless CVT shrinks the width
        // ([`CVT_H_GRANULARITY`]) or KWin kept a stored slot we could not move. It is
        // `preferred_mode` — capturer gate and encoder both key on it.
        let (final_dims, expect_exact_dims, achieved_hz) = if want_high {
            // Both resolvers address the output by the size it is at, and KWin's stored
            // setup can move it off its birth size before we ever look. Ask where it is.
            let at = crate::kwin::mgmt::actual_dims(mgmt.as_mut(), &our_prefix)
                .map(|(w, h, _, _)| (w, h))
                .unwrap_or((width, birth_h));
            if at != (width, birth_h) {
                tracing::info!(
                    at_w = at.0,
                    at_h = at.1,
                    birth_w = width,
                    birth_h,
                    "KWin moved the new output off its birth size (a stored setup) — addressing \
                     it where it is so the custom mode still lands"
                );
            }
            // Install+select the high-refresh custom mode; the kscreen half addresses ours
            // by the kscreen id it resolved, which later position applies reuse.
            let want = (width, height, mode.refresh_hz);
            let active = match outputs::custom_mode(mgmt.as_mut(), &name, &our_prefix, at, want) {
                Resolved::InProcess(active) => Some(active),
                Resolved::Kscreen((addr, active)) => {
                    self.last_name = Some(addr);
                    active
                }
                Resolved::Neither(_) => None,
            };
            // Accept only our custom mode (exact height, width at or just below). That
            // also proves we left the sacrificial birth size so the stream will renegotiate.
            match active {
                Some((aw, ah, ahz)) if mode_satisfies((aw, ah), width, height) => {
                    ((aw, ah), true, ahz)
                }
                other => {
                    // Install rejected: stuck at sacrificial size. Recreate at the real
                    // size and KWin's native 60 Hz.
                    tracing::warn!(
                        active = ?other,
                        requested_w = width,
                        requested_h = height,
                        requested_hz = mode.refresh_hz,
                        "KWin rejected the custom mode — recreating the virtual output at the real \
                         size (60 Hz ceiling on this KWin)"
                    );
                    stop.0.store(true, Ordering::Relaxed);
                    // Let KWin retire the doomed output before re-using its name.
                    std::thread::sleep(Duration::from_millis(300));
                    let (nid, st) = spawn_vout(width, height)?;
                    node_id = nid;
                    stop = st;
                    // The kscreen id the custom mode resolved named the retired output.
                    self.last_name = Some(our_prefix.clone());
                    tracing::info!(
                        node_id,
                        width,
                        height,
                        "KWin virtual output ready (fallback)"
                    );
                    // The recreate takes the same stored slot as a ≤60 Hz create. Unverified,
                    // the session streams whatever `kwinoutputconfig.json` restored.
                    let (dims, exact) =
                        verify_or_reassert(mgmt.as_mut(), &our_prefix, width, height);
                    (dims, exact, 60)
                }
            }
        } else {
            // ≤60 Hz installs no mode, so nothing here learned what KWin built.
            let (dims, exact) = verify_or_reassert(mgmt.as_mut(), &our_prefix, width, height);
            (dims, exact, mode.refresh_hz)
        };
        self.apply_topology(mgmt.as_mut(), &name, &our_prefix, final_dims, &pre_enabled);
        self.watch = mgmt;
        let mut out = VirtualOutput::owned(
            node_id,
            Some((final_dims.0, final_dims.1, achieved_hz)),
            // Dropping it releases the output: the worker's Wayland connection goes with it.
            Box::new(stop),
        );
        out.expect_exact_dims = expect_exact_dims;
        out.input_output = Some(our_prefix);
        Ok(out)
    }
}

/// Delay after create/teardown before re-checking outputs KWin's stored setup switched
/// off. The stored setup is not synchronous with our reads; the immediate pass catches
/// the common case, this the late apply.
const STRAND_RECHECK_DELAY: Duration = Duration::from_millis(2000);

/// Non-managed outputs currently enabled, each `(name, "WxH@Hz")` — same shape as
/// restore, so the lists interchange. Empty when output management is unavailable.
fn enabled_physicals() -> Vec<(String, String)> {
    crate::kwin::mgmt::list_monitors()
        .map(|ms| {
            ms.into_iter()
                .filter(|m| m.enabled && !m.managed && m.width > 0 && m.height > 0)
                .map(|m| {
                    let hz = ((m.refresh_mhz as f64) / 1000.0).round() as u32;
                    (m.connector, format!("{}x{}@{hz}", m.width, m.height))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Undo stored-config disable of outputs that were lit in `pre_enabled` before create.
/// Two one-shot passes (now, then after [`STRAND_RECHECK_DELAY`]) so a user's later
/// disable stays honored.
fn reenable_stranded(pre_enabled: Vec<(String, String)>) {
    if pre_enabled.is_empty() {
        return;
    }
    reenable_pass(&pre_enabled, "immediate", false);
    reenable_later(pre_enabled, "delayed", false);
}

/// [`reenable_pass`] once more after [`STRAND_RECHECK_DELAY`], off this thread: KWin applies a
/// stored setup on its own time, not ours. One shot, never a loop.
fn reenable_later(expected: Vec<(String, String)>, wave: &'static str, abort_if_managed: bool) {
    std::thread::Builder::new()
        .name("punktfunk-kwin-reenable".into())
        .spawn(move || {
            std::thread::sleep(STRAND_RECHECK_DELAY);
            reenable_pass(&expected, wave, abort_if_managed);
        })
        .ok();
}

/// Heads an `Exclusive` create holds dark, from its journal step until its `undo`.
/// A sibling's re-enable pass, two seconds late, would light them.
static HELD_DARK: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Journal the `exclusive` restore list ([`union_restore`] of `plan` and `pre_enabled`), each
/// head at the mode KWin puts back, before the disable, and hold those heads dark
/// ([`HELD_DARK`]). `None` when there is nothing to restore.
fn journal_heads(plan: &[(String, String)], pre_enabled: &[(String, String)]) -> Option<u64> {
    let heads: Vec<Head> = union_restore(plan.to_vec(), pre_enabled)
        .into_iter()
        .map(|(name, mode)| Head {
            name,
            mode: (!mode.is_empty()).then_some(mode),
        })
        .collect();
    let mut held = HELD_DARK.lock().unwrap_or_else(|e| e.into_inner());
    held.extend(heads.iter().map(|h| h.name.clone()));
    drop(held);
    (!heads.is_empty()).then(|| journal::push("kwin", Undo::EnableHeads { heads }))
}

/// The heads of `expected` that are `dark` now and no `Exclusive` holds dark.
fn to_relight(
    expected: &[(String, String)],
    dark: &[&str],
    held: &BTreeSet<String>,
) -> Vec<(String, String)> {
    expected
        .iter()
        .filter(|(name, _)| dark.contains(&name.as_str()) && !held.contains(name))
        .cloned()
        .collect()
}

/// One pass: re-enable [`to_relight`] through the seam. `abort_if_managed` is post-teardown:
/// a managed output by then is a new session that already owns topology — lighting
/// physicals under its `exclusive` would undo it.
fn reenable_pass(expected: &[(String, String)], wave: &'static str, abort_if_managed: bool) {
    let Ok(now) = mgmt::list_monitors() else {
        return;
    };
    if abort_if_managed && now.iter().any(|m| m.managed) {
        return;
    }
    let off: Vec<&str> = now
        .iter()
        .filter(|m| !m.enabled)
        .map(|m| m.connector.as_str())
        .collect();
    let held = HELD_DARK.lock().unwrap_or_else(|e| e.into_inner());
    let dark = to_relight(expected, &off, &held);
    drop(held);
    if dark.is_empty() {
        return;
    }
    tracing::warn!(
        outputs = ?dark,
        wave,
        "KWin's stored output setup (kwinoutputconfig.json) left physical output(s) disabled that \
         the current topology says stay enabled — re-enabling them"
    );
    outputs::reenable(&dark);
}

/// Exclusive restore list: what this apply disabled, plus every `pre_enabled` output
/// not already in it. KWin may disable physicals in the create window before the apply
/// enumerates them. Re-enabling an already-enabled output is a no-op.
fn union_restore(
    mut disabled: Vec<(String, String)>,
    pre_enabled: &[(String, String)],
) -> Vec<(String, String)> {
    for (name, spec) in pre_enabled {
        if !disabled.iter().any(|(n, _)| n == name) {
            disabled.push((name.clone(), spec.clone()));
        }
    }
    disabled
}

/// Re-enable `heads` (`(name, "WxH@Hz")`) through the seam. This runs under the monitor set
/// with our output; when it goes, KWin applies the setup stored for the set without it, which
/// can disable the heads again, so one delayed pass re-asserts them. One shot, never a loop.
fn restore_heads(heads: &[(String, String)]) {
    outputs::reenable(heads);
    reenable_later(heads.to_vec(), "post-teardown", true);
}

/// CVT horizontal cell grain. KWin times custom modes with libxcvt, whose first step
/// is `hdisplay_rnd = hdisplay - (hdisplay % 8)` — a width not a multiple of 8 comes
/// back narrower, and clock-step rounding lands a fractional refresh (2868×1320@120
/// → 2864×1320@119.92). Never select a custom mode by the requested `WxH@Hz` string:
/// kscreen-doctor `findMode` matches id or `WxH@qRound(Hz)`, so the request matches
/// nothing and the output stays on its sacrificial birth. Shared with
/// [`crate::kwin::mgmt`].
pub(crate) const CVT_H_GRANULARITY: u32 = 8;

/// Does the active mode satisfy a request for `want_w`×`want_h`?
///
/// Exact height, width at or just below — never exact width: libxcvt rounds width
/// down ([`CVT_H_GRANULARITY`]). A width above the request is someone else's mode;
/// `aw <= want_w` also guards the subtraction.
fn mode_satisfies(active: (u32, u32), want_w: u32, want_h: u32) -> bool {
    let (aw, ah) = active;
    ah == want_h && aw <= want_w && want_w - aw < CVT_H_GRANULARITY
}

/// Verify a just-created virtual output really sits at `width`×`height`, re-asserting the mode
/// when a stored slot won instead. Returns the dims to stream at and whether they are ours
/// (arms the capture's birth-mode gate).
///
/// `kwinoutputconfig.json` restores mode+scale by our stable output name, so a slot last at
/// 1080p comes back 1080p on a 4K request. Unverified, capture and encoder open at KWin's size
/// while the client decodes the size it negotiated. A size we cannot correct is returned as it
/// is: everything dims-keyed runs off it, and a monitor mirror legitimately streams a size the
/// client never asked for. `sess` is this create's [`mgmt::watch`]; the re-assert is
/// in-process only.
fn verify_or_reassert(
    mut sess: Option<&mut mgmt::Session>,
    our_prefix: &str,
    width: u32,
    height: u32,
) -> ((u32, u32), bool) {
    match mgmt::actual_dims(sess.as_deref_mut(), our_prefix) {
        // Honoured. Do not force scale 1.0: the stable name exists so KDE reapplies this
        // client's scale. Screencast is PIXEL size, so scale should not move captured dims.
        Some((aw, ah, _, scale)) if (aw, ah) == (width, height) => {
            if scale != 1.0 {
                tracing::debug!(
                    width,
                    height,
                    scale,
                    "KWin virtual output verified at the requested size, carrying a stored \
                     non-unity scale (per-client scaling — capture is unaffected)"
                );
            }
            ((width, height), false)
        }
        Some((aw, ah, _, scale)) => {
            tracing::warn!(
                actual_w = aw,
                actual_h = ah,
                requested_w = width,
                requested_h = height,
                stored_scale = scale,
                our_prefix,
                "KWin built our virtual output at a DIFFERENT size than requested (a stored \
                 kwinoutputconfig.json mode/scale for this output name) — re-asserting the \
                 requested mode so the stream matches what the client negotiated"
            );
            // 60 Hz, not the client's rate: only the size is wrong here, and a 30 fps client
            // would install 30 Hz and throttle the compositor.
            match mgmt::set_custom_mode(sess, our_prefix, aw, ah, width, height, 60) {
                Ok((cw, ch, _)) if mode_satisfies((cw, ch), width, height) => {
                    tracing::info!(
                        active_w = cw,
                        active_h = ch,
                        "KWin virtual output corrected to the requested size"
                    );
                    ((cw, ch), true)
                }
                other => {
                    tracing::warn!(
                        active = ?other,
                        actual_w = aw,
                        actual_h = ah,
                        requested_w = width,
                        requested_h = height,
                        "KWin would not re-assert the requested mode — the output is STUCK at \
                         its stored size. Clear this output's entry from kwinoutputconfig.json \
                         (or set it to the streamed resolution in System Settings → Display) \
                         and reconnect"
                    );
                    ((aw, ah), false)
                }
            }
        }
        // Management unavailable, or our output still mid-announce. Do not reconfigure an
        // output we cannot identify.
        None => {
            tracing::debug!(
                our_prefix,
                "KWin: could not read back the virtual output's actual mode (management \
                 unavailable or the output still announcing) — proceeding unverified"
            );
            ((width, height), false)
        }
    }
}

/// Prefix every managed KWin output shares (`Virtual-punktfunk` / `Virtual-punktfunk-<id>`).
/// Group membership is this prefix so we never thread the live set through the backend.
/// Shared with [`crate::kwin::mgmt`]: a drift would let the in-process path disable
/// a sibling session's output that the kscreen path spares.
pub(crate) const MANAGED_PREFIX: &str = "Virtual-punktfunk";

#[derive(Default)]
struct State {
    screencast: Option<Screencast>,
    node_id: Option<u32>,
    failed: Option<String>,
    closed: bool,
    /// Highest `wl_display.sync` serial whose `done` has arrived — the barrier
    /// [`roundtrip_within`] waits on, so a compositor that stops serving costs a budget.
    sync_done: u32,
    /// Bind `wl_output` objects only on the monitor-mirror path. `stream_virtual_output`
    /// names its output by string; binding them there accumulates globals for the whole
    /// session, and every managed display we create is another `wl_output`.
    want_outputs: bool,
    /// Every `wl_output` KWin advertises: (registry global name, proxy, connector once
    /// `name` arrives). The global name is so `global_remove` can find the entry
    /// ([`State::forget_output`]).
    outputs: Vec<(u32, WlOutput, Option<String>)>,
}

impl State {
    /// The proxy must be `release`d — wayland-rs sends no destructor on drop, so an
    /// unreleased binding leaks for the session. The entry must go too: [`run_existing`]
    /// scans this vector, and a stale row would shadow the live output that reused the
    /// connector name.
    fn forget_output(&mut self, global: u32) {
        let Some(pos) = self.outputs.iter().position(|(n, _, _)| *n == global) else {
            return;
        };
        let (_, out, connector) = self.outputs.remove(pos);
        // `wl_output.release` is `since 3`; below that the object simply has no destructor.
        if out.version() >= 3 {
            out.release();
        }
        tracing::debug!(?connector, "KWin: a wl_output went away — released it");
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == Screencast::interface().name {
                    let v = version.min(MAX_VERSION);
                    state.screencast = Some(registry.bind::<Screencast, _, _>(name, v, qh, ()));
                } else if state.want_outputs && interface == WlOutput::interface().name {
                    // v4 is where `wl_output.name` (the connector) arrives; bind at least
                    // that when offered, else bind what it has and fail loudly.
                    let v = version.min(WL_OUTPUT_MAX_VERSION);
                    let out = registry.bind::<WlOutput, _, _>(name, v, qh, ());
                    state.outputs.push((name, out, None));
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.forget_output(name),
            _ => {}
        }
    }
}

/// `wl_display.sync` callback: `done` releases the [`roundtrip_within`] waiting on
/// this serial. A plain `roundtrip()` has no ceiling — a compositor that stops
/// answering pins the session's stream thread forever.
impl Dispatch<WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        serial: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = state.sync_done.max(*serial);
        }
    }
}

impl SyncDone for State {
    fn sync_done(&self) -> u32 {
        self.sync_done
    }
}

/// Bind `wl_output` at 4: that is the `name` event carrying the connector.
const WL_OUTPUT_MAX_VERSION: u32 = 4;

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = state.outputs.iter_mut().find(|(_, o, _)| o == output) {
                slot.2 = Some(name);
            }
        }
    }
}

// The manager has no events.
impl Dispatch<Screencast, ()> for State {
    fn event(
        _: &mut Self,
        _: &Screencast,
        _: zkde::zkde_screencast_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ScreencastStream, ()> for State {
    fn event(
        state: &mut Self,
        _: &ScreencastStream,
        event: StreamEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => state.node_id = Some(node),
            StreamEvent::Failed { error } => state.failed = Some(error),
            StreamEvent::Closed => state.closed = true,
            // `serial` (v6) — we use the node id from `created`.
            _ => {}
        }
    }
}

/// Create a `width`×`height` virtual output, send its PipeWire node id over `setup_tx`,
/// then keep the Wayland connection alive until `stop` is set.
fn virtual_output_thread(
    width: u32,
    height: u32,
    name: String,
    pointer_mode: u32,
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = run(width, height, &name, pointer_mode, &setup_tx, &stop) {
        let _ = setup_tx.send(Err(format!("{e:#}")));
    }
}

/// Start recording the existing KWin output named `connector` (monitor-mirror).
/// `hw_cursor` selects pointer mode the same way as the virtual-output path.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let pointer_mode = if hw_cursor {
        POINTER_METADATA
    } else {
        POINTER_EMBEDDED
    };
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let connector_thread = connector.to_string();
    thread::Builder::new()
        .name("punktfunk-kwin-mirror".into())
        .spawn(move || {
            if let Err(e) = run_existing(&connector_thread, pointer_mode, &setup_tx, &stop_thread) {
                let _ = setup_tx.send(Err(format!("{e:#}")));
            }
        })
        .context("spawn KWin monitor-mirror thread")?;
    // Built before the wait so a timeout stops the recording too.
    let stop = StopFlag(stop);
    let node_id = match setup_rx.recv_timeout(OPENER_BUDGET) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("KWin monitor mirror failed: {e}"),
        Err(_) => bail!("timed out recording the KWin output {connector:?}"),
    };
    Ok(crate::mirror::MirrorStream {
        node_id,
        // KWin publishes on the user's own PipeWire daemon — no portal remote to carry.
        remote_fd: None,
        // Not an xdg-portal session: the `zkde_screencast` pointer mode was asked of
        // KWin directly, so the request is the answer.
        cursor_mode: None,
        keepalive: Box::new(stop),
    })
}

/// The refusal for a registry without `zkde_screencast`, naming the path KWin read and
/// why it missed.
///
/// KWin (through 6.6) re-checks every new connection: it reads `/proc/<pid>/exe` and
/// matches that path against an installed `.desktop`'s `Exec=`. Two host-side states
/// defeat it: a file capability (the kernel refuses KWin the readlink, see
/// [`capability_denial_hint_for`]) and a binary replaced since start (the link then
/// reads `… (deleted)` or a stale mount's path). A restart is the only repair for the
/// second.
fn screencast_withheld() -> anyhow::Error {
    let exe = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    let replaced = pf_paths::exe_replaced();
    let permitted = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| permitted_caps_from_status(&status));
    anyhow!(
        "{WITHHELD} ({}) — KWin grants it only to a binary whose path is the Exec= of an \
         installed .desktop listing it in X-KDE-Wayland-Interfaces \
         (io.unom.punktfunk.host.desktop){}",
        exe.display(),
        identity_hint(replaced, permitted)
    )
}

/// Tail of [`screencast_withheld`]: every cause the host can see, else the install advice.
fn identity_hint(replaced: bool, permitted: Option<u64>) -> String {
    let mut hint = capability_denial_hint_for(permitted);
    if replaced {
        hint.insert_str(
            0,
            " — this binary was replaced on disk after the host started (a package update?), \
             so KWin no longer matches it: restart the host",
        );
    }
    if hint.is_empty() {
        hint = " — install that .desktop and log in again, or run KWin with \
                KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 for a headless test"
            .into();
    }
    hint
}

/// Capability sentence of [`identity_hint`], split from `/proc/self/status` so it
/// is testable against a given mask. Calling the real reader in CI sees the runner's
/// root permitted set (`CapPrm=0x000001ffffffffff`) and the hint fires on a clean host.
///
/// KWin has no capabilities, and the kernel refuses a `/proc/<pid>/exe` readlink unless
/// the reader's effective set covers the target's permitted set. `PR_SET_DUMPABLE` and
/// systemd `AmbientCapabilities=` leave that check failing.
fn capability_denial_hint_for(permitted: Option<u64>) -> String {
    match permitted {
        Some(caps) if caps != 0 => format!(
            " — NOTE: this process carries capabilities (CapPrm={caps:#018x}), which is enough on \
             its own to cause this: the kernel then refuses KWin the /proc/<pid>/exe read it \
             identifies clients by, so no .desktop can match however correctly it is installed. \
             Clear them with `sudo setcap -r /usr/bin/punktfunk-host` and restart the host"
        ),
        _ => String::new(),
    }
}

/// Permitted-capability mask from a `/proc/<pid>/status` body. The kernel prints a
/// tab-separated 16-digit hex word with no `0x` (`CapPrm:\t0000000000800000`).
fn permitted_caps_from_status(status: &str) -> Option<u64> {
    let field = status.lines().find(|l| l.starts_with("CapPrm:"))?;
    u64::from_str_radix(field.split_whitespace().nth(1)?, 16).ok()
}

#[cfg(test)]
mod capability_hint_tests {
    use super::*;

    /// `/proc/self/status` excerpt from a `cap_sys_nice=ep` process.
    const CAPPED: &str = "Name:\tpunktfunk-host\nUid:\t1000\t1000\t1000\t1000\nCapPrm:\t0000000000800000\nCapEff:\t0000000000800000\n";
    /// Same binary with no capability — the hint must stay silent.
    const CLEAN: &str = "Name:\tpunktfunk-host\nUid:\t1000\t1000\t1000\t1000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n";

    #[test]
    fn parses_the_kernels_permitted_mask() {
        assert_eq!(permitted_caps_from_status(CAPPED), Some(0x0080_0000));
        assert_eq!(permitted_caps_from_status(CLEAN), Some(0));
        // CapPrm is not guaranteed present; stay quiet, never panic.
        assert_eq!(permitted_caps_from_status("Name:\tx\n"), None);
        assert_eq!(permitted_caps_from_status("CapPrm:\tzzzz\n"), None);
        assert_eq!(permitted_caps_from_status("CapPrm:\n"), None);
    }

    /// A capability-free host must not append the hint — the same message is printed
    /// for a genuinely missing `.desktop`, and a spurious setcap line would mislead.
    /// Driven off an explicit mask: see [`capability_denial_hint_for`].
    #[test]
    fn silent_without_capabilities() {
        assert_eq!(
            capability_denial_hint_for(permitted_caps_from_status(CLEAN)),
            ""
        );
        // Absent or unparseable field: also silent.
        assert_eq!(capability_denial_hint_for(None), "");
    }

    /// The capped case must name the mask and the repair, or the silent test also
    /// passes against a function that always returns `""`.
    #[test]
    fn names_the_mask_and_the_repair_when_capped() {
        let hint = capability_denial_hint_for(permitted_caps_from_status(CAPPED));
        assert!(
            hint.contains("0x0000000000800000"),
            "names the mask: {hint}"
        );
        assert!(hint.contains("setcap -r"), "names the repair: {hint}");
    }

    /// A replaced binary names the restart; the install advice appears only when
    /// the host sees no cause of its own.
    #[test]
    fn picks_the_repair_the_host_can_see() {
        let replaced = identity_hint(true, Some(0));
        assert!(replaced.contains("restart the host"), "{replaced}");
        assert!(!replaced.contains("install that .desktop"), "{replaced}");
        let clean = identity_hint(false, Some(0));
        assert!(clean.contains("install that .desktop"), "{clean}");
        let capped = identity_hint(false, permitted_caps_from_status(CAPPED));
        assert!(!capped.contains("install that .desktop"), "{capped}");
    }
}

/// Readiness probe: connect, roundtrip the registry, confirm `zkde_screencast` is
/// advertised. `Ok(())` = ready; `Err` = not ready / no global yet.
pub fn probe() -> Result<()> {
    let conn = crate::session::wayland_connect().context("connect to KWin Wayland")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    // A probe is a one-shot question, bounded by the roundtrip budget.
    let never = AtomicBool::new(false);
    roundtrip_within(
        &conn,
        &mut queue,
        &mut state,
        &never,
        1,
        "registry roundtrip",
    )?;
    if state.screencast.is_none() {
        return Err(screencast_withheld());
    }
    Ok(())
}

/// KWin is usable iff [`probe`] succeeds — a session exposing `zkde_screencast`.
pub fn is_available() -> bool {
    probe().is_ok()
}

/// Stream an existing KWin output — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md`). Same privileged global and keepalive
/// shape as the virtual-output path; `stream_output` takes a `wl_output` instead of
/// minting one. The thread parks until `stop`; dropping it stops the recording and
/// leaves the monitor untouched.
fn run_existing(
    connector: &str,
    pointer_mode: u32,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    // The opener started its clock a moment ago; work before `await_created` comes
    // out of the same 20 s (this path has two barriers — see [`CREATE_BUDGET`]).
    let started = Instant::now();
    let conn = crate::session::wayland_connect().context("connect to KWin Wayland")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // The one path that resolves a connector to a `wl_output`, so the only one that binds them.
    let mut state = State {
        want_outputs: true,
        ..State::default()
    };
    // First roundtrip binds screencast + every wl_output; the second drains each
    // output's property burst — the `name` event we resolve the connector by.
    roundtrip_within(&conn, &mut queue, &mut state, stop, 1, "registry roundtrip")?;
    roundtrip_within(
        &conn,
        &mut queue,
        &mut state,
        stop,
        2,
        "wl_output property roundtrip",
    )?;

    let screencast = state.screencast.clone().ok_or_else(screencast_withheld)?;

    // A miss is a hard error naming what is there: mirroring some other monitor
    // because the requested one is unplugged is worse than a refused session.
    let named: Vec<&str> = state
        .outputs
        .iter()
        .filter_map(|(_, _, n)| n.as_deref())
        .collect();
    let output = state
        .outputs
        .iter()
        .find(|(_, _, n)| n.as_deref() == Some(connector))
        .or_else(|| {
            state.outputs.iter().find(|(_, _, n)| {
                n.as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(connector))
            })
        })
        .map(|(_, o, _)| o.clone())
        .ok_or_else(|| {
            if named.is_empty() {
                anyhow!(
                    "KWin advertised no named wl_output (needs wl_output v4 for the connector \
                     name) — cannot mirror {connector:?}"
                )
            } else {
                anyhow!(
                    "KWin has no output named {connector:?} — it has: {}",
                    named.join(", ")
                )
            }
        })?;

    let stream = screencast.stream_output(&output, pointer_mode, &qh, ());
    tracing::info!(
        connector,
        embedded_pointer = pointer_mode != POINTER_METADATA,
        "KWin: recording an existing output; awaiting PipeWire node"
    );

    let node_id = await_created(
        &conn,
        &mut queue,
        &mut state,
        stop,
        "stream_output",
        started,
    )?;
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("monitor-mirror opener went away"))?;

    park_until_stopped(&conn, &mut queue, &mut state, stop, connector, node_id)?;
    stream.close();
    let _ = conn.flush();
    Ok(())
}

fn run(
    width: u32,
    height: u32,
    name: &str,
    pointer_mode: u32,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    // Same clock as the mirror path: one barrier here, but the create wait is
    // bounded against the opener either way (see [`CREATE_BUDGET`]).
    let started = Instant::now();
    let conn = crate::session::wayland_connect().context("connect to KWin Wayland")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // `stream_virtual_output` names its output by string; this connection lives
    // for the whole session and never needs a `wl_output` (see `State`).
    let mut state = State::default();
    roundtrip_within(&conn, &mut queue, &mut state, stop, 1, "registry roundtrip")?;

    let screencast = state.screencast.clone().ok_or_else(screencast_withheld)?;

    // Pointer rides as stream metadata (cursor-channel) or KWin embeds it.
    let stream = screencast.stream_virtual_output(
        name.to_string(),
        width as i32,
        height as i32,
        1.0, // scale (logical == physical)
        pointer_mode,
        &qh,
        (),
    );
    tracing::info!(
        width,
        height,
        "KWin: requested virtual output; awaiting PipeWire node"
    );

    // A refusal here is the KWin ≥ 6.6 disabled-output trap, repairable only while
    // this connection is up: KWin destroys the output when the stream is destroyed.
    // Enabling it fixes the NEXT request, not this one.
    let node_id = match await_created(
        &conn,
        &mut queue,
        &mut state,
        stop,
        "stream_virtual_output",
        started,
    ) {
        Ok(id) => id,
        Err(e) => {
            // `Virtual-<name>` is the address KWin exposes our output under.
            match crate::kwin::mgmt::enable_disabled_output(&format!("Virtual-{name}")) {
                // Do not carry the "KWin virtual output failed" prefix: that string
                // marks a refusal permanent for the retry loop, and this is the one
                // refusal where something DID change between attempts.
                Some(repaired) => bail!(
                    "KWin created the virtual output disabled and refused to stream it ({e}); \
                     {REPAIRED_HINT} (head {repaired}) — the retry picks up the configuration \
                     KWin just persisted"
                ),
                // No such head, already enabled, or apply refused: the refusal stands
                // and its prefix keeps it permanent so we fail fast.
                None => return Err(e),
            }
        }
    };
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("virtual-output opener went away"))?;

    park_until_stopped(&conn, &mut queue, &mut state, stop, name, node_id)?;

    stream.close();
    let _ = conn.flush();
    Ok(())
}

/// Budget for one compositor roundtrip. Healthy is a few ms; this exists so a KWin
/// that accepted the connection and then stopped serving cannot pin the calling thread.
const ROUNDTRIP_BUDGET: Duration = Duration::from_secs(3);

/// How long an opener waits for the worker's first word.
const OPENER_BUDGET: Duration = Duration::from_secs(20);

/// Slack subtracted from [`OPENER_BUDGET`] so the worker's error can travel one
/// `mpsc` send while the opener is still listening.
const WORKER_MARGIN: Duration = Duration::from_millis(500);

/// Ceiling for the `created` handshake. The worker must give up before its opener,
/// so the client sees a reason rather than a bare timeout with the worker still
/// parked. [`run`] spends one [`ROUNDTRIP_BUDGET`]; [`run_existing`] spends two —
/// a fixed 15 s plus two 3 s barriers exceeds [`OPENER_BUDGET`]. [`await_created`]
/// therefore takes the worker's start instant and bounds by whichever comes first.
const CREATE_BUDGET: Duration = Duration::from_secs(15);

/// A `wl_display.sync` barrier bounded by [`ROUNDTRIP_BUDGET`] — replacement for
/// `EventQueue::roundtrip`, which waits on the socket with no ceiling. `serial`
/// must be unique per connection (callers number from 1).
fn roundtrip_within(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    serial: u32,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + ROUNDTRIP_BUDGET;
    match sync_barrier(conn, queue, state, serial, deadline, Some(stop))? {
        Pumped::Done => Ok(()),
        Pumped::Stopped => bail!("{what} abandoned — the stream was released while we waited"),
        Pumped::Expired => bail!(
            "KWin accepted the Wayland connection but did not answer the {what} within \
             {ROUNDTRIP_BUDGET:?} — the compositor is not serving this client"
        ),
    }
}

/// Keep the connection (and thus the stream) alive until told to stop. For a virtual
/// output this connection IS the output's lifetime; for a mirror it is only the
/// recording's. The only deadline-free [`pump_until`] in the file.
fn park_until_stopped(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    output: &str,
    node_id: u32,
) -> Result<()> {
    match pump_until(conn, queue, state, None, Some(stop), |st| st.closed)? {
        Pumped::Done => {
            tracing::warn!(output = %output, node_id, "KWin closed the screencast stream");
        }
        // `Expired` cannot happen without a deadline; `Stopped` is the ordinary teardown.
        Pumped::Stopped | Pumped::Expired => {}
    }
    Ok(())
}

/// Wait for the `created` event (PipeWire node id), bounded and interruptible by `stop`.
///
/// `started` is when the worker began, not this wait: the bound is the earlier of
/// [`CREATE_BUDGET`] and the opener's deadline, so barriers before us come out of
/// this wait rather than the opener's patience.
fn await_created(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    request: &str,
    started: Instant,
) -> Result<u32> {
    let began = Instant::now();
    let deadline = (began + CREATE_BUDGET).min(started + OPENER_BUDGET - WORKER_MARGIN);
    let settled = |st: &State| st.node_id.is_some() || st.failed.is_some() || st.closed;
    match pump_until(conn, queue, state, Some(deadline), Some(stop), settled)? {
        // Node id first: a `closed` in the same burst as `created` is a stream that
        // was made and then torn down, not a failure to make one.
        Pumped::Done => match (state.node_id, state.failed.take()) {
            (Some(node), _) => Ok(node),
            (None, Some(e)) => bail!("{request} failed: {e}"),
            (None, None) => bail!("KWin closed the stream before it was created"),
        },
        Pumped::Stopped => bail!("{request} abandoned — released before KWin created the stream"),
        // Report the wait we actually got, not the budget — they differ when the
        // opener's deadline was tighter, and naming 15 s after 11 s sends people
        // hunting a stall that never happened.
        Pumped::Expired => bail!(
            "KWin acknowledged {request} but never sent the PipeWire node within {:?}",
            began.elapsed()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{mode_satisfies, to_relight, union_restore, MANAGED_PREFIX};

    /// KWin's stored setup can disable physicals between create and the exclusive
    /// apply's enumeration. The pre-create snapshot must reach the restore list;
    /// what the apply itself disabled keeps its (fresher) entry.
    #[test]
    fn the_restore_list_covers_outputs_kwin_disabled_before_the_apply_saw_them() {
        let pre = vec![
            ("DP-1".to_string(), "2560x1440@144".to_string()),
            ("DP-2".to_string(), "2560x1440@60".to_string()),
            ("DP-3".to_string(), "1920x1080@60".to_string()),
        ];
        // Apply enumerated nothing enabled.
        assert_eq!(union_restore(Vec::new(), &pre), pre);
        // Apply's own capture wins for outputs it saw; the snapshot fills the gaps.
        let seen = vec![("DP-1".to_string(), "2560x1440@120".to_string())];
        let merged = union_restore(seen, &pre);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0], ("DP-1".to_string(), "2560x1440@120".to_string()));
        assert!(merged.contains(&("DP-2".to_string(), "2560x1440@60".to_string())));
        assert!(merged.contains(&("DP-3".to_string(), "1920x1080@60".to_string())));
    }

    /// An Extend create's delayed pass runs after an Exclusive sibling darkened DP-1: it
    /// re-lights only what KWin's stored setup darkened, never a head the sibling holds.
    #[test]
    fn the_delayed_pass_leaves_a_sibling_exclusive_dark() {
        let expected = vec![
            ("DP-1".to_string(), "2560x1440@144".to_string()),
            ("DP-2".to_string(), "1920x1080@60".to_string()),
            ("HDMI-A-1".to_string(), "1920x1080@60".to_string()),
        ];
        let held = ["DP-1".to_string()].into_iter().collect();
        let got = to_relight(&expected, &["DP-1", "DP-2"], &held);
        assert_eq!(got, vec![expected[1].clone()]);
        assert!(to_relight(&expected, &[], &held).is_empty(), "nothing dark");
        let none = Default::default();
        assert_eq!(
            to_relight(&expected, &["DP-1"], &none),
            vec![expected[0].clone()]
        );
    }

    /// A stored 1080p for the output name is not an alignment of a 4K request.
    /// The second pair is the field case: a stored slot streamed for a whole
    /// session because the create path never re-read the recreated output.
    #[test]
    fn a_restored_stored_mode_does_not_pass_for_the_requested_one() {
        assert!(!mode_satisfies((1920, 1080), 3840, 2160));
        assert!(!mode_satisfies((3600, 2260), 1800, 880));
    }

    /// libxcvt rounds a 2868-wide request down to 2864; rejecting it would strand
    /// the output on its birth mode.
    #[test]
    fn a_cvt_aligned_width_still_satisfies_the_request() {
        assert!(mode_satisfies((2864, 1320), 2868, 1320));
        assert!(mode_satisfies((3840, 2160), 3840, 2160)); // exact is the common case
    }

    /// Alignment slack is bounded and one-sided. 8+ px short is a different mode;
    /// a width above the request would underflow the subtraction without `<=`.
    /// Height is never rounded.
    #[test]
    fn the_alignment_slack_is_bounded_one_sided_and_width_only() {
        assert!(mode_satisfies((3833, 2160), 3840, 2160)); // 7 short — inside the grain
        assert!(!mode_satisfies((3832, 2160), 3840, 2160)); // 8 short — a different mode
        assert!(!mode_satisfies((3848, 2160), 3840, 2160)); // wider than asked
        assert!(!mode_satisfies((3840, 2159), 3840, 2160)); // height is never aligned
    }

    /// Exclusive disables only the non-managed panel — never a sibling session's
    /// per-slot output.
    #[test]
    fn exclusive_disables_only_non_managed() {
        let enabled = [
            "Virtual-punktfunk",   // base name (shared identity)
            "Virtual-punktfunk-1", // client A's per-slot output
            "Virtual-punktfunk-7", // client B's per-slot output
            "eDP-1",               // a physical panel
        ];
        let to_disable: Vec<&str> = enabled
            .iter()
            .copied()
            .filter(|n| !n.starts_with(MANAGED_PREFIX))
            .collect();
        assert_eq!(to_disable, vec!["eDP-1"]);
    }

    /// Live mid-stream mode switches: each create runs while its predecessor still holds
    /// the name and KWin's stored mode, as the registry's create-before-drop does. The
    /// 60 Hz steps take the `verify_or_reassert` path. Needs
    /// `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 kwin_wayland --virtual` (6.5.6+) and PipeWire.
    #[test]
    #[ignore = "needs a live KWin and PipeWire; run with --ignored"]
    fn live_kwin_mode_switch_lands_on_the_new_output() {
        use super::{KwinDisplay, Mode, VirtualDisplay};
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("pf_vdisplay=info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
        let modes = [
            (1920, 1080, 120),
            (1280, 720, 120),
            (1920, 1080, 60),
            (1280, 720, 60),
        ];
        let mode = |(width, height, refresh_hz)| Mode {
            width,
            height,
            refresh_hz,
        };
        let name = format!("Virtual-{}", super::VOUT_NAME);
        let mut vd = KwinDisplay::new().unwrap();
        let mut live = vd.create(mode(modes[0])).expect("first create");
        for i in 1..=8 {
            let want = modes[i % modes.len()];
            let next = vd.create(mode(want)).expect("mode-switch create");
            drop(std::mem::replace(&mut live, next));
            // KWin re-applies its stored setup once the predecessor is gone.
            std::thread::sleep(std::time::Duration::from_millis(500));
            let got = crate::kwin::mgmt::actual_dims(crate::kwin::mgmt::watch().as_mut(), &name)
                .map(|(w, h, mhz, _)| (w, h, (mhz + 500) / 1000));
            assert_eq!(got, Some(want), "switch {i}");
        }
    }
}
