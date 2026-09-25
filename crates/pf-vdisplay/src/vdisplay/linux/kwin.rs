//! KWin virtual-output backend via the privileged `zkde_screencast_unstable_v1` protocol.
//!
//! `stream_virtual_output` creates an output at `width`×`height` (native, unscaled) and returns
//! a PipeWire node on the user's default daemon — [`VirtualOutput::remote_fd`] is therefore
//! `None`. The host must run inside the KWin session (`$WAYLAND_DISPLAY`).
//!
//! The global is restricted: KWin advertises it only to a client whose `.desktop` lists it
//! under `X-KDE-Wayland-Interfaces` (matched by `/proc/<pid>/exe` → `Exec=`). Packages ship
//! `io.unom.Punktfunk.Host.desktop` for that. The host binary must carry no file capability and
//! must still be the file at its path ([`screencast_withheld`] names which one broke).
//! Headless tests use `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. `createVirtualOutput` needs the
//! DRM backend, or VirtualBackend since KWin 6.5.6.

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

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
    /// Slot the last [`create`](VirtualDisplay::create) resolved, so the registry keys
    /// arrangement and `/display/state` to the same id this backend named the output with.
    last_slot: Option<u32>,
    /// Resolved kscreen address of the last `create` (numeric id, else `Virtual-<name>`).
    /// [`apply_position`](VirtualDisplay::apply_position) must use this, not the shared name:
    /// a superseded sibling is still alive and would take a name-addressed command.
    last_name: Option<String>,
    /// `kde_output_device_v2` UUID of the last `create`, when in-process topology handled it.
    /// Stable across a supersede (unlike the shared name); preferred over `last_name`.
    our_uuid: Option<String>,
    /// Restore closure for outputs `exclusive` disabled, pending [`take_topology_restore`].
    /// Run when the display group's last member drops, not this session. [`Drop`] is the
    /// backstop if the registry never took it.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// Cursor-channel session: METADATA at creation; otherwise EMBEDDED.
    hw_cursor: bool,
    /// `mode_conflict: join` admitted this session.
    join_live: bool,
    /// `wl_output.name` of the last `create` (`Virtual-<name>`), which a joiner streams.
    join_name: Option<String>,
}

impl Drop for KwinDisplay {
    fn drop(&mut self) {
        // Backstop: the registry normally takes this right after `create`. Run it here if
        // not, so a physical is never left dark.
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl KwinDisplay {
    pub fn new() -> Result<Self> {
        Ok(KwinDisplay::default())
    }

    /// Apply topology for the just-created output `our_prefix` (size `dims`). Prefers
    /// in-process `kde_output_management_v2`; falls back to `kscreen-doctor` if the global
    /// is absent or the compositor misses its budget. Records UUID or kscreen address for
    /// [`apply_position`](VirtualDisplay::apply_position). Returns disabled outputs
    /// (`(name, "WxH@Hz")`) for group teardown; `Extend`/`Auto` disable nothing.
    ///
    /// `pre_enabled` is the non-managed outputs lit before create ([`enabled_physicals`]).
    /// KWin may apply a stored setup for the new monitor set that already has physicals
    /// disabled — so a post-create enumerate can miss them. The snapshot is the only
    /// unpolluted read: `Exclusive` unions it into restore; other topologies
    /// [`reenable_stranded`].
    fn apply_topology(
        &mut self,
        name: &str,
        our_prefix: &str,
        dims: (u32, u32),
        pre_enabled: &[(String, String)],
    ) -> Vec<(String, String)> {
        use crate::kwin_output_mgmt::TopologyKind;
        use crate::policy::Topology;
        // Per device: `set_client_identity` runs before `create`, so this display knows
        // whose it is and can honour that device's own topology (§6.1).
        let topology = crate::effective_topology(self.client_fp);
        let kind = match topology {
            Topology::Exclusive => TopologyKind::Exclusive,
            Topology::Primary => TopologyKind::Primary,
            Topology::Extend | Topology::Auto => {
                // Takes no primary and darkens nothing — but KWin restores a stored setup
                // onto our stable name, so the apply still has to clear a mirror source
                // and keep our origin off a live screen.
                let outcome = crate::kwin_output_mgmt::apply_topology(
                    our_prefix,
                    dims.0,
                    dims.1,
                    TopologyKind::Extend,
                );
                if outcome.handled {
                    self.our_uuid = outcome.our_uuid;
                } else {
                    crate::kwin_output_mgmt::clear_replication_source(our_prefix, dims.0, dims.1);
                }
                // These topologies promise physicals stay lit; undo KWin switching them off
                // in reaction to our output appearing.
                reenable_stranded(pre_enabled.to_vec());
                return Vec::new();
            }
        };
        // In-process Wayland; immune to a wedged kscreen-doctor.
        let outcome = crate::kwin_output_mgmt::apply_topology(our_prefix, dims.0, dims.1, kind);
        if outcome.handled {
            self.our_uuid = outcome.our_uuid;
            if kind == TopologyKind::Primary {
                // `Primary` keeps physicals enabled — undo stored-config disable as Extend
                // does. Nothing to restore at teardown.
                reenable_stranded(pre_enabled.to_vec());
                return outcome.disabled;
            }
            return union_restore(outcome.disabled, pre_enabled);
        }
        tracing::info!(
            "KWin topology: kde_output_management unavailable — kscreen-doctor fallback"
        );
        let addr = resolve_kscreen_addr(name, dims.0, dims.1);
        self.last_name = Some(addr.clone());
        match topology {
            Topology::Exclusive => union_restore(apply_virtual_primary(&addr), pre_enabled),
            Topology::Primary => {
                apply_virtual_primary_only(&addr);
                reenable_stranded(pre_enabled.to_vec());
                Vec::new()
            }
            Topology::Extend | Topology::Auto => Vec::new(),
        }
    }
}

impl VirtualDisplay for KwinDisplay {
    fn name(&self) -> &'static str {
        "kwin"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
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

    fn apply_position(&mut self, x: i32, y: i32) {
        // Address OUR output by its stable UUID over kde_output_management_v2. A name
        // would hit a superseded sibling still alive.
        if let Some(uuid) = self.our_uuid.clone() {
            if crate::kwin_output_mgmt::set_position(&uuid, x, y) {
                return;
            }
        }
        // `last_name` is the resolved kscreen address. Never re-derive from the name:
        // during a supersede two outputs share it and the command hits the old one.
        let Some(output) = self.last_name.clone() else {
            return;
        };
        // kscreen-doctor: `output.<name-or-id>.position.<x>,<y>`.
        let ok = kscreen_ok(&[format!("output.{output}.position.{x},{y}")]);
        if ok {
            tracing::info!(output, x, y, "KWin: placed output in the desktop layout");
        } else {
            tracing::warn!(output, x, y, "KWin: output position apply failed");
        }
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
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
        // A supersede keeps this `KwinDisplay` while the predecessor is still alive.
        // A stale UUID still resolves: `set_position` would move the old output and
        // report success. Re-set below only if the in-process path handles us.
        self.our_uuid = None;
        let (width, height) = (mode.width, mode.height);
        let pointer_mode = if self.hw_cursor {
            POINTER_METADATA
        } else {
            POINTER_EMBEDDED
        };
        let spawn_vout = |w: u32, h: u32| -> Result<(u32, Arc<AtomicBool>)> {
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
                Err(_) => {
                    // `StopGuard` is only built on success, so nothing else will flip
                    // `stop`. The worker is still inside `await_created` holding a
                    // half-built output KWin keeps alive for this connection.
                    stop.store(true, Ordering::Relaxed);
                    bail!("timed out creating the KWin virtual output")
                }
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
            let at = crate::kwin_output_mgmt::actual_dims(&our_prefix)
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
            // Install+select the high-refresh custom mode. In-process first; kscreen-doctor
            // if KWin has no `set_custom_modes` or misses its budget.
            let active = crate::kwin_output_mgmt::set_custom_mode(
                &our_prefix,
                at.0,
                at.1,
                width,
                height,
                mode.refresh_hz,
            )
            .or_else(|| {
                // Address by numeric kscreen id, never by name: a supersede reuses the
                // per-slot name while the sibling is still alive, so a name hits the old one.
                let addr = resolve_kscreen_addr(&name, at.0, at.1);
                self.last_name = Some(addr.clone());
                set_custom_refresh(width, height, mode.refresh_hz, &addr)
            });
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
                    stop.store(true, Ordering::Relaxed);
                    // Let KWin retire the doomed output before re-using its name.
                    std::thread::sleep(Duration::from_millis(300));
                    let (nid, st) = spawn_vout(width, height)?;
                    node_id = nid;
                    stop = st;
                    tracing::info!(
                        node_id,
                        width,
                        height,
                        "KWin virtual output ready (fallback)"
                    );
                    // The recreate takes the same stored slot as a ≤60 Hz create. Unverified,
                    // the session streams whatever `kwinoutputconfig.json` restored.
                    let (dims, exact) = verify_or_reassert(&our_prefix, width, height);
                    (dims, exact, 60)
                }
            }
        } else {
            // ≤60 Hz installs no mode, so nothing here learned what KWin built.
            let (dims, exact) = verify_or_reassert(&our_prefix, width, height);
            (dims, exact, mode.refresh_hz)
        };
        let disabled = self.apply_topology(&name, &our_prefix, final_dims, &pre_enabled);
        // Stash restore on the group, not this session's keepalive: a per-session
        // `StopGuard` would re-enable physicals when the FIRST exclusive member drops
        // under a still-live sibling. Empty ⇒ nothing to restore.
        let prepared = (!disabled.is_empty()).then(|| {
            let disabled = disabled.clone();
            // In-process first; kscreen-doctor if the compositor misses its budget.
            // Both must return honest verdicts — a fallback that acks unchecked
            // re-introduces silent success.
            Box::new(move || {
                if !crate::kwin_output_mgmt::reenable_outputs(&disabled) {
                    reenable_outputs_kscreen(&disabled);
                }
                // This ran under the with-us monitor set (before reclaim). Reclaim
                // flips KWin to the without-us set, whose stored setup can disable
                // physicals again. One delayed re-assert after reclaim persists as
                // a user-applied config. One shot, never a loop.
                let verify = disabled.clone();
                std::thread::Builder::new()
                    .name("punktfunk-kwin-restore-verify".into())
                    .spawn(move || {
                        std::thread::sleep(STRAND_RECHECK_DELAY);
                        reenable_pass(&verify, "post-teardown", true);
                    })
                    .ok();
            }) as Box<dyn FnOnce() + Send>
        });
        // Keep the first restore. The registry drains this after every `create`, but
        // a retry-loop create must not overwrite a held restore.
        crate::backend::stash_topology_restore(&mut self.pending_restore, prepared);
        let mut out = VirtualOutput::owned(
            node_id,
            Some((final_dims.0, final_dims.1, achieved_hz)),
            Box::new(StopGuard { stop }),
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
    crate::kwin_output_mgmt::list_monitors()
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
    std::thread::Builder::new()
        .name("punktfunk-kwin-reenable".into())
        .spawn(move || {
            std::thread::sleep(STRAND_RECHECK_DELAY);
            reenable_pass(&pre_enabled, "delayed", false);
        })
        .ok();
}

/// One pass: re-enable `expected ∩ now-disabled`. `abort_if_managed` is post-teardown:
/// a managed output by then is a new session that already owns topology — lighting
/// physicals under its `exclusive` would undo it.
fn reenable_pass(expected: &[(String, String)], wave: &'static str, abort_if_managed: bool) {
    let Ok(now) = crate::kwin_output_mgmt::list_monitors() else {
        return;
    };
    if abort_if_managed && now.iter().any(|m| m.managed) {
        return;
    }
    let dark: Vec<(String, String)> = expected
        .iter()
        .filter(|(name, _)| now.iter().any(|m| &m.connector == name && !m.enabled))
        .cloned()
        .collect();
    if dark.is_empty() {
        return;
    }
    tracing::warn!(
        outputs = ?dark,
        wave,
        "KWin's stored output setup (kwinoutputconfig.json) left physical output(s) disabled that \
         the current topology says stay enabled — re-enabling them"
    );
    if !crate::kwin_output_mgmt::reenable_outputs(&dark) {
        reenable_outputs_kscreen(&dark);
    }
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

/// Re-enable outputs `exclusive` disabled, via `kscreen-doctor` — fallback when
/// [`crate::kwin_output_mgmt::reenable_outputs`] reports the compositor did not answer.
/// The registry runs this when the group's last member drops, before reclaiming that
/// member's output, so KWin never sees zero enabled outputs.
///
/// A helper killed at [`KSCREEN_BUDGET`] is not a refusal: kscreen-doctor applies, then
/// waits on the compositor, so a slow KWin can land the enable and still get killed.
/// Treat `None` as "unknown" and continue to the mode re-assert and the settle.
fn reenable_outputs_kscreen(outputs: &[(String, String)]) {
    if outputs.is_empty() {
        return;
    }
    // Enable first, alone: a bare `output.X.enable` always succeeds. Batching a stale
    // `mode` into the same invoke can reject the whole config and leave the output dark.
    let enable_args: Vec<String> = outputs
        .iter()
        .map(|(name, _)| format!("output.{name}.enable"))
        .collect();
    let enable_verdict = kscreen_verdict(&enable_args);
    match enable_verdict {
        // Both in-process and this path declined. Stop: further mode/settle work cannot
        // light a refused enable.
        Some(false) => {
            tracing::error!(
                outputs = ?outputs,
                args = ?enable_args,
                "KWin: the physical/bootstrap outputs did not re-enable (kscreen-doctor refused \
                 the config or would not run, after the in-process restore already declined) — \
                 a monitor may be left dark"
            );
            return;
        }
        // Budget kill ≠ refusal. kscreen-doctor applies then waits on KWin, so a loaded
        // compositor can land the enable and still get killed. Returning here would skip
        // the mode re-assert (120 Hz comes back at EDID ~60 Hz) and the 200 ms settle
        // that keeps KWin from seeing zero outputs when the caller reclaims next.
        None => tracing::warn!(
            outputs = ?outputs,
            args = ?enable_args,
            "KWin: kscreen-doctor was killed at its budget re-enabling the physical/bootstrap \
             outputs — the apply may well have landed, so continuing with the mode restore"
        ),
        Some(true) => {}
    }
    // Then re-assert each captured mode. A bare enable falls back to EDID-preferred
    // (~60 Hz on a 120 Hz panel). A rejected mode is a wrong refresh, not a dark screen.
    let mode_args: Vec<String> = outputs
        .iter()
        .filter(|(_, mode)| !mode.is_empty())
        .map(|(name, mode)| format!("output.{name}.mode.{mode}"))
        .collect();
    let modes_restored = mode_args.is_empty() || kscreen_ok(&mode_args);
    std::thread::sleep(Duration::from_millis(200));
    // After a budget kill the enable is probable, not established; the log must say which.
    let enable_confirmed = enable_verdict == Some(true);
    if modes_restored {
        tracing::info!(reenabled = ?outputs, enable_confirmed, "KWin: restored the physical/bootstrap outputs at their captured modes (group empty)");
    } else {
        tracing::warn!(
            reenabled = ?outputs,
            args = ?mode_args,
            enable_confirmed,
            "KWin: re-enabled the physical/bootstrap outputs but could not re-assert their captured \
             modes — they are back at KWin's preferred refresh, not the one they were streaming at"
        );
    }
}

/// kscreen address of the output just created. The managed-prefix name is ambiguous
/// during a supersede (the replacement reuses the per-slot name while the sibling is
/// still alive), so match birth size too — only the new output sits at sacrificial
/// `(w, h)` — and prefer the highest id. Falls back to `Virtual-<name>`.
fn resolve_kscreen_addr(name: &str, w: u32, h: u32) -> String {
    let fallback = format!("Virtual-{name}");
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        let Some(doc) = kscreen_json() else { continue };
        let Some(outputs) = doc.get("outputs").and_then(|o| o.as_array()) else {
            continue;
        };
        let best = outputs
            .iter()
            .filter(|o| {
                o.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(&fallback))
                    && output_active_size(o) == Some((w, h))
            })
            .filter_map(|o| o.get("id").and_then(|i| i.as_u64()))
            .max();
        if let Some(id) = best {
            tracing::info!(id, name, w, h, "KWin: resolved the new output's kscreen id");
            return id.to_string();
        }
    }
    tracing::warn!(
        name,
        w,
        h,
        "KWin: could not resolve the new output's kscreen id — falling back to name addressing \
         (ambiguous during a mode-switch supersede)"
    );
    fallback
}

/// Budget for one `kscreen-doctor` call. It is a Wayland client of the compositor it
/// configures, so a wedged KWin blocks in connect forever — and these calls run on
/// the session's stream thread. Healthy calls are tens of ms.
const KSCREEN_BUDGET: Duration = Duration::from_secs(5);

/// `kscreen-doctor <args>` for exit status, bounded by [`KSCREEN_BUDGET`]. A timeout
/// is a failed apply.
fn kscreen_ok(args: &[String]) -> bool {
    kscreen_verdict(args) == Some(true)
}

/// Same call, keeping the outcome [`kscreen_ok`]'s `bool` throws away.
///
/// `Some(true)`/`Some(false)`: ran to completion and accepted / refused (spawn failure
/// is a refusal). `None`: killed at [`KSCREEN_BUDGET`]. kscreen-doctor applies then
/// waits on the compositor, so a slow KWin yields a kill on a request that already
/// landed; treating `None` as failure costs a monitor its refresh on the restore path.
pub(crate) fn kscreen_verdict(args: &[String]) -> Option<bool> {
    match crate::proc::status_within(
        std::process::Command::new("kscreen-doctor").args(args),
        KSCREEN_BUDGET,
    ) {
        Ok(status) => Some(status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
        Err(_) => Some(false),
    }
}

fn kscreen_json_bytes() -> Option<Vec<u8>> {
    crate::proc::output_within(
        std::process::Command::new("kscreen-doctor").arg("-j"),
        KSCREEN_BUDGET,
    )
    .ok()
    .map(|o| o.stdout)
}

fn kscreen_json() -> Option<serde_json::Value> {
    serde_json::from_slice(&kscreen_json_bytes()?).ok()
}

/// Current mode from a `kscreen-doctor -j` entry: `(width, height, refresh_mHz)`.
/// No current mode or no size → `None`. Missing `refreshRate` reports 0 mHz.
fn output_active_mode(o: &serde_json::Value) -> Option<(u32, u32, u32)> {
    let current = o.get("currentModeId").and_then(json_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(json_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    // Hz → mHz without an intermediate round: `refreshRate` is a float (59.94, 119.92).
    // Whole Hz would throw away the distinction `PhysicalMonitor::refresh_mhz` keeps.
    let mhz = mode
        .get("refreshRate")
        .and_then(|r| r.as_f64())
        .map(|hz| (hz * 1000.0).round().max(0.0) as u32)
        .unwrap_or(0);
    Some((w, h, mhz))
}

fn output_active_size(o: &serde_json::Value) -> Option<(u32, u32)> {
    output_active_mode(o).map(|(w, h, _)| (w, h))
}

/// Every head KWin reports, for [`crate::monitors::list`] — in-process enumerate
/// ([`crate::kwin_output_mgmt::list_monitors`]) with a `kscreen-doctor -j` fallback.
///
/// A failed `list` is not "no monitors": `monitors::resolve` treats a miss as a hard
/// error, so the picker and `PUNKTFUNK_CAPTURE_MONITOR` would refuse to start.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let declined = match crate::kwin_output_mgmt::list_monitors() {
        Ok(monitors) => return Ok(monitors),
        Err(e) => e,
    };
    let Some(doc) = kscreen_json() else {
        return Err(declined.context(
            "kscreen-doctor -j did not answer either (not installed, or killed at its budget)",
        ));
    };
    let monitors = monitors_from_kscreen_json(&doc);
    tracing::info!(
        count = monitors.len(),
        reason = %declined,
        "KWin: enumerated monitors via kscreen-doctor (in-process output management declined)"
    );
    Ok(monitors)
}

/// Parse `kscreen-doctor -j` into the shared monitor type. Split from the process
/// call so the mapping can be tested against captured JSON. Mirrors the in-process
/// contract: disabled outputs report zeroed geometry, `primary` accepts `priority: 1`
/// or `primary: true`, list sorted by desktop position.
fn monitors_from_kscreen_json(doc: &serde_json::Value) -> Vec<crate::monitors::PhysicalMonitor> {
    let Some(outputs) = doc.get("outputs").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<crate::monitors::PhysicalMonitor> = outputs
        .iter()
        .filter_map(|o| {
            let connector = o.get("name").and_then(|n| n.as_str())?.to_string();
            let mode = output_active_mode(o);
            let coord = |k: &str| {
                o.get("pos")
                    .and_then(|p| p.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32
            };
            Some(crate::monitors::PhysicalMonitor {
                managed: connector.starts_with(MANAGED_PREFIX),
                description: crate::monitors::describe(
                    o.get("vendor").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    &connector,
                ),
                width: mode.map(|m| m.0).unwrap_or(0),
                height: mode.map(|m| m.1).unwrap_or(0),
                refresh_mhz: mode.map(|m| m.2).unwrap_or(0),
                x: coord("x"),
                y: coord("y"),
                scale: o
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1),
                enabled: o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    out
}

/// CVT horizontal cell grain. KWin times custom modes with libxcvt, whose first step
/// is `hdisplay_rnd = hdisplay - (hdisplay % 8)` — a width not a multiple of 8 comes
/// back narrower, and clock-step rounding lands a fractional refresh (2868×1320@120
/// → 2864×1320@119.92). Never select a custom mode by the requested `WxH@Hz` string:
/// kscreen-doctor `findMode` matches id or `WxH@qRound(Hz)`, so the request matches
/// nothing and the output stays on its sacrificial birth. Shared with
/// [`crate::kwin_output_mgmt`].
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
/// client never asked for.
fn verify_or_reassert(our_prefix: &str, width: u32, height: u32) -> ((u32, u32), bool) {
    match crate::kwin_output_mgmt::actual_dims(our_prefix) {
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
            match crate::kwin_output_mgmt::set_custom_mode(our_prefix, aw, ah, width, height, 60) {
                Some((cw, ch, _)) if mode_satisfies((cw, ch), width, height) => {
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

#[derive(Clone, Debug, PartialEq)]
struct KModeRow {
    /// kscreen mode id — address by this, never the requested `WxH@Hz` string.
    id: String,
    w: u32,
    h: u32,
    hz: f64,
}

/// A kscreen JSON id, which is a string on some KWin versions and a number on others.
fn json_id(v: &serde_json::Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.as_u64().map(|n| n.to_string()))
}

/// Full mode list of `output` (resolved kscreen address) from parsed `-j` JSON.
/// Split from the process call so the picker can be tested on captured JSON.
fn modes_from_json(doc: &serde_json::Value, output: &str) -> Vec<KModeRow> {
    let Some(o) = doc
        .get("outputs")
        .and_then(|v| v.as_array())
        .and_then(|outs| {
            outs.iter().find(|o| {
                o.get("name").and_then(|n| n.as_str()) == Some(output)
                    || o.get("id").and_then(json_id).as_deref() == Some(output)
            })
        })
    else {
        return Vec::new();
    };
    o.get("modes")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let size = m.get("size")?;
                    Some(KModeRow {
                        id: m.get("id").and_then(json_id)?,
                        w: size.get("width").and_then(|v| v.as_u64())? as u32,
                        h: size.get("height").and_then(|v| v.as_u64())? as u32,
                        hz: m.get("refreshRate").and_then(|r| r.as_f64())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn output_modes(output: &str) -> Vec<KModeRow> {
    kscreen_json()
        .map(|doc| modes_from_json(&doc, output))
        .unwrap_or_default()
}

/// Mode in `modes` that fulfils `width`×`height`@`hz`, allowing CVT alignment
/// ([`CVT_H_GRANULARITY`]): exact height, width up to one cell narrower (never
/// wider), refresh within 1 Hz (excludes the native 60 Hz entry). Widest then
/// fastest, so an exact-width mode beats an aligned one.
fn pick_custom_mode(modes: &[KModeRow], width: u32, height: u32, hz: u32) -> Option<&KModeRow> {
    modes
        .iter()
        .filter(|m| {
            m.h == height
                && m.w <= width
                && width - m.w < CVT_H_GRANULARITY
                && (m.hz - f64::from(hz)).abs() < 1.0
        })
        .max_by(|a, b| {
            a.w.cmp(&b.w)
                .then(a.hz.partial_cmp(&b.hz).unwrap_or(std::cmp::Ordering::Equal))
        })
}

/// Install + select the `width`×`height`@`hz` custom mode via `kscreen-doctor`
/// (`output` is the resolved kscreen address), then read back the active mode as
/// `(width, height, refresh_hz)`. `None` if read-back failed.
///
/// The apply can report success yet leave the old mode; the caller drives the
/// pipeline off the achieved mode. Select by kscreen mode id, never the requested
/// `WxH@Hz` string ([`CVT_H_GRANULARITY`]).
fn set_custom_refresh(width: u32, height: u32, hz: u32, output: &str) -> Option<(u32, u32, u32)> {
    let output = output.to_string();
    let mhz = hz.saturating_mul(1000);
    let run = |arg: String| kscreen_ok(&[arg]);
    // Install only if no usable mode exists: kscreen-doctor appends, and KWin persists
    // the list per output name, so re-adding every connect would grow the display list.
    let mut modes = output_modes(&output);
    if pick_custom_mode(&modes, width, height, hz).is_none() {
        let _ = run(format!(
            "output.{output}.addCustomMode.{width}.{height}.{mhz}.full"
        ));
        modes = output_modes(&output);
    }
    let applied = match pick_custom_mode(&modes, width, height, hz) {
        Some(target) => {
            if (target.w, target.h) != (width, height) {
                tracing::info!(
                    output,
                    requested_w = width,
                    requested_h = height,
                    mode_w = target.w,
                    mode_h = target.h,
                    mode_hz = target.hz,
                    "KWin aligned the custom mode to the CVT cell grain — streaming at its size"
                );
            }
            // By id first; the human `WxH@Hz` (from the mode's own size, not the request)
            // is fallback for builds whose ids don't round-trip through the CLI.
            run(format!("output.{output}.mode.{}", target.id))
                || run(format!(
                    "output.{output}.mode.{}x{}@{}",
                    target.w,
                    target.h,
                    target.hz.round() as u32
                ))
        }
        None => {
            tracing::warn!(
                output,
                requested_w = width,
                requested_h = height,
                requested_hz = hz,
                offered = ?modes,
                "KWin offers no mode matching the request after addCustomMode — is kscreen-doctor \
                 up to date, and KWin ≥ 6.6 (custom modes on virtual outputs)?"
            );
            false
        }
    };
    match read_active_mode(&output) {
        Some((w, h, achieved)) => {
            if !mode_satisfies((w, h), width, height) {
                tracing::warn!(
                    output,
                    requested_w = width,
                    requested_h = height,
                    active_w = w,
                    active_h = h,
                    achieved,
                    applied,
                    "KWin kept a mode that is not the one selected — a stored \
                     kwinoutputconfig.json slot for this output name, not a CVT alignment"
                );
            } else if achieved >= hz {
                tracing::info!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    "KWin virtual output: custom mode applied"
                );
            } else {
                tracing::warn!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    applied,
                    "KWin virtual output mode below requested — pacing the encoder to the \
                     achieved rate (custom-mode install rejected? is kscreen-doctor up to date?)"
                );
            }
            Some((w, h, achieved.max(1)))
        }
        None => {
            tracing::warn!(
                output,
                requested = hz,
                applied,
                "could not read back KWin virtual output refresh — assuming 60 Hz (is \
                 kscreen-doctor installed?)"
            );
            None
        }
    }
}

/// Active mode `(width, height, refresh_hz)` of `output` (resolved kscreen address)
/// from `kscreen-doctor -j`. Mode/output ids are strings or numbers depending on
/// KWin version; both are accepted.
fn read_active_mode(output: &str) -> Option<(u32, u32, u32)> {
    let doc = kscreen_json()?;
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let o = doc.get("outputs")?.as_array()?.iter().find(|o| {
        o.get("name").and_then(|n| n.as_str()) == Some(output)
            || o.get("id").and_then(as_id).as_deref() == Some(output)
    })?;
    let current = o.get("currentModeId").and_then(as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(as_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?;
    Some((w, h, hz.round() as u32))
}

/// Prefix every managed KWin output shares (`Virtual-punktfunk` / `Virtual-punktfunk-<id>`).
/// Group membership is this prefix so we never thread the live set through the backend.
/// Shared with [`crate::kwin_output_mgmt`]: a drift would let the in-process path disable
/// a sibling session's output that the kscreen path spares.
pub(crate) const MANAGED_PREFIX: &str = "Virtual-punktfunk";

/// Current mode as a kscreen-doctor setter. Prefer human `WxH@Hz` (survives mode-id
/// re-enumeration across disable→enable); fall back to raw `currentModeId`.
fn output_current_mode_spec(o: &serde_json::Value) -> Option<String> {
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let current = o.get("currentModeId").and_then(&as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(&as_id).as_deref() == Some(current.as_str()))?;
    let human = (|| {
        let size = mode.get("size")?;
        let w = size.get("width").and_then(|v| v.as_u64())?;
        let h = size.get("height").and_then(|v| v.as_u64())?;
        let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?.round() as u64;
        Some(format!("{w}x{h}@{hz}"))
    })();
    Some(human.unwrap_or(current))
}

/// Currently-enabled outputs that are not ours — bootstrap + physical, i.e. what
/// `exclusive` must disable — each paired with its current mode so teardown restores
/// that refresh (a bare enable drops 120 Hz to ~60). Excludes the whole
/// [`MANAGED_PREFIX`] family so a second exclusive session never disables the first.
fn other_enabled_outputs() -> Vec<(String, String)> {
    let out = match kscreen_json_bytes() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
                .filter_map(|o| {
                    let name = o.get("name").and_then(|n| n.as_str())?;
                    (!name.starts_with(MANAGED_PREFIX)).then(|| {
                        (
                            name.to_string(),
                            output_current_mode_spec(o).unwrap_or_default(),
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True if any managed group member is already KWin primary — first-slot-wins, so a
/// later exclusive session doesn't steal the shell. No primary flag → `false` (we then
/// set ourselves). Accepts `"priority": 1` or `"primary": true`.
fn a_managed_output_is_primary() -> bool {
    let Some(out) = kscreen_json_bytes() else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out) else {
        return false;
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter().any(|o| {
                let managed = o
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
                let primary = o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1);
                managed && primary
            })
        })
        .unwrap_or(false)
}

/// Set our output primary and disable bootstrap/physicals so the managed group is the
/// sole desktop. `ours` is the resolved kscreen address. Returns disabled outputs for
/// teardown restore. On failure, streaming continues rather than failing the session.
fn apply_virtual_primary(ours: &str) -> Vec<(String, String)> {
    let kscreen = |args: &[String]| kscreen_ok(args);
    // First-slot-wins: only grab primary if no managed member has it, so a second
    // exclusive session joins as a secondary instead of stealing the shell.
    let sole = !a_managed_output_is_primary();
    if sole {
        if !kscreen(&[format!("output.{ours}.primary")]) {
            tracing::warn!(
                "KWin: virtual output not made primary — the client may see only the wallpaper"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Disable still-enabled non-managed outputs (bootstrap / physical). Capture each
    // with its current mode so teardown restores the real refresh.
    let others = other_enabled_outputs();
    let mut args: Vec<String> = others
        .iter()
        .map(|(o, _mode)| format!("output.{o}.disable"))
        .collect();
    // Ours ends up the whole desktop, so it belongs at the desktop origin: KWin parks
    // a new output right of the existing row and never re-normalizes once the row goes
    // dark, and Plasma places popups off a screen whose origin is not zero.
    if sole {
        args.push(format!("output.{ours}.position.0,0"));
    }
    if args.is_empty() {
        return others;
    }
    if kscreen(&args) {
        tracing::info!(also_disabled = ?others, "KWin: streamed output set as the sole desktop");
    } else {
        // Report the request, not success: the outputs may still be enabled. Return
        // them for restore anyway — re-enabling a never-disabled output is a no-op,
        // and dropping them here would strand a physical if the disable landed and
        // only the ack was lost to the budget.
        tracing::warn!(
            attempted_disable = ?others,
            "KWin: could not disable the other outputs for the exclusive topology (kscreen-doctor \
             failed or hit its budget) — the streamed output is not the sole desktop"
        );
    }
    others
}

/// Make the streamed output primary but keep other outputs enabled. Nothing to restore.
fn apply_virtual_primary_only(ours: &str) {
    let ok = kscreen_ok(&[format!("output.{ours}.primary")]);
    if ok {
        tracing::info!("KWin: streamed output set primary (physical outputs kept)");
    } else {
        tracing::warn!("KWin: virtual output not made primary");
    }
}

/// Dropping this releases the KWin virtual output: it flips the keepalive thread's
/// `stop`, which drops the Wayland connection. Topology restore lives on the registry
/// group and runs when the last member drops, before this keepalive is dropped.
struct StopGuard {
    stop: Arc<AtomicBool>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

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
    let node_id = match setup_rx.recv_timeout(OPENER_BUDGET) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("KWin monitor mirror failed: {e}"),
        Err(_) => {
            // Same leak as the virtual-output opener: `StopOnDrop` only owns `stop` on
            // success, so without this the mirror thread keeps recording until its budget.
            stop.store(true, Ordering::Relaxed);
            bail!("timed out recording the KWin output {connector:?}")
        }
    };
    Ok(crate::mirror::MirrorStream {
        node_id,
        // KWin publishes on the user's own PipeWire daemon — no portal remote to carry.
        remote_fd: None,
        // Not an xdg-portal session: the `zkde_screencast` pointer mode was asked of
        // KWin directly, so the request is the answer.
        cursor_mode: None,
        keepalive: Box::new(StopOnDrop(stop)),
    })
}

/// Stops the mirror thread (and thus the recording) when the capturer drops it.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
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
    let replaced = !exe.as_os_str().is_empty()
        && std::fs::metadata("/proc/self/exe").is_ok_and(|run| replaced_on_disk(&exe, &run));
    let permitted = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| permitted_caps_from_status(&status));
    anyhow!(
        "{WITHHELD} ({}) — KWin grants it only to a binary whose path is the Exec= of an \
         installed .desktop listing it in X-KDE-Wayland-Interfaces \
         (io.unom.Punktfunk.Host.desktop){}",
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

/// Whether the file at `path` is no longer the `running` binary: a package update
/// swaps the inode, a sysext refresh swaps the mount, a deleted path is gone.
fn replaced_on_disk(path: &std::path::Path, running: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    !std::fs::metadata(path)
        .is_ok_and(|now| (now.dev(), now.ino()) == (running.dev(), running.ino()))
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

    /// A package manager renames a new file over the path; the kernel suffixes a
    /// deleted exe's link with ` (deleted)`. Both must read as replaced.
    #[test]
    fn spots_a_binary_swapped_under_the_process() {
        let dir = std::env::temp_dir().join(format!("pf-kwin-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("punktfunk-host");
        std::fs::write(&path, b"old").unwrap();
        let running = std::fs::metadata(&path).unwrap();
        assert!(!replaced_on_disk(&path, &running));

        let staged = dir.join("punktfunk-host.new");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::rename(&staged, &path).unwrap();
        assert!(replaced_on_disk(&path, &running));
        assert!(replaced_on_disk(
            &dir.join("punktfunk-host (deleted)"),
            &running
        ));
        std::fs::remove_dir_all(&dir).unwrap();
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
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
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
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
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
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
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
            match crate::kwin_output_mgmt::enable_disabled_output(&format!("Virtual-{name}")) {
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

/// Poll slice while waiting on the Wayland fd — granularity at which `stop` and a
/// deadline are observed (matches `kwin_output_mgmt`'s `POLL_MS`).
const POLL_MS: i32 = 200;

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

enum Pumped {
    Done,
    /// `stop` was set — the caller's output/recording was released while we waited.
    Stopped,
    Expired,
}

/// Bounded event loop: dispatch, poll the connection fd up to [`POLL_MS`], read,
/// until `done`, `stop`, or `deadline`.
///
/// `blocking_dispatch` and `roundtrip` cannot be interrupted and have no ceiling.
/// `deadline: None` is correct only for [`park_until_stopped`], where the wait IS
/// the output's lifetime.
fn pump_until(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    deadline: Option<Instant>,
    stop: &AtomicBool,
    done: impl Fn(&State) -> bool,
) -> Result<Pumped> {
    loop {
        queue.dispatch_pending(state).context("dispatch_pending")?;
        if done(state) {
            return Ok(Pumped::Done);
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(Pumped::Stopped);
        }
        let timeout = match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(Pumped::Expired);
                }
                (remaining.as_millis() as i64).clamp(0, i64::from(POLL_MS)) as i32
            }
            None => POLL_MS,
        };
        conn.flush().context("wayland flush")?;
        let Some(guard) = conn.prepare_read() else {
            continue; // events already queued — loop dispatches them
        };
        let mut pfd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `&mut pfd` points at a single live, fully-initialized `libc::pollfd` on the stack, and
        // the count `1` matches that one-element array, so `poll` reads `fd`/`events` and writes `revents`
        // strictly within `pfd`. `pfd.fd` is the Wayland connection's fd, valid because `conn` (and the
        // `prepare_read` guard) are alive across the call. `poll` blocks up to `timeout` ms and writes
        // only `revents`; `pfd` outlives the synchronous call and aliases nothing (a fresh local).
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = guard.read();
        } // else: timeout or signal — drop the guard, re-check `stop` and the deadline
    }
}

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
    let qh = queue.handle();
    let _cb = conn.display().sync(&qh, serial);
    let deadline = Instant::now() + ROUNDTRIP_BUDGET;
    match pump_until(conn, queue, state, Some(deadline), stop, |st| {
        st.sync_done >= serial
    })? {
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
    match pump_until(conn, queue, state, None, stop, |st| st.closed)? {
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
    match pump_until(conn, queue, state, Some(deadline), stop, settled)? {
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
    use super::{
        mode_satisfies, modes_from_json, monitors_from_kscreen_json, pick_custom_mode,
        union_restore, KModeRow, MANAGED_PREFIX,
    };

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

    fn row(id: &str, w: u32, h: u32, hz: f64) -> KModeRow {
        KModeRow {
            id: id.to_string(),
            w,
            h,
            hz,
        }
    }

    /// libxcvt rounds 2868×1320@120 to 2864×1320@119.92. Selecting by the requested
    /// `WxH@Hz` string matches nothing; the picker must find the aligned mode.
    #[test]
    fn picks_the_cvt_aligned_mode() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // the virtual output's native/birth mode
            row("2", 2864, 1320, 119.92), // the custom mode KWin actually generated
        ];
        let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("CVT-aligned mode");
        assert_eq!((got.id.as_str(), got.w, got.h), ("2", 2864, 1320));
    }

    /// A width already on the cell grain round-trips; an exact-width mode outranks
    /// an aligned one when both are offered.
    #[test]
    fn exact_width_outranks_an_aligned_one() {
        let modes = [
            row("1", 2560, 1440, 60.0),
            row("2", 2552, 1440, 119.93), // a stale narrower custom mode from an earlier session
            row("3", 2560, 1440, 119.98),
        ];
        let got = pick_custom_mode(&modes, 2560, 1440, 120).expect("exact mode");
        assert_eq!(got.id, "3");
    }

    /// Never wander onto the native 60 Hz entry, a different height, a wider width,
    /// or a mode more than one cell narrower than asked.
    #[test]
    fn rejects_modes_that_are_not_the_request() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // native — refresh too far off
            row("2", 2868, 1080, 119.92), // wrong height
            row("3", 2880, 1320, 119.92), // wider than requested
            row("4", 2856, 1320, 119.92), // two cells narrower — not a CVT alignment of 2868
            row("5", 1920, 1080, 120.0),  // unrelated
        ];
        assert!(pick_custom_mode(&modes, 2868, 1320, 120).is_none());
    }

    /// Mode + output ids are JSON strings on some KWin versions and numbers on others;
    /// both must parse. A row missing size/refresh is skipped, not a poisoned list.
    #[test]
    fn parses_both_id_encodings() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":7,"name":"Virtual-punktfunk","modes":[
                   {"id":"m1","size":{"width":2868,"height":1320},"refreshRate":60.0},
                   {"id":42,"size":{"width":2864,"height":1320},"refreshRate":119.92},
                   {"id":"broken","size":{"width":800}}
                 ]},
                 {"id":1,"name":"eDP-1","modes":[
                   {"id":"x","size":{"width":2864,"height":1320},"refreshRate":119.92}
                 ]}
               ]}"#,
        )
        .expect("fixture parses");
        // Addressable by numeric id (how `resolve_kscreen_addr` returns it) and by name.
        for addr in ["7", "Virtual-punktfunk"] {
            let modes = modes_from_json(&doc, addr);
            assert_eq!(modes.len(), 2, "the malformed row is dropped ({addr})");
            assert_eq!(modes[1].id, "42", "numeric mode ids stringify ({addr})");
            let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("aligned mode");
            assert_eq!(got.id, "42");
        }
        // Never reads another output's list (the eDP-1 entry carries a matching mode).
        assert!(modes_from_json(&doc, "Virtual-nope").is_empty());
    }

    /// kscreen fallback for `monitors::list` matches the in-process contract: geometry
    /// from `pos`, mode in pixels with refresh in mHz (59.94 ≠ 60), a disabled head
    /// listed but zeroed, our managed output flagged, list sorted by position.
    #[test]
    fn parses_a_kscreen_monitor_list() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":2,"name":"HDMI-A-1","enabled":true,"priority":2,"scale":1,
                  "pos":{"x":1920,"y":0},"vendor":"ACME","model":"U2720Q",
                  "currentModeId":"m9","modes":[
                    {"id":"m9","size":{"width":1920,"height":1080},"refreshRate":59.94}]},
                 {"id":1,"name":"eDP-1","enabled":true,"priority":1,"scale":1.5,
                  "pos":{"x":0,"y":0},
                  "currentModeId":7,"modes":[
                    {"id":7,"size":{"width":3840,"height":2160},"refreshRate":120.0}]},
                 {"id":3,"name":"DP-3","enabled":false,"scale":1,"pos":{"x":0,"y":0},
                  "modes":[{"id":"z","size":{"width":2560,"height":1440},"refreshRate":60.0}]},
                 {"id":4,"name":"Virtual-punktfunk-7","enabled":true,"scale":1,
                  "pos":{"x":5760,"y":0},"currentModeId":"v1","modes":[
                    {"id":"v1","size":{"width":2560,"height":1440},"refreshRate":119.98}]}
               ]}"#,
        )
        .expect("fixture parses");
        let mons = monitors_from_kscreen_json(&doc);
        let by = |c: &str| {
            mons.iter()
                .find(|m| m.connector == c)
                .unwrap_or_else(|| panic!("{c} missing"))
                .clone()
        };
        // Sorted by desktop position, not by kscreen's own order.
        let order: Vec<&str> = mons.iter().map(|m| m.connector.as_str()).collect();
        assert_eq!(
            order,
            vec!["DP-3", "eDP-1", "HDMI-A-1", "Virtual-punktfunk-7"]
        );
        let edp = by("eDP-1");
        // Pixels at the scale the desk runs — the point of `logical_size`.
        assert_eq!((edp.width, edp.height), (3840, 2160));
        assert_eq!(edp.scale, 1.5);
        assert_eq!(edp.logical_size(), (2560.0, 1440.0));
        assert!(edp.primary, "priority 1 is KWin's primary");
        assert_eq!(edp.refresh_mhz, 120_000);
        // 59.94 must survive as mHz; rounding to whole Hz here is the bug this guards.
        assert_eq!(by("HDMI-A-1").refresh_mhz, 59_940);
        assert_eq!(by("HDMI-A-1").description, "ACME U2720Q");
        assert!(!by("HDMI-A-1").primary);
        // Disabled: listed with no invented mode.
        let dark = by("DP-3");
        assert!(!dark.enabled);
        assert_eq!((dark.width, dark.height, dark.refresh_mhz), (0, 0, 0));
        // Ours, labelled by connector when the entry carries no make/model.
        let ours = by("Virtual-punktfunk-7");
        assert!(ours.managed);
        assert_eq!(ours.description, "Virtual-punktfunk-7");
        assert!(!by("eDP-1").managed);
    }

    /// No `outputs` array is an empty list, never a panic.
    #[test]
    fn a_malformed_kscreen_document_yields_no_monitors() {
        assert!(monitors_from_kscreen_json(&serde_json::json!({})).is_empty());
        assert!(monitors_from_kscreen_json(&serde_json::json!({"outputs": 7})).is_empty());
        // An output with no name cannot be pinned, so it is dropped.
        assert!(
            monitors_from_kscreen_json(&serde_json::json!({"outputs": [{"enabled": true}]}))
                .is_empty()
        );
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
}
