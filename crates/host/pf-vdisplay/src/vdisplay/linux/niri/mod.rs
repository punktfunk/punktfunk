//! niri backend: one virtual output per [`create`](VirtualDisplay::create), made over niri's IPC
//! and cast through its in-process ScreenCast ([`cast`]).
//!
//! Only a niri that serves `CreateVirtualOutput` can host — the biri fork today
//! (`design/niri-support.md`). [`ipc::probe`] tells it from stock niri without side effects, and
//! stock niri gets [`NoVirtualOutputs`], which names biri.
//!
//! Outputs are `PF-<slot>` for a device with a slot, else `PF-<pid>-<n>` from Hyprland's counter
//! ([`crate::hyprland::head_name`]). Every one is journaled for the next host; a counter-named
//! leftover of a dead host is also recognisable by its pid, and [`reclaim_leftovers_once`]
//! removes it.

pub(crate) mod cast;
pub(crate) mod desktop;
mod events;
pub(crate) mod ipc;

use super::{Mode, SessionCastParts, VirtualDisplay, VirtualOutput};
use crate::hyprland::{head_name, is_managed_output};
use crate::topology_journal::{self as journal, Undo};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

/// How long `create` waits for a new output to report its mode. niri applies it in the same
/// event-loop turn; this covers a busy compositor.
const MODE_WAIT: Duration = Duration::from_millis(1500);

/// The running niri has no virtual outputs: stock niri, not biri.
#[derive(Debug)]
pub struct NoVirtualOutputs;

impl std::fmt::Display for NoVirtualOutputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this niri has no virtual outputs — install biri, the niri fork that adds them. See \
             https://docs.punktfunk.unom.io/docs/niri#get-biri",
        )
    }
}

impl std::error::Error for NoVirtualOutputs {}

impl NoVirtualOutputs {
    /// The sentence the client shows.
    pub fn user_message(&self) -> &'static str {
        "The host's niri can't create a display for this stream. Its owner can install biri; the \
         web console shows how."
    }
}

/// A niri session is running for this user.
pub(crate) fn is_available() -> bool {
    crate::session::niri_socket().is_some()
}

/// The socket answers, and this niri can create virtual outputs.
pub(crate) fn probe() -> Result<()> {
    let version = ipc::version().unwrap_or_default();
    match ipc::probe().context("ask niri over its IPC socket")? {
        ipc::Capability::VirtualOutputs => {
            tracing::info!(version = %version, "niri: virtual outputs available");
            Ok(())
        }
        ipc::Capability::NoVirtualOutputs => {
            tracing::warn!(version = %version, "{NoVirtualOutputs}");
            Err(NoVirtualOutputs.into())
        }
        ipc::Capability::Unsupported(why) => {
            bail!("niri {version} can't create a virtual output here: {why}")
        }
    }
}

/// Every head niri reports, for the monitor picker and the mirror pin.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let focused = ipc::workspaces()
        .ok()
        .and_then(|ws| ws.into_iter().find(|w| w.is_focused))
        .and_then(|w| w.output);
    let mut out: Vec<_> = ipc::outputs()?
        .into_iter()
        .map(|o| {
            let mode = o.mode().cloned();
            let logical = o.logical.clone();
            crate::monitors::PhysicalMonitor {
                description: crate::monitors::describe(&o.make, &o.model, &o.name),
                width: mode.as_ref().map_or(0, |m| m.width),
                height: mode.as_ref().map_or(0, |m| m.height),
                refresh_mhz: mode.as_ref().map_or(0, |m| m.refresh_rate),
                x: logical.as_ref().map_or(0, |l| l.x),
                y: logical.as_ref().map_or(0, |l| l.y),
                scale: logical
                    .as_ref()
                    .map(|l| l.scale)
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: focused.as_deref() == Some(o.name.as_str()),
                enabled: logical.is_some(),
                managed: is_managed_output(&o.name),
                connector: o.name,
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Cast a head niri already has (the monitor-mirror pin). Works on stock niri too.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (node_id, keepalive) = cast::start(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        // niri's ScreenCast publishes on the user's own PipeWire daemon.
        remote_fd: None,
        // Not a portal: niri honours the cursor mode it was asked for.
        cursor_mode: None,
        keepalive,
    })
}

/// Point niri's focus at `name`: new windows open on the focused output, and an output nothing
/// draws on produces no frames.
pub(crate) fn focus_output(name: &str) {
    if let Err(e) = ipc::action(json!({ "FocusMonitor": { "output": name } })) {
        tracing::warn!(output = %name, error = %format!("{e:#}"), "niri: output not focused");
    }
}

/// Owner pid for `PF-<pid>-<n>`; `None` for any other name, a slot head among them (the
/// journal's), so reclaim never guesses.
fn output_owner_pid(name: &str) -> Option<u32> {
    let (pid, seq) = name.strip_prefix("PF-")?.split_once('-')?;
    seq.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// Remove `PF-<pid>-<n>` outputs whose host is gone. Once per process: a SIGKILLed host's
/// outputs outlive it. The journal takes them back at host start; this catches what it missed.
fn reclaim_leftovers_once() {
    static RECLAIMED: Once = Once::new();
    RECLAIMED.call_once(|| {
        let Ok(outputs) = ipc::outputs() else { return };
        for name in outputs.into_iter().map(|o| o.name) {
            let Some(pid) = output_owner_pid(&name) else {
                continue;
            };
            if pid == std::process::id() || std::path::Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            match ipc::remove_virtual_output(&name) {
                Ok(()) => tracing::info!(
                    output = %name, owner_pid = pid,
                    "niri: removed a virtual output left behind by a host that is no longer running"
                ),
                Err(e) => tracing::warn!(
                    output = %name, owner_pid = pid, error = %format!("{e:#}"),
                    "niri: leftover virtual output not removed"
                ),
            }
        }
    });
}

/// Poll until `name` reports a mode, then return it as `(width, height, millihertz)`.
fn wait_for_mode(name: &str) -> Result<(u32, u32, u32)> {
    let deadline = Instant::now() + MODE_WAIT;
    loop {
        let found = ipc::outputs()?
            .into_iter()
            .find(|o| o.name == name)
            .and_then(|o| o.mode().map(|m| (m.width, m.height, m.refresh_rate)));
        if let Some(mode) = found {
            return Ok(mode);
        }
        if Instant::now() >= deadline {
            bail!("niri created {name} but it reported no mode within {MODE_WAIT:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Removes its output on drop.
/// One virtual output, journaled before it exists. Drop removes it and settles the entry; a
/// remove refused for an output niri no longer lists settles it too.
struct OutputGuard {
    name: String,
    seq: u64,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match ipc::remove_virtual_output(&self.name) {
            Ok(()) => {
                journal::done(self.seq);
                tracing::info!(output = %self.name, "niri: virtual output removed")
            }
            Err(_) if !listed(&self.name) => journal::done(self.seq),
            Err(e) => tracing::warn!(
                output = %self.name, error = %format!("{e:#}"),
                "niri: virtual output not removed"
            ),
        }
    }
}

/// Whether niri lists an output named `name`. A niri that does not answer lists everything.
fn listed(name: &str) -> bool {
    match ipc::outputs() {
        Ok(outs) => outs.iter().any(|o| o.name == name),
        Err(_) => true,
    }
}

/// A standalone output with its cast. Field order is drop order: the cast's drop waits for the
/// session's `Stop`, then niri removes the output.
struct OutputKeepalive {
    _cast: Box<dyn Send>,
    _output: OutputGuard,
}

/// The cast `create` started, parked until the registry's `session_cast_for` takes it, so a
/// pooled output never holds a session's cast.
struct PendingCast {
    name: String,
    node_id: u32,
    keepalive: Box<dyn Send>,
}

pub struct NiriDisplay {
    /// Whose display this is, for the per-device topology and scale resolved inside `create`.
    client_fp: Option<[u8; 32]>,
    /// The session's topology ([`VirtualDisplay::set_topology`]).
    topology: Option<crate::policy::Topology>,
    /// The identity slot `create` resolves for `client_fp`, peeked by `set_client_identity`
    /// before it: the owner key of this output once kept, and the budget's own-display exemption.
    last_slot: Option<u32>,
    /// The registry splits the output's lifetime from the session's cast around `create`.
    handoff_cast: bool,
    pending_cast: Option<PendingCast>,
    /// `mode_conflict: join`: the registry shares a live output and casts it for this session.
    join_live: bool,
    /// Journal seqs of the heads `exclusive` turned off, across every `create` of this instance:
    /// one instance serves the host's retry loop, and only attempt 1 finds heads to turn off.
    /// The registry takes them and runs them when the group empties; [`Drop`] runs what a
    /// create that never reached the pool left.
    pending_undo: Vec<u64>,
    /// The output the last `create` or recast used: the next `create` carries its workspace
    /// over, and `apply_position` places it.
    prev_output: Option<String>,
}

impl Drop for NiriDisplay {
    fn drop(&mut self) {
        let seqs = std::mem::take(&mut self.pending_undo);
        journal::apply(&seqs, self);
    }
}

impl NiriDisplay {
    pub fn new() -> Result<Self> {
        Ok(NiriDisplay {
            client_fp: None,
            topology: None,
            last_slot: None,
            handoff_cast: false,
            pending_cast: None,
            join_live: false,
            pending_undo: Vec::new(),
            prev_output: None,
        })
    }
}

impl VirtualDisplay for NiriDisplay {
    fn name(&self) -> &'static str {
        "niri"
    }

    /// Every head niri lists: what the journal reads back after an `EnableHeads` undo.
    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        list_monitors()
    }

    /// The pointer is always niri's own, painted into the frames: direct capture has no cursor
    /// metadata to give, so the cast matches it and the host composites nothing.
    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        Some(crate::PortalCursorMode::Embedded)
    }

    /// Called before every `create`, so the per-device topology resolves inside it (§6.1). Also
    /// peeks the slot `create` will resolve, so a reuse and the same-client retire know whose
    /// display this is first. `per-client-mode` keys on a mode only `create` has; the 0x0 peek
    /// finds none there.
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

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn take_undo_seqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_undo)
    }

    fn undo(&mut self, undo: &Undo) -> Result<()> {
        desktop::undo(undo)
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    /// A second cast of the owner's output; the owner's keeps running.
    fn join_cast(&mut self, name: &str, _node_id: u32) -> Result<Option<SessionCastParts>> {
        let (node_id, keepalive) = cast::start(name, false)?;
        Ok(Some((node_id, None, keepalive)))
    }

    /// A keep-alive reconnect recasts the kept output by name, for the device that owns it.
    fn accepts_kept(&self, identity_slot: Option<u32>, output_name: Option<&str>) -> bool {
        output_name.is_some() && crate::identity::may_reuse(identity_slot, self.client_fp)
    }

    fn set_session_cast_handoff(&mut self, enabled: bool) {
        self.handoff_cast = enabled;
    }

    fn session_cast_for(&mut self, name: &str) -> Result<Option<SessionCastParts>> {
        if let Some(pending) = self.pending_cast.take() {
            if pending.name == name {
                return Ok(Some((pending.node_id, None, pending.keepalive)));
            }
        }
        let (node_id, keepalive) = cast::start(name, false)?;
        // A recast output is the workspace source for the next `create`, as on Hyprland.
        self.prev_output = Some(name.to_string());
        Ok(Some((node_id, None, keepalive)))
    }

    /// Where the console's arrangement puts this output, in niri's logical space.
    fn apply_position(&mut self, x: i32, y: i32) {
        let Some(name) = self.prev_output.as_deref() else {
            return;
        };
        let at = json!({ "Position": { "position": { "Specific": { "x": x, "y": y } } } });
        if let Err(e) = ipc::output_action(name, at) {
            tracing::warn!(output = %name, x, y, error = %format!("{e:#}"), "niri: output not positioned");
        }
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        probe()?;
        reclaim_leftovers_once();
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        events::ensure_running();
        // A create whose handoff never completed still owns its cast.
        self.pending_cast = None;
        let name = head_name(self.last_slot, listed);
        let seq = journal::push(
            "niri",
            Undo::RemoveOutput {
                name: name.clone(),
                instance: ipc::instance(),
            },
        );
        // Owned before the request: every error from here on drops the guard, whose remove
        // takes back what the request made, and a remove of nothing settles the entry.
        let mut output = OutputGuard {
            name: name.clone(),
            seq,
        };
        let made = ipc::create_virtual_output(&name, mode.width, mode.height, mode.refresh_hz)
            .context("create a niri virtual output")?;
        if made != name {
            // niri named it otherwise: the journal and the guard follow the name it lists.
            journal::done(output.seq);
            output.seq = journal::push(
                "niri",
                Undo::RemoveOutput {
                    name: made.clone(),
                    instance: ipc::instance(),
                },
            );
            output.name = made.clone();
        }
        let name = made;
        let (width, height, mhz) = wait_for_mode(&name)?;
        if (width, height) != (mode.width, mode.height) {
            tracing::warn!(
                output = %name, want = %format!("{}x{}", mode.width, mode.height),
                got = %format!("{width}x{height}"),
                "niri: the virtual output came up at another size — streaming what it has"
            );
        }
        if let Some(scale) = crate::policy::prefs()
            .get()
            .scale_for(crate::policy::fp_hex(self.client_fp).as_deref())
        {
            desktop::apply_scale(&name, scale);
        }
        focus_output(&name);
        let (node_id, cast) = cast::start(&name, false)?;
        tracing::info!(
            output = %name, node_id, w = width, h = height, mhz,
            "niri virtual output ready"
        );
        // Last fallible step: nothing after it can fail, so no path turns heads off and then
        // unwinds past the journal hand-off.
        if crate::backend::exclusive_wanted("niri", self.topology, self.client_fp) {
            desktop::disable_other_heads(&name, &mut self.pending_undo);
        }
        if let Some(prev) = self.prev_output.replace(name.clone()) {
            desktop::adopt_active_workspace(&prev, &name);
        }
        let keepalive: Box<dyn Send> = if self.handoff_cast {
            self.pending_cast = Some(PendingCast {
                name: name.clone(),
                node_id,
                keepalive: cast,
            });
            Box::new(output)
        } else {
            Box::new(OutputKeepalive {
                _cast: cast,
                _output: output,
            })
        };
        let mut out =
            VirtualOutput::owned(node_id, Some((width, height, mode.refresh_hz)), keepalive);
        // Extend topology: input and direct capture find the output by its `wl_output` name.
        out.output_name = Some(name);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    /// A dead host's output is removed only from the niri that named it. With no match the entry
    /// is settled and nothing runs: no IPC here, and none is needed.
    #[test]
    fn an_output_of_another_niri_instance_is_left_alone() {
        for instance in ["/run/user/1000/niri.wayland-1.4242.sock", ""] {
            let undo = crate::topology_journal::Undo::RemoveOutput {
                name: "PF-4242-1".into(),
                instance: instance.into(),
            };
            assert!(super::desktop::undo(&undo).is_ok(), "{instance:?}");
        }
    }

    /// The slot the registry keys owner reuse and the budget on comes from the identity map
    /// here as on KWin: peeked at `set_client_identity`, none for a device with no identity.
    #[test]
    fn niri_reports_the_slot_the_identity_map_holds_for_its_device() {
        crate::fake::in_child(
            module_path!(),
            "niri_reports_the_slot_the_identity_map_holds_for_its_device",
            &[],
            || {
                use crate::VirtualDisplay;
                std::fs::write(
                    pf_paths::config_dir().join("display-settings.json"),
                    r#"{"identity":"per-client"}"#,
                )
                .unwrap();
                let fp = Some([9u8; 32]);
                let slot = crate::identity::resolve_slot(
                    fp,
                    (1920, 1080),
                    crate::policy::Identity::Shared,
                );
                assert!(slot.is_some(), "a per-client device has a slot");
                let mut d = super::NiriDisplay::new().unwrap();
                d.set_client_identity(fp);
                assert_eq!(d.last_identity_slot(), slot);
                d.set_client_identity(None);
                assert_eq!(d.last_identity_slot(), None);
            },
        );
    }

    /// The journal reads the heads back after every `EnableHeads` undo. A backend that lists
    /// none keeps the entry forever and re-lights the heads at every host start.
    #[test]
    fn niri_lists_its_heads_for_the_journal() {
        let d = super::NiriDisplay::new().unwrap();
        let err = crate::VirtualDisplay::heads(&d)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(!err.contains("lists no heads"), "{err}");
    }
}
