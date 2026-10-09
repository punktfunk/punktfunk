//! wlroots/Sway virtual-output backend via sway IPC + xdg-desktop-portal-wlr. scroll, a sway
//! fork, takes the same path through `scrollmsg`.
//!
//! 1. `swaymsg create_output` adds a headless output (`HEADLESS-N`). Sway must run the
//!    headless backend (or co-load it). The name is the before/after diff of
//!    `swaymsg -t get_outputs`.
//! 2. `swaymsg output <NAME> mode --custom WxH@HzHz` sets the client's mode. A fresh
//!    headless output needs a real mode for a refresh clock or it produces no frames.
//! 3. The ScreenCast portal yields the PipeWire node. There is no GUI picker, so a
//!    managed `~/.config/xdg-desktop-portal-wlr/config` sets `chooser_type=simple` and a
//!    `chooser_cmd` that cats a per-session file (`Monitor: <NAME>` — xdpw 0.8 parses
//!    that prefix strictly). Host shutdown puts the user's chooser back
//!    ([`restore_chooser_on_shutdown`]); the portal restarts on each change.
//! 4. Teardown is ordered: drop closes the ScreenCast session and waits for the portal
//!    to confirm, then `swaymsg output <NAME> unplug` (sway ≥1.8). See [`Keepalive`].
//!
//! Requirements: `SWAYSOCK` inherited or discovered per child ([`swaymsg_command`]),
//! portal env via `scripts/headless/prepare-session.sh`, ScreenCast routed to xdpw
//! (`scripts/headless/portals.conf`).

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use crate::monitors::DISABLE_BUDGET;
use crate::portal_cast::StopGuard;
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

/// Per-session file xdpw's chooser cats (`Monitor: <NAME>\n`). Under `$XDG_RUNTIME_DIR`
/// (0700), not a world-writable /tmp another user could pre-create or rewrite between
/// our write and xdpw's read.
fn chooser_file() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdpw-output")
}

/// `Monitor: <NAME>`: xdpw 0.8 parses that prefix strictly.
fn chooser_line(output: &str) -> String {
    format!("Monitor: {output}\n")
}

/// xdpw runs this via `/bin/sh -c` and reads stdout. With no session casting (the file
/// is gone), a share on the box goes to `fallback`. [`crate::portal_cast`] removes the
/// per-session file with the handshake so it cannot name an already-unplugged output.
fn chooser_cmd(fallback: &str) -> String {
    format!("cat {} 2>/dev/null || {fallback}", chooser_file())
}

/// What xdpw's own default chooser tries first.
const XDPW_DEFAULT_CHOOSER: &str = "slurp -f 'Monitor: %o' -or";

/// The user's own `simple` command, else [`XDPW_DEFAULT_CHOOSER`]. A `dmenu`-type command
/// reads xdpw's output list on stdin, which a `simple` chooser never gets.
fn chooser_fallback(path: &std::path::Path) -> String {
    let theirs = |key| match crate::portal_config::peek(path, XDPW_BLOCK, key) {
        (_, Some(prior)) => prior,
        (current, None) => current,
    };
    match (theirs("chooser_type").as_deref(), theirs("chooser_cmd")) {
        (Some("simple"), Some(cmd)) if !cmd.contains(&chooser_file()) => cmd,
        _ => XDPW_DEFAULT_CHOOSER.to_string(),
    }
}

/// xdpw's chooser cats [`chooser_file`].
pub(crate) const SELECTOR: crate::portal_cast::Selector = crate::portal_cast::Selector {
    file: chooser_file,
    line: chooser_line,
    ensure_config: ensure_xdpw_config,
    thread: "punktfunk-wlr-cast",
    portal: "xdpw",
};

/// wlroots/Sway virtual-display driver. Each [`create`](VirtualDisplay::create) adds one
/// headless output; a portal thread owns the cast.
pub struct WlrootsDisplay {
    /// Whose display this is. Set by `set_client_identity` before `create`, so the
    /// per-device topology (`design/web-console-overhaul.md` §6.1) can be resolved here.
    client_fp: Option<[u8; 32]>,

    /// Out-of-band cursor request: prefer portal `CursorMode::Metadata` (`SPA_META_Cursor`
    /// for the channel + blend). Off: prefer `Embedded` (compositor paints the pointer).
    ///
    /// Preference only: [`crate::portal_cursor`] matches what xdpw advertises — an
    /// unadvertised mode closes the session. xdpw refuses metadata by construction, so
    /// this backend always degrades to `Embedded`.
    hw_cursor: bool,
    /// Last portal-negotiated cursor mode. The host must read this rather than infer
    /// overlay absence from `hw_cursor`.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Restore for heads the last `create` disabled (`exclusive`). Written only through
    /// [`stash_topology_restore`](crate::backend::stash_topology_restore) — first-wins:
    /// one instance serves the host retry loop and only attempt 1 finds heads to disable.
    ///
    /// The registry takes it after a pooled `create` and runs it when the group empties.
    /// [`Drop`] is the backstop for a create that never reached the pool.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// ScreenCast from the `create` that just ran, until
    /// [`session_cast_for`](VirtualDisplay::session_cast_for) hands it to the session. The
    /// pooled output never holds the portal fd.
    pending_cast: Option<PendingCast>,
    /// The registry splits output and cast lifetimes around `create`. Direct callers keep
    /// both on the returned output.
    handoff_cast: bool,
    /// `mode_conflict: join` admitted this session.
    join_live: bool,
}

/// A parked ScreenCast of `name`: node, portal fd, and the guard that closes it.
struct PendingCast {
    node_id: u32,
    fd: OwnedFd,
    name: String,
    stop: StopGuard,
}

impl Drop for WlrootsDisplay {
    fn drop(&mut self) {
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl WlrootsDisplay {
    pub fn new() -> Result<Self> {
        Ok(WlrootsDisplay {
            client_fp: None,
            hw_cursor: false,
            last_cursor_mode: None,
            pending_restore: None,
            pending_cast: None,
            handoff_cast: false,
            join_live: false,
        })
    }

    /// Another portal cast of `name`, beside any cast already on it.
    fn cast_existing(&mut self, name: &str) -> Result<crate::backend::SessionCastParts> {
        let stream = crate::portal_cast::stream_existing_output(&SELECTOR, name, self.hw_cursor)?;
        self.last_cursor_mode = stream.cursor_mode;
        Ok(stream.into_cast())
    }
}

/// True when the host inherited `SWAYSOCK` (the IPC socket `swaymsg` needs).
/// Children get the socket via [`swaymsg_command`], so a `systemd --user` host
/// never sees it here. [`crate::available`] asks the `/proc` scan first.
///
/// Under [`crate::with_env_lock`] against this crate's remaining env writers. The mutex
/// is not reentrant; no caller holds it.
pub fn is_available() -> bool {
    crate::with_env_lock(|| std::env::var_os("SWAYSOCK")).is_some()
}

impl VirtualDisplay for WlrootsDisplay {
    /// The trait calls this before every `create`, which is what lets the per-device
    /// topology be resolved from inside it (§6.1).
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn name(&self) -> &'static str {
        "wlroots"
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        self.last_cursor_mode
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
    }

    fn set_session_cast_handoff(&mut self, enabled: bool) {
        self.handoff_cast = enabled;
    }

    /// The cast `create` parked, else a recast of a kept output, refocused as at create.
    fn session_cast_for(&mut self, name: &str) -> Result<Option<crate::backend::SessionCastParts>> {
        if let Some(pending) = self.pending_cast.take() {
            if pending.name == name {
                return Ok(Some((
                    pending.node_id,
                    Some(pending.fd),
                    Box::new(pending.stop),
                )));
            }
        }
        focus_output(name);
        self.cast_existing(name).map(Some)
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    fn join_cast(
        &mut self,
        name: &str,
        _node_id: u32,
    ) -> Result<Option<crate::backend::SessionCastParts>> {
        self.cast_existing(name).map(Some)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // A retry's parked cast from the last attempt closes before a new head exists.
        self.pending_cast = None;
        // Snapshot → create → identify under CREATE_LOCK. Sway names the output
        // (`HEADLESS-N`); two concurrent creates would each adopt the other's head
        // (silent mis-capture). The lock also serializes unplug on the failure path:
        // the output exists before `wait_new_output` can fail.
        let output = {
            let _create = CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let before = output_names().context(
                "swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)",
            )?;
            swaymsg(&["create_output"])
                .context("swaymsg create_output (sway needs the headless backend loaded)")?;
            // Own it from here so error unwind unplugs it; the output is usually already listed.
            match wait_new_output(&before, Duration::from_secs(5)) {
                Ok(name) => OutputGuard(name),
                Err(e) => {
                    // create_output succeeded; an unidentified HEADLESS-* would stay in the layout.
                    unplug_strays(&before);
                    return Err(e);
                }
            }
        };
        let name = output.0.clone();

        // Client mode is also the refresh clock; without it the output produces no frames.
        let m = format!(
            "{}x{}@{}Hz",
            mode.width,
            mode.height,
            mode.refresh_hz.max(1)
        );
        swaymsg(&["output", &name, "mode", "--custom", &m])
            .with_context(|| format!("swaymsg output {name} mode --custom {m}"))?;
        swaymsg(&["output", &name, "enable"])
            .with_context(|| format!("swaymsg output {name} enable"))?;

        focus_output(&name);

        let (fd, node_id, cursor_mode, stop) =
            crate::portal_cast::cast(&SELECTOR, &name, self.hw_cursor)?;
        // xdpw refuses metadata, so this is `embedded` regardless of `hw_cursor`.
        self.last_cursor_mode = Some(cursor_mode);
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            cursor = cursor_mode.name(),
            "sway headless output ready"
        );
        // Last: no failure path unwinds past the restore hand-off.
        crate::backend::apply_exclusive(
            "wlroots",
            self.client_fp,
            &name,
            &mut self.pending_restore,
            disable_other_heads,
            restore_heads,
        );
        // The registry pools the output and hands the cast to the session; a direct
        // caller keeps both.
        let (remote_fd, keepalive): (Option<OwnedFd>, Box<dyn Send>) = if self.handoff_cast {
            self.pending_cast = Some(PendingCast {
                node_id,
                fd,
                name: name.clone(),
                stop,
            });
            (None, Box::new(output))
        } else {
            (
                Some(fd),
                Box::new(Keepalive {
                    _stop: stop,
                    _output: output,
                }),
            )
        };
        Ok(VirtualOutput {
            node_id,
            remote_fd,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive,
            ownership: DisplayOwnership::Owned,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            // Absolute input aims at this `wl_output.name`; with real heads the
            // HEADLESS-* sits beside them.
            output_name: Some(name),
            input_output: None,
            seat: None,
            pid: None,
        })
    }
}

/// [`StopGuard`] blocks until the ScreenCast session is closed; [`OutputGuard`] then
/// unplugs. Fields drop in declaration order.
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// Serializes snapshot → `create_output` → identify, process-wide. Sway names the
/// output; a concurrent pair would each stream the other's head (no error). Mutter's
/// `TOPOLOGY_LOCK` is the same class; Hyprland names the output itself.
static CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `HEADLESS-` prefix only: sway names these itself, so an operator-made headless
/// output is indistinguishable. Callers stay narrow: [`unplug_strays`] also requires
/// it appeared during our `create_output`; [`super::super::focus_streamed_output`]
/// only passes the head this session is streaming.
pub(crate) fn is_managed_output(name: &str) -> bool {
    name.starts_with("HEADLESS-")
}

/// Unplug `HEADLESS-*` that appeared since `before` and nothing owns. A physical
/// hotplug in the same window is the operator's; `unplug` on a real connector takes
/// their screen. Runs with [`CREATE_LOCK`] held.
fn unplug_strays(before: &[String]) {
    let Ok(now) = output_names() else { return };
    for name in now
        .into_iter()
        .filter(|n| is_managed_output(n) && !before.iter().any(|b| b == n))
    {
        match swaymsg(&["output", &name, "unplug"]) {
            Ok(_) => tracing::warn!(output = %name, "unplugged a headless output we created but \
                 could not identify in time"),
            Err(e) => tracing::warn!(output = %name, error = %format!("{e:#}"), "could not unplug \
                 the headless output left behind by a failed create"),
        }
    }
}

/// Focus the head we are about to stream so session windows land where the client
/// can see them.
///
/// sway opens a new window on the focused workspace; `create_output` does not move
/// focus. The client's pointer is confined to the streamed output, so without this
/// every launch opens on the physical monitor. Best-effort: failure costs placement,
/// not the session.
pub(crate) fn focus_output(name: &str) {
    match swaymsg(&focus_argv(name)) {
        Ok(_) => tracing::info!(output = %name, "focused the streamed headless output"),
        Err(e) => tracing::warn!(
            output = %name, error = %format!("{e:#}"),
            "could not focus the streamed headless output — apps this session launches may open on \
             a physical monitor instead of on the stream"
        ),
    }
}

/// `focus output <name>` — noun second, unlike every other call in this file
/// (`output <name> mode|enable|unplug`). `output focus <name>` is rejected.
/// [`swaymsg`] passes through `--` and treats a non-zero exit as failure, so a bad
/// shape logs rather than succeeding silently (`hyprctl` would exit 0).
fn focus_argv(name: &str) -> [&str; 3] {
    ["focus", "output", name]
}

/// Windows on output `name`, or on every output when `name` is `None`.
///
/// Empty on any failure — this list is never worth an error. The tree already
/// nests output → workspace → containers, so one read answers the whole shape.
pub(crate) fn toplevels(name: Option<&str>) -> Vec<crate::toplevels::Toplevel> {
    match swaymsg_query("get_tree") {
        Ok(tree) => {
            let mut out = Vec::new();
            walk_tree(&tree, name, "", "", &mut out);
            out
        }
        Err(e) => {
            tracing::debug!(output = ?name, error = %format!("{e:#}"), "wlroots: no window list");
            Vec::new()
        }
    }
}

/// Descend `get_tree`, collecting leaf containers — on output `want`, or on
/// every output when it is `None`.
///
/// `output`/`workspace` are the names of the enclosing nodes, threaded down —
/// sway states each only at its own level. A leaf is a `con` with no children:
/// a split or tabbed container is a `con` too, and holds windows, not pixels.
fn walk_tree(
    node: &serde_json::Value,
    want: Option<&str>,
    output: &str,
    workspace: &str,
    out: &mut Vec<crate::toplevels::Toplevel>,
) {
    let kind = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let (output, workspace) = match kind {
        "output" => (name, workspace),
        "workspace" => (output, name),
        _ => (output, workspace),
    };
    let kids: Vec<&serde_json::Value> = ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|k| node.get(*k))
        .filter_map(|v| v.as_array())
        .flatten()
        .collect();
    // A scratchpad window's output is `__i3`, and it is on no screen, so it is
    // never in a list even when every output is wanted.
    let on_screen = want.map_or(output != "__i3", |w| output == w);
    if kids.is_empty() && matches!(kind, "con" | "floating_con") && on_screen {
        if let Some(id) = node.get("id").and_then(|v| v.as_i64()) {
            out.push(crate::toplevels::Toplevel {
                id: id.to_string(),
                title: name.to_string(),
                app_id: app_id_of(node),
                pid: node
                    .get("pid")
                    .and_then(|v| v.as_i64())
                    .and_then(|p| u32::try_from(p).ok()),
                focused: node
                    .get("focused")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                // 0 none, 1 full-screen on its workspace, 2 across outputs.
                fullscreen: node
                    .get("fullscreen_mode")
                    .and_then(|v| v.as_i64())
                    .is_some_and(|m| m != 0),
                workspace: workspace.to_string(),
                output: output.to_string(),
            });
        }
    }
    for kid in kids {
        walk_tree(kid, want, output, workspace, out);
    }
}

/// `app_id` on a Wayland window; an Xwayland one has none and carries an X11
/// class instead. Empty when neither is readable.
fn app_id_of(node: &serde_json::Value) -> String {
    node.get("app_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            node.get("window_properties")
                .and_then(|p| p.get("class"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or_default()
        .to_string()
}

/// Carry con `id` onto output `dest`. Best-effort: the game stays where it
/// opened on a refusal.
pub(crate) fn move_to_output(id: &str, dest: &str) -> Result<()> {
    let con: i64 = id
        .parse()
        .map_err(|_| anyhow!("window id {id} is not ours"))?;
    swaymsg(&[&format!("[con_id={con}]"), "move", "to", "output", dest]).map(|_| ())
}

/// Run one window verb on con `id`.
///
/// The id is re-parsed as a number before it reaches a criteria string: the
/// caller already checked it against the live list, and this keeps anything
/// else out of `[con_id=…]` whatever a future caller does.
pub(crate) fn window_action(verb: crate::toplevels::WindowVerb, id: &str) -> Result<()> {
    use crate::toplevels::WindowVerb;
    let con: i64 = id
        .parse()
        .map_err(|_| anyhow!("window id {id} is not ours"))?;
    let sel = format!("[con_id={con}]");
    let argv = match verb {
        WindowVerb::Focus => vec![sel.as_str(), "focus"],
        WindowVerb::Fullscreen => vec![sel.as_str(), "fullscreen", "enable"],
    };
    swaymsg(&argv).map(|_| ())
}

/// Workspace this launch gets on output `name`, as `(claimed, restore)`.
///
/// `want` re-focuses the workspace an earlier session claimed for the same
/// launch (keep-alive adopt); otherwise [`crate::routing::pick_workspace`]
/// chooses. `None` when the output shows no numbered workspace or the switch
/// is refused — the launch then opens where the output already looks.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let parsed = swaymsg_query("get_workspaces").ok()?;
    let restore = visible_workspace(&parsed, name)?;
    let id = want.unwrap_or_else(|| {
        crate::routing::pick_workspace(&workspace_slots(&parsed, name), restore)
    });
    if id == restore {
        return Some((id, restore));
    }
    match focus_workspace(id) {
        Ok(()) => Some((id, restore)),
        Err(e) => {
            tracing::warn!(
                workspace = id, output = %name, error = %format!("{e:#}"),
                "wlroots: workspace switch refused — this launch opens beside whatever the \
                 streamed output is already showing"
            );
            None
        }
    }
}

/// `swaymsg -t get_workspaces` reduced to the pick. sway reports no window
/// count, so emptiness is `representation` (the layout tree rendering, null on
/// an empty workspace); anything unreadable reads as occupied, which costs a
/// free number and never the operator's windows. Unnumbered workspaces
/// (`num: -1`) are dropped — `workspace number` cannot name one.
fn workspace_slots(parsed: &serde_json::Value, output: &str) -> Vec<crate::routing::WsSlot> {
    let Some(arr) = parsed.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|w| {
            let id = w.get("num")?.as_i64().filter(|n| *n >= 1)?;
            Some(crate::routing::WsSlot {
                id,
                on_head: w.get("output").and_then(|o| o.as_str()) == Some(output),
                empty: w
                    .get("representation")
                    .is_some_and(|r| r.is_null() || r.as_str() == Some("")),
            })
        })
        .collect()
}

/// Numbered workspace `output` is showing — the restore target. `None` when
/// the output shows a workspace with no number, which `workspace number`
/// cannot switch back to.
fn visible_workspace(parsed: &serde_json::Value, output: &str) -> Option<i64> {
    parsed
        .as_array()?
        .iter()
        .find(|w| {
            w.get("output").and_then(|o| o.as_str()) == Some(output)
                && w.get("visible").and_then(|v| v.as_bool()) == Some(true)
        })?
        .get("num")?
        .as_i64()
        .filter(|n| *n >= 1)
}

/// Switch to workspace `id`. A number sway does not have yet is created here,
/// empty, on the focused output.
pub(crate) fn focus_workspace(id: i64) -> Result<()> {
    swaymsg(&workspace_argv(&id.to_string())).map(|_| ())
}

/// `workspace number <n>` — `number` so sway matches the digit, not a
/// workspace literally named `4`. Split so a test pins the shape.
fn workspace_argv(n: &str) -> [&str; 3] {
    ["workspace", "number", n]
}

/// Disable the heads an `exclusive` session darkens ([`crate::monitors::disable_for_exclusive`]),
/// returning the ones [`restore_heads`] re-enables. `keep_monitors` stays lit.
///
/// `managed` is [`is_managed_output`], so a concurrent session's output is never blacked out.
/// The prefix is blunt: sway's own bootstrap `HEADLESS-1` is spared too. Leaving a headless
/// box's only screen lit is the cheaper failure.
fn disable_other_heads(ours: &str) -> Vec<String> {
    let keep = crate::policy::prefs().get().keep_monitors;
    crate::monitors::disable_for_exclusive(
        "wlroots",
        list_monitors(),
        ours,
        &keep,
        disable_head,
        focus_output,
    )
}

/// `swaymsg output <name> disable`, then read back. A bad command already fails
/// [`swaymsg`] (unlike `hyprctl`'s exit 0). The poll proves the output went
/// inactive — the state teardown must undo.
fn disable_head(name: &str) -> Result<()> {
    swaymsg(&disable_argv(name)).with_context(|| format!("swaymsg output {name} disable"))?;
    if wait_head_enabled_is(name, false, DISABLE_BUDGET) {
        return Ok(());
    }
    bail!("swaymsg accepted `output {name} disable` but the output never went inactive")
}

/// `output <name> disable` — noun first, opposite of [`focus_argv`]. Test-pinned.
fn disable_argv(name: &str) -> [&str; 3] {
    ["output", name, "disable"]
}

/// `output <name> dpms on|off`. Same noun-first shape as [`disable_argv`], different
/// axis: `dpms off` leaves the output enabled (workspaces stay) and stops the panel.
fn dpms_argv(name: &str, on: bool) -> [&str; 4] {
    ["output", name, "dpms", if on { "on" } else { "off" }]
}

/// `output <name> dpms on|off`. Sway's dpms sets state, so every accepted call counts as a change.
fn set_dpms(name: &str, on: bool) -> Result<bool> {
    swaymsg(&dpms_argv(name, on)).map(|_| true)
}

/// DPMS every head but a sibling's for a **gamescope** `exclusive` stream
/// ([`crate::monitors::dpms_others`]).
///
/// Not [`disable_other_heads`]: gamescope owns no sway output, so a disable would move
/// workspaces for a stream that is not on this compositor.
pub(crate) fn dpms_other_heads(on: bool) -> Vec<String> {
    crate::monitors::dpms_others("wlroots", list_monitors(), |n| set_dpms(n, on))
}

/// `dpms on` for exactly `names`, the heads [`dpms_other_heads`] darkened. The ones that took it.
pub(crate) fn relight_heads(names: &[String]) -> Vec<String> {
    crate::monitors::relight("wlroots", names, |n| set_dpms(n, true))
}

/// `output <name> enable`. Sway keeps a disabled output's config, so this restores
/// mode/position/scale; Hyprland needs `reload` instead.
fn enable_argv(name: &str) -> [&str; 3] {
    ["output", name, "enable"]
}

fn wait_head_enabled_is(name: &str, want: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(head_is_enabled(name), Ok(Some(got)) if got == want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Sway's `active` for `name`. `None` if absent. A disabled output stays in
/// `get_outputs` with `"active": false` — that is the read-back, not presence.
fn head_is_enabled(name: &str) -> Result<Option<bool>> {
    let parsed = swaymsg_query("get_outputs")?;
    let Some(arr) = parsed.as_array() else {
        return Ok(None);
    };
    for o in arr {
        if o.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(Some(
                o.get("active").and_then(|v| v.as_bool()).unwrap_or(true),
            ));
        }
    }
    Ok(None)
}

/// Re-enable heads `exclusive` disabled. The registry runs this when the group's
/// last member tears down, and **before** that member's output is unplugged — sway
/// must not see zero enabled outputs. `enable` is the inverse of `disable`
/// (Hyprland needs `hyprctl reload`). A miss logs the hand command.
fn restore_heads(disabled: &[String]) {
    for name in disabled {
        match swaymsg(&enable_argv(name)) {
            Ok(_) => {
                if wait_head_enabled_is(name, true, DISABLE_BUDGET) {
                    tracing::info!(output = %name, "wlroots: re-enabled the output `topology: exclusive` disabled");
                } else {
                    tracing::warn!(
                        output = %name,
                        "wlroots: `output enable` was accepted but the output is still inactive — \
                         re-enable it by hand with `swaymsg output {name} enable`"
                    );
                }
            }
            Err(e) => tracing::error!(
                output = %name, error = %format!("{e:#}"),
                "wlroots: could not re-enable this output — it is still dark. Run \
                 `swaymsg output {name} enable` by hand."
            ),
        }
    }
}

struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        match swaymsg(&["output", &self.0, "unplug"]) {
            Ok(_) => tracing::info!(output = %self.0, "sway headless output unplugged"),
            Err(e) => tracing::warn!(output = %self.0, error = %format!("{e:#}"), "unplug failed"),
        }
    }
}

/// 5 s per `swaymsg` ([`crate::proc`]). Against a wedged sway the client blocks in
/// connect forever; these calls run on the stream thread, whose only end is return.
/// Every call site already has a failed-query path.
const SWAYMSG_BUDGET: Duration = Duration::from_secs(5);

/// The IPC tool with its socket on the child only. `Command::env` avoids a process
/// `setenv` racing every `getenv` on a live host. `sock` is `None` when no IPC is
/// found: leave the child's env alone so an inherited socket still wins.
///
/// A `scroll-ipc.*` socket gets `scrollmsg`, which reads `SCROLLSOCK` before `SWAYSOCK`.
fn swaymsg_command(sock: Option<String>) -> Command {
    let scroll = sock.as_deref().is_some_and(|s| {
        std::path::Path::new(s)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("scroll-ipc."))
    });
    let (bin, var) = if scroll {
        ("scrollmsg", "SCROLLSOCK")
    } else {
        ("swaymsg", "SWAYSOCK")
    };
    let mut cmd = Command::new(bin);
    if let Some(sock) = sock {
        cmd.env(var, sock);
    }
    cmd
}

/// `swaymsg -- <args>` (`--` so `--custom` reaches sway, not swaymsg's getopt).
/// Non-zero exit covers `{"success": false}` too.
fn swaymsg(args: &[&str]) -> Result<String> {
    let mut cmd = swaymsg_command(crate::session::sway_socket());
    let prog = cmd.get_program().to_string_lossy().into_owned();
    let out = crate::proc::output_within(cmd.arg("--").args(args), SWAYMSG_BUDGET)
        .with_context(|| format!("run {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Query (`-t <kind> --raw`) and parse JSON. Not [`swaymsg`]: that helper inserts
/// `--`, so `-t` is read as a sway command (`Unknown/invalid command '-t'`).
fn swaymsg_query(kind: &str) -> Result<serde_json::Value> {
    let mut cmd = swaymsg_command(crate::session::sway_socket());
    let prog = cmd.get_program().to_string_lossy().into_owned();
    let out = crate::proc::output_within(cmd.args(["-t", kind, "--raw"]), SWAYMSG_BUDGET)
        .with_context(|| format!("run {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} -t {kind} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    serde_json::from_str(&raw).with_context(|| format!("parse {kind}"))
}

fn output_names() -> Result<Vec<String>> {
    let outputs = swaymsg_query("get_outputs")?;
    Ok(outputs
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| o.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .collect())
}

/// Every head `get_outputs` reports, for [`crate::monitors::list`]. `rect` is
/// logical (post-scale, post-transform). Inactive outputs have no `current_mode`;
/// mode fields read as zeros, not a guess.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let parsed = swaymsg_query("get_outputs")?;
    let mut out: Vec<_> = parsed
        .as_array()
        .context("get_outputs: not an array")?
        .iter()
        .filter_map(|o| {
            let connector = o.get("name")?.as_str()?.to_string();
            let rect = |k: &str| {
                o.get("rect")
                    .and_then(|r| r.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            };
            let mode = |k: &str| {
                o.get("current_mode")
                    .and_then(|m| m.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            };
            let str_field = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
            Some(crate::monitors::PhysicalMonitor {
                description: crate::monitors::describe(
                    str_field("make"),
                    str_field("model"),
                    &connector,
                ),
                width: mode("width").max(0) as u32,
                height: mode("height").max(0) as u32,
                // sway reports `refresh` in mHz already.
                refresh_mhz: mode("refresh").max(0) as u32,
                x: rect("x") as i32,
                y: rect("y") as i32,
                scale: o
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: o
                    .get("primary")
                    .and_then(|v| v.as_bool())
                    .or_else(|| o.get("focused").and_then(|v| v.as_bool()))
                    .unwrap_or(false),
                enabled: o.get("active").and_then(|v| v.as_bool()).unwrap_or(true),
                managed: is_managed_output(&connector),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// The name `create_output` added: present now, absent from `before`.
fn wait_new_output(before: &[String], timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(name) = output_names()?
            .into_iter()
            .find(|n| !before.iter().any(|b| b == n))
        {
            return Ok(name);
        }
        if Instant::now() >= deadline {
            bail!("create_output succeeded but no new output appeared");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

const XDPW_BLOCK: crate::portal_config::Block<'static> =
    crate::portal_config::Block::Ini("screencast");

fn xdpw_config_path() -> Result<std::path::PathBuf> {
    Ok(crate::portal_config::user_config_dir()?
        .join("xdg-desktop-portal-wlr")
        .join("config"))
}

/// Point xdpw at our chooser. It reads config only at startup, so `try-restart` on
/// change (D-Bus activation starts it later if it is not running). Selection is the
/// chooser file; this config is static. The edit records what it replaced, so
/// [`restore_chooser_on_shutdown`] can put it back.
fn ensure_xdpw_config() -> Result<()> {
    let path = xdpw_config_path()?;
    if !take_chooser(&path)? {
        return Ok(());
    }
    tracing::info!(path = %path.display(), "pointed xdg-desktop-portal-wlr at the managed output chooser");
    restart_xdpw();
    Ok(())
}

/// Only the two keys we own, in place. A full-file write would wipe the user's
/// other xdpw settings. `true` when the file changed.
fn take_chooser(path: &std::path::Path) -> Result<bool> {
    // Read before the edit: once taken, the file holds ours and marks theirs as prior.
    let cmd = chooser_cmd(&chooser_fallback(path));
    let mut changed = crate::portal_config::ensure_key(path, XDPW_BLOCK, "chooser_type", "simple")?;
    changed |= crate::portal_config::ensure_key(path, XDPW_BLOCK, "chooser_cmd", &cmd)?;
    Ok(changed)
}

/// Hand `chooser_type` and `chooser_cmd` back and restart xdpw if either changed.
/// Host shutdown only, never per cast: the restart cuts a live cast. Safe on a box
/// we never touched (no-op).
///
/// Left in place, every screen share on the box goes through our chooser first.
pub(crate) fn restore_chooser_on_shutdown() {
    let Ok(path) = xdpw_config_path() else { return };
    if !give_back_chooser(&path) {
        return;
    }
    tracing::info!(
        path = %path.display(),
        "restored the screen-share chooser xdg-desktop-portal-wlr had before this host"
    );
    restart_xdpw();
}

/// Undo [`take_chooser`]. `true` when the file changed.
fn give_back_chooser(path: &std::path::Path) -> bool {
    let mut changed = false;
    for key in ["chooser_type", "chooser_cmd"] {
        match crate::portal_config::restore_key(path, XDPW_BLOCK, key) {
            Ok(c) => changed |= c,
            Err(e) => tracing::warn!(path = %path.display(), key, error = %format!("{e:#}"),
                "previous xdpw chooser key not restored"),
        }
    }
    changed
}

/// Bounded and fire-and-forget: a timeout means xdpw reads the new config when it next starts.
fn restart_xdpw() {
    crate::gamescope::systemctl_user(&["try-restart", "xdg-desktop-portal-wlr.service"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `swaymsg -t get_tree` shape: root → output → workspace → cons, with
    /// a split container, a floating con, and an Xwayland window.
    const TREE: &str = r#"{"id":1,"name":"root","type":"root","nodes":[
      {"id":2,"name":"eDP-1","type":"output","nodes":[
        {"id":3,"name":"1","type":"workspace","nodes":[
          {"id":4,"name":"Private call — Ada","type":"con","app_id":"discord",
           "pid":7,"focused":false,"fullscreen_mode":0}
        ],"floating_nodes":[]}
      ],"floating_nodes":[]},
      {"id":10,"name":"HEADLESS-1","type":"output","nodes":[
        {"id":11,"name":"3","type":"workspace","nodes":[
          {"id":12,"name":"split","type":"con","nodes":[
            {"id":13,"name":"Dota 2","type":"con","pid":4242,"focused":true,
             "fullscreen_mode":1,"window_properties":{"class":"steam_app_570"}},
            {"id":14,"name":"kitty","type":"con","app_id":"kitty","pid":50,
             "focused":false,"fullscreen_mode":0}
          ],"floating_nodes":[]}
        ],"floating_nodes":[
          {"id":15,"name":"Steam - News","type":"floating_con","app_id":"steam",
           "pid":99,"focused":false,"fullscreen_mode":0}
        ]}
      ],"floating_nodes":[]}
    ],"floating_nodes":[]}"#;

    /// Only the streamed output's leaves leave the walk: the split container is
    /// not a window, and the operator's own head carries titles a guest must
    /// never be handed.
    #[test]
    fn a_window_list_holds_the_streamed_output_and_nothing_else() {
        let tree: serde_json::Value = serde_json::from_str(TREE).unwrap();
        let mut out = Vec::new();
        walk_tree(&tree, Some("HEADLESS-1"), "", "", &mut out);
        assert_eq!(
            out.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["13", "14", "15"],
            "three leaves; the split con holds windows, it is not one"
        );
        let game = &out[0];
        assert_eq!(game.title, "Dota 2");
        // Xwayland: no `app_id`, so the X11 class stands in.
        assert_eq!(game.app_id, "steam_app_570");
        assert_eq!(game.pid, Some(4242));
        assert_eq!(game.workspace, "3");
        assert_eq!(game.output, "HEADLESS-1");
        assert!(game.focused);
        assert!(game.fullscreen);
        assert_eq!(out[2].app_id, "steam", "a floating con is still a window");
        assert!(!out.iter().any(|w| w.title.contains("Private")));
    }

    /// A criteria string only ever carries a number, whatever it is handed.
    #[test]
    fn a_window_verb_refuses_an_id_that_is_not_a_con_number() {
        use crate::toplevels::WindowVerb;
        assert!(window_action(WindowVerb::Fullscreen, "12] kill; [con_id=99").is_err());
        assert!(window_action(WindowVerb::Focus, "").is_err());
    }

    /// Real `swaymsg -t get_workspaces` shape, trimmed to the fields read here.
    const WORKSPACES: &str = r#"[
      {"num":1,"name":"1","output":"eDP-1","visible":true,"focused":true,
       "representation":"H[firefox kitty]"},
      {"num":2,"name":"2","output":"eDP-1","visible":false,"representation":null},
      {"num":3,"name":"3","output":"HEADLESS-1","visible":true,"representation":"H[steam]"},
      {"num":-1,"name":"scratch","output":"HEADLESS-1","visible":false,"representation":null}
    ]"#;

    /// The streamed output's own workspaces decide, and an unnumbered one is
    /// never the answer — `workspace number` cannot name it.
    #[test]
    fn a_launch_lands_on_an_empty_workspace_of_the_streamed_output() {
        let parsed: serde_json::Value = serde_json::from_str(WORKSPACES).unwrap();
        assert_eq!(visible_workspace(&parsed, "HEADLESS-1"), Some(3));
        let slots = workspace_slots(&parsed, "HEADLESS-1");
        // Only 3 is ours and it holds the last game: the next free number.
        assert_eq!(crate::routing::pick_workspace(&slots, 3), 4);
        // The operator's own head already has an empty 2.
        let slots = workspace_slots(&parsed, "eDP-1");
        assert_eq!(crate::routing::pick_workspace(&slots, 1), 2);
    }

    /// `number` keeps sway matching the digit, not a workspace named `4`.
    #[test]
    fn a_workspace_switch_names_the_number() {
        assert_eq!(workspace_argv("4"), ["workspace", "number", "4"]);
    }

    /// `focus output <name>` — noun second. `output focus <name>` is rejected, and
    /// the only symptom is apps opening on the operator's monitor.
    #[test]
    fn focus_names_the_output_after_the_verb() {
        assert_eq!(focus_argv("HEADLESS-2"), ["focus", "output", "HEADLESS-2"]);
    }

    /// Topology verbs are `output <name> <verb>` (noun first), unlike [`focus_argv`].
    /// Both orders are pinned because this file uses both.
    #[test]
    fn disable_and_enable_name_the_output_before_the_verb() {
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
        assert_eq!(enable_argv("DP-1"), ["output", "DP-1", "enable"]);
    }

    /// `SWAYSOCK` is a per-child override, never `set_var` on the host (that write
    /// races every `getenv`). Known socket is set; unknown leaves the child's env.
    #[test]
    fn the_sway_socket_travels_on_the_child_not_the_process_env() {
        let overrides = |sock: Option<String>| -> Vec<(String, Option<String>)> {
            swaymsg_command(sock)
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect()
        };
        assert_eq!(
            overrides(Some("/run/user/1000/sway-ipc.1000.42.sock".to_string())),
            [(
                "SWAYSOCK".to_string(),
                Some("/run/user/1000/sway-ipc.1000.42.sock".to_string())
            )]
        );
        assert!(overrides(None).is_empty());
    }

    /// scroll ships `scrollmsg`, not `swaymsg`, and it reads `SCROLLSOCK` first.
    #[test]
    fn a_scroll_socket_runs_scrollmsg() {
        let cmd = swaymsg_command(Some("/run/user/1000/scroll-ipc.1000.42.sock".to_string()));
        assert_eq!(cmd.get_program(), "scrollmsg");
        let envs: Vec<_> = cmd.get_envs().map(|(k, _)| k.to_owned()).collect();
        assert_eq!(envs, ["SCROLLSOCK"]);
        assert_eq!(swaymsg_command(None).get_program(), "swaymsg");
    }

    fn head(connector: &str, enabled: bool) -> crate::monitors::PhysicalMonitor {
        crate::monitors::PhysicalMonitor {
            connector: connector.to_string(),
            description: connector.to_string(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled,
            // The rule the real `list_monitors` applies, so the fixture cannot drift from it.
            managed: is_managed_output(connector),
        }
    }

    /// `exclusive` disables the operator's outputs only. A sibling `HEADLESS-N` must
    /// survive or the second session blacks out the first.
    #[test]
    fn exclusive_disables_the_operators_outputs_and_never_a_headless_sibling() {
        let ours = "HEADLESS-2";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            // Sibling session, or sway's own bootstrap headless — both spared.
            head("HEADLESS-1", true),
            // Already off: must not enter the restore list or teardown would switch it on.
            head("DP-3", false),
        ];
        assert_eq!(
            crate::monitors::heads_to_darken(&heads, ours, &[]),
            vec!["DP-1", "HDMI-A-1"]
        );
    }

    /// `dpms` ≠ `disable`: disable moves workspaces; `dpms off` only stops the panel.
    /// Four tokens: `output <name> dpms on|off`.
    #[test]
    fn dpms_is_a_separate_verb_from_disable() {
        assert_eq!(dpms_argv("DP-1", false), ["output", "DP-1", "dpms", "off"]);
        assert_eq!(dpms_argv("DP-1", true), ["output", "DP-1", "dpms", "on"]);
        assert_eq!(disable_argv("DP-1"), ["output", "DP-1", "disable"]);
    }

    /// Gamescope DPMS reuses the disable filter with empty `ours`: nothing of ours
    /// to spare, but a concurrent session's `HEADLESS-*` still must not go dark.
    #[test]
    fn the_gamescope_dpms_arm_still_spares_a_sibling_headless() {
        let heads = [
            head("DP-1", true),
            head("HEADLESS-1", true),
            head("DP-3", false),
        ];
        assert_eq!(
            crate::monitors::heads_to_darken(&heads, "", &[]),
            vec!["DP-1"]
        );
    }

    /// Shutdown hands both chooser keys back byte for byte; left taken, every share on
    /// the box goes to our chooser.
    #[test]
    fn shutdown_hands_the_users_chooser_back_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("pf-vd-xdpw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("config");
        let user = "[screencast]\nchooser_type=dmenu\nchooser_cmd=wofi -d\noutput_name=DP-1\n";
        std::fs::write(&path, user).expect("seed");
        assert!(take_chooser(&path).expect("take"));
        let taken = std::fs::read_to_string(&path).expect("taken");
        assert!(give_back_chooser(&path));
        let back = std::fs::read_to_string(&path).expect("restored");
        let again = give_back_chooser(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(taken.contains("chooser_type=simple"), "{taken}");
        assert_eq!(back, user);
        assert!(!again, "a second restore finds nothing of ours");
    }

    /// With no session casting, a share on the box reaches the user's own chooser, never a
    /// guessed headless output.
    #[test]
    fn an_idle_share_falls_back_to_the_users_chooser() {
        let dir = std::env::temp_dir().join(format!("pf-vd-xdpw-fb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("config");
        std::fs::write(
            &path,
            "[screencast]\nchooser_type=simple\nchooser_cmd=my-picker\n",
        )
        .expect("seed");
        assert_eq!(chooser_fallback(&path), "my-picker");
        assert!(take_chooser(&path).expect("take"));
        // Taken: the prior is still theirs, not our own command.
        assert_eq!(chooser_fallback(&path), "my-picker");
        std::fs::write(
            &path,
            "[screencast]\nchooser_type=dmenu\nchooser_cmd=wofi -d\n",
        )
        .expect("seed");
        assert_eq!(chooser_fallback(&path), XDPW_DEFAULT_CHOOSER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "HEADLESS-1";
        assert!(crate::monitors::heads_to_darken(&[head(ours, true)], ours, &[]).is_empty());
    }
}
