//! Umbriel backend: one virtual output per [`create`](VirtualDisplay::create), made over
//! Umbriel's IPC and captured directly over `ext-image-copy-capture-v1`. There is no PipeWire
//! node (`node_id` 0): a session that cannot capture directly fails with one typed error.
//!
//! Outputs are `PF-<slot>` for a device with a slot, else `PF-<pid>-<n>` from Hyprland's counter
//! ([`crate::hyprland::head_name`]). Every one is journaled for the next host; a counter-named
//! leftover of a dead host is also recognisable by its pid, and [`reclaim_leftovers_once`]
//! removes it. `design/umbriel-support.md`.

pub(crate) mod desktop;
pub(crate) mod ipc;

use super::{Mode, SessionCastParts, VirtualDisplay, VirtualOutput};
use crate::hyprland::{head_name, is_managed_output};
use crate::topology_journal::{self as journal, Undo};
use anyhow::{bail, Context, Result};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

/// How long `create` waits for a new output to report its mode. Umbriel commits it before it
/// replies; this covers a busy compositor.
const MODE_WAIT: Duration = Duration::from_millis(1500);

/// This Umbriel parses neither request shape the host sends. Nothing works until the host is
/// updated.
#[derive(Debug)]
pub struct UmbrielIpcMismatch;

impl std::fmt::Display for UmbrielIpcMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("umbriel parses neither request shape this host sends: update the host")
    }
}

impl std::error::Error for UmbrielIpcMismatch {}

impl UmbrielIpcMismatch {
    /// The sentence the client shows.
    pub fn user_message(&self) -> &'static str {
        "The host can't talk to this version of Umbriel. Update Punktfunk on the host."
    }
}

/// An Umbriel session is running for this user.
pub(crate) fn is_available() -> bool {
    crate::session::umbriel_socket().is_some()
}

/// The socket answers, and this Umbriel can create outputs.
pub(crate) fn probe() -> Result<()> {
    ipc::outputs().context("ask umbriel over its IPC socket")?;
    if !ipc::has_virtual_outputs()? {
        bail!("umbriel has no output-destroy: it predates virtual outputs (2026-09-28); update it")
    }
    Ok(())
}

/// Every head Umbriel reports, for the monitor picker, the mirror pin and the journal.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let mut out: Vec<_> = ipc::outputs()?
        .into_iter()
        .map(|o| {
            let mode = o.mode().cloned().unwrap_or_default();
            let at = o.position.clone().unwrap_or_default();
            crate::monitors::PhysicalMonitor {
                description: crate::monitors::describe(
                    o.make.as_deref().unwrap_or_default(),
                    o.model.as_deref().unwrap_or_default(),
                    &o.name,
                ),
                width: mode.width,
                height: mode.height,
                refresh_mhz: mode.refresh_mhz,
                x: at.x as i32,
                y: at.y as i32,
                scale: o.scale.filter(|s| *s > 0.0).unwrap_or(1.0),
                primary: o.focused,
                enabled: o.enabled,
                managed: is_managed_output(&o.name),
                connector: o.name,
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Stream a head Umbriel already has (the monitor-mirror pin): the host captures it directly
/// by name, so there is nothing to start here.
pub(crate) fn stream_existing_output(connector: &str) -> crate::mirror::MirrorStream {
    tracing::info!(output = %connector, "umbriel: mirroring by direct capture");
    crate::mirror::MirrorStream {
        node_id: 0,
        remote_fd: None,
        cursor_mode: None,
        keepalive: Box::new(()),
    }
}

/// Point Umbriel's focus at `name` by showing its current workspace there: new windows open on
/// the focused output, and the cursor moves to its centre.
pub(crate) fn focus_output(name: &str) {
    let index = ipc::workspaces()
        .ok()
        .and_then(|ws| desktop::active_index(&ws, name))
        .unwrap_or(1);
    if let Err(e) = ipc::msg(&desktop::switch_action(index, name)) {
        tracing::warn!(output = %name, error = %format!("{e:#}"), "umbriel: output not focused");
    }
}

/// Owner pid for `PF-<pid>-<n>`; `None` for any other name, a slot head among them (the
/// journal's), so reclaim never guesses.
fn output_owner_pid(name: &str) -> Option<u32> {
    let (pid, seq) = name.strip_prefix("PF-")?.split_once('-')?;
    seq.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// Destroy `PF-<pid>-<n>` outputs whose host is gone. Once per process: a SIGKILLed host's
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
            match ipc::destroy_output(&name) {
                Ok(()) => tracing::info!(
                    output = %name, owner_pid = pid,
                    "umbriel: destroyed a virtual output left behind by a host that is no longer \
                     running"
                ),
                Err(e) => tracing::warn!(
                    output = %name, owner_pid = pid, error = %format!("{e:#}"),
                    "umbriel: leftover virtual output not destroyed"
                ),
            }
        }
    });
}

/// `mode` is `width`×`height` at `hz`, its refresh within 1 Hz.
fn mode_matches(mode: &ipc::OutputMode, width: u32, height: u32, hz: u32) -> bool {
    (mode.width, mode.height) == (width, height)
        && mode.refresh_mhz.abs_diff(hz.saturating_mul(1000)) <= 1000
}

/// Poll until `name` is listed at `mode`, then return what it runs as `(width, height,
/// millihertz)`. At the deadline a listed output is streamed at whatever mode it has.
fn wait_for_mode(name: &str, mode: Mode) -> Result<(u32, u32, u32)> {
    let deadline = Instant::now() + MODE_WAIT;
    loop {
        let current = ipc::outputs()?
            .into_iter()
            .find(|o| o.name == name)
            .and_then(|o| o.mode().cloned());
        match current {
            Some(m) if mode_matches(&m, mode.width, mode.height, mode.refresh_hz) => {
                return Ok((m.width, m.height, m.refresh_mhz));
            }
            Some(m) if Instant::now() >= deadline => {
                tracing::warn!(
                    output = %name,
                    want = %format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                    got = %format!("{}x{}@{}mHz", m.width, m.height, m.refresh_mhz),
                    "umbriel: the virtual output came up at another mode — streaming what it has"
                );
                return Ok((m.width, m.height, m.refresh_mhz));
            }
            None if Instant::now() >= deadline => {
                bail!("umbriel created {name} but listed no mode for it within {MODE_WAIT:?}")
            }
            _ => thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// One virtual output, journaled before it exists. Drop destroys it and settles the entry; a
/// destroy refused for an output Umbriel no longer lists settles it too.
struct OutputGuard {
    name: String,
    seq: u64,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match ipc::destroy_output(&self.name) {
            Ok(()) => {
                journal::done(self.seq);
                tracing::info!(output = %self.name, "umbriel: virtual output destroyed")
            }
            Err(_) if !listed(&self.name) => journal::done(self.seq),
            Err(e) => tracing::warn!(
                output = %self.name, error = %format!("{e:#}"),
                "umbriel: virtual output not destroyed"
            ),
        }
    }
}

/// Whether Umbriel lists an output named `name`, ignoring case as Umbriel does. An Umbriel that
/// does not answer lists everything.
fn listed(name: &str) -> bool {
    match ipc::outputs() {
        Ok(outs) => outs.iter().any(|o| o.name.eq_ignore_ascii_case(name)),
        Err(_) => true,
    }
}

#[derive(Default)]
pub struct UmbrielDisplay {
    /// Whose display this is, for the per-device topology resolved inside `create`.
    client_fp: Option<[u8; 32]>,
    /// The identity slot `create` resolves for `client_fp`, peeked by `set_client_identity`
    /// before it: the owner key of this output once kept, and the budget's own-display exemption.
    last_slot: Option<u32>,
    /// Journal seqs of the heads `exclusive` turned off, across every `create` of this instance.
    /// The registry takes them; [`Drop`] runs what a create that never reached the pool left.
    pending_undo: Vec<u64>,
    /// The output the last `create` or reuse served: the next `create` takes its windows.
    prev_output: Option<String>,
}

impl Drop for UmbrielDisplay {
    fn drop(&mut self) {
        let seqs = std::mem::take(&mut self.pending_undo);
        journal::apply(&seqs, self);
    }
}

impl VirtualDisplay for UmbrielDisplay {
    fn name(&self) -> &'static str {
        "umbriel"
    }

    /// Every head Umbriel lists: what the journal reads back after an `EnableHeads` undo.
    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        list_monitors()
    }

    /// Direct capture paints Umbriel's pointer into the frames and has no cursor metadata.
    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        Some(crate::PortalCursorMode::Embedded)
    }

    /// Called before every `create`; peeks the slot `create` will resolve, as on niri.
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
        let default = crate::policy::Identity::Shared;
        self.last_slot = crate::identity::peek_slot(fingerprint, (0, 0), default);
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

    /// A keep-alive reconnect reuses the kept output by name, for the device that owns it.
    fn accepts_kept(&self, identity_slot: Option<u32>, output_name: Option<&str>) -> bool {
        output_name.is_some() && crate::identity::may_reuse(identity_slot, self.client_fp)
    }

    /// No cast to start: the capture opens the kept output by name. Recorded so the next
    /// `create` takes its windows.
    fn session_cast_for(&mut self, name: &str) -> Result<Option<SessionCastParts>> {
        self.prev_output = Some(name.to_string());
        Ok(None)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        probe()?;
        reclaim_leftovers_once();
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        let name = head_name(self.last_slot, listed);
        let seq = journal::push(
            "umbriel",
            Undo::RemoveOutput {
                name: name.clone(),
                instance: ipc::instance(),
            },
        );
        // Owned before the request: every error from here on drops the guard, whose destroy
        // takes back what the request made, and a destroy of nothing settles the entry.
        let output = OutputGuard {
            name: name.clone(),
            seq,
        };
        ipc::create_output(&name, mode.width, mode.height, mode.refresh_hz)
            .context("create an umbriel virtual output")?;
        let (width, height, mhz) = wait_for_mode(&name, mode)?;
        focus_output(&name);
        tracing::info!(output = %name, w = width, h = height, mhz, "umbriel virtual output ready");
        // Last fallible step: nothing after it can fail, so no path turns heads off and then
        // unwinds past the journal hand-off.
        if crate::backend::exclusive_wanted("umbriel", self.client_fp) {
            desktop::disable_other_heads(&name, &mut self.pending_undo);
        }
        if let Some(prev) = self.prev_output.replace(name.clone()) {
            desktop::adopt_windows(&prev, &name);
        }
        let mut out =
            VirtualOutput::owned(0, Some((width, height, mode.refresh_hz)), Box::new(output));
        // Direct capture and absolute input find the output by its `wl_output` name.
        out.output_name = Some(name);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_prefixed_and_unique() {
        let a = head_name(None, |_| false);
        let b = head_name(None, |_| false);
        assert!(a.starts_with("PF-") && is_managed_output(&a), "{a}");
        assert_ne!(a, b);
        assert_eq!(head_name(Some(3), |_| false), "PF-3");
        assert_ne!(head_name(Some(3), |n| n == "PF-3"), "PF-3");
    }

    /// Only a counter name carries a pid: a slot head, a user's `PF-office` and a real head never.
    #[test]
    fn an_owner_pid_is_read_only_from_our_counter_names() {
        assert_eq!(output_owner_pid("PF-4242-1"), Some(4242));
        for name in ["PF-3", "PF-office", "PF-x-1", "DP-1", "PF-42-"] {
            assert_eq!(output_owner_pid(name), None, "{name}");
        }
    }

    #[test]
    fn a_mode_matches_within_one_hertz() {
        let m = |w, h, mhz| ipc::OutputMode {
            width: w,
            height: h,
            refresh_mhz: mhz,
            current: true,
        };
        assert!(mode_matches(&m(2560, 1600, 120_000), 2560, 1600, 120));
        assert!(mode_matches(&m(1920, 1080, 59_940), 1920, 1080, 60));
        assert!(!mode_matches(&m(1920, 1080, 58_000), 1920, 1080, 60));
        assert!(!mode_matches(&m(1920, 1200, 60_000), 1920, 1080, 60));
    }

    /// The journal reads the heads back after every `EnableHeads` undo; a backend that lists
    /// none keeps the entry forever.
    #[test]
    fn umbriel_lists_its_heads_for_the_journal() {
        let d = UmbrielDisplay::default();
        let err = VirtualDisplay::heads(&d)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(!err.contains("lists no heads"), "{err}");
    }
}
