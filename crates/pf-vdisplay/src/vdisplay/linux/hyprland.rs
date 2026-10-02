//! Hyprland virtual-output backend via `hyprctl` IPC and the xdg ScreenCast portal
//! (xdg-desktop-portal-hyprland / xdph). See `design/hyprland-support.md`.
//!
//! Distinct from [`super::wlroots`]: Hyprland names headless outputs explicitly
//! (`hyprctl output create headless PF-<pid>-<n>`), so there is no before/after
//! diff. The creator pid in the name is what [`reclaim_leftovers_once`] uses to
//! drop leftovers whose owner is gone.
//!
//! A monitor rule sets the client's exact mode ([`set_monitor_rule`]). xdph is
//! steered at that output through a custom picker ([`crate::portal_picker`]).
//! The cast ([`crate::portal_cast`]) is session-scoped; [`OutputGuard`] lingers the named head and
//! evacuates its workspace onto a remaining physical before `output remove`.
//!
//! Requires a reachable Hyprland instance (`HYPRLAND_INSTANCE_SIGNATURE` or
//! `$XDG_RUNTIME_DIR/hypr/`) and ScreenCast routed to xdph
//! (`scripts/headless/portals.conf`). `hyprctl focusmonitor` without `dispatch`
//! answers `unknown request` at exit 0; [`hyprctl_dispatch`] turns that into
//! an error.

use super::{DisplayOwnership, Mode, SessionCastParts, VirtualDisplay, VirtualOutput};
use crate::portal_cast::StopGuard;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::BufRead;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Per-session picker file, under `$XDG_RUNTIME_DIR` (0700) — not world-writable
/// `/tmp`, where another local user could rewrite it between our write and xdph's
/// read.
fn selection_file() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-output")
}

/// Shim xdph runs as `custom_picker_binary`. Empty stdout (no session has written
/// the selection file) leaves xdph to its own fallback.
fn picker_shim_path() -> String {
    let dir = crate::session::runtime_dir();
    format!("{dir}/punktfunk-xdph-picker.sh")
}

fn xdph_config_path() -> Result<std::path::PathBuf> {
    Ok(crate::portal_config::user_config_dir()?
        .join("hypr")
        .join("xdph.conf"))
}
const XDPH_BLOCK: crate::portal_config::Block<'static> =
    crate::portal_config::Block::Hyprlang("screencopy");
const XDPH_PICKER_KEY: &str = "custom_picker_binary";

/// Is `cmd` safe to paste into the shim's `exec` line? A newline would split
/// the generated script; this is robustness, not a privilege boundary.
fn picker_is_plain(cmd: &str) -> bool {
    !cmd.is_empty()
        && cmd.len() <= 512
        && cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._/@:+=-".contains(c))
}

/// xdph's custom picker cats [`selection_file`]: `[SELECTION]/screen:<name>`,
/// every byte load-bearing ([`crate::portal_picker`]).
pub(crate) const SELECTOR: crate::portal_cast::Selector = crate::portal_cast::Selector {
    file: selection_file,
    line: crate::portal_picker::selection_line,
    ensure_config: ensure_xdph_config,
    thread: "punktfunk-hypr-cast",
    portal: "xdph",
};

/// Per-process seq for `PF-<pid>-<n>`. Named outputs skip sway's before/after
/// diff race.
static OUTPUT_SEQ: AtomicU32 = AtomicU32::new(0);

/// `PF-<pid>-<n>`. The pid is not decoration: `OutputGuard::drop` is the only
/// unplug, so a SIGKILLed host leaves heads behind. A bare `PF-<n>` restarts
/// at `PF-1` and collides; the pid lets [`reclaim_leftovers_once`] drop only
/// leftovers whose owner is gone.
fn next_output_name() -> String {
    format!(
        "PF-{}-{}",
        std::process::id(),
        OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// `PF-<pid>-<n>` or legacy `PF-<n>`. A user's own `PF-office` must not match.
pub(crate) fn is_managed_output(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("PF-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Owner pid for `PF-<pid>-<n>` only. `None` for legacy `PF-<n>` — no owner,
/// so reclaim must not guess.
fn output_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("PF-")?;
    let (pid, seq) = rest.split_once('-')?;
    seq.parse::<u32>().ok()?;
    pid.parse::<u32>().ok()
}

/// One named headless output per [`create`](VirtualDisplay::create). Stateless
/// besides the fields below.
pub struct HyprlandDisplay {
    /// Whose display this is. Set by `set_client_identity` before `create`, so the
    /// per-device topology (`design/web-console-overhaul.md` §6.1) can be resolved here.
    client_fp: Option<[u8; 32]>,

    /// Out-of-band cursor request. On: prefer portal `CursorMode::Metadata`.
    /// Off: prefer `Embedded` (compositor paints the pointer). Both are only a
    /// preference — [`crate::portal_cursor`] settles against what xdph advertises;
    /// an unadvertised mode fails the portal call. Current xdph advertises
    /// `Hidden|Embedded` only, so every session here resolves to `Embedded`.
    hw_cursor: bool,
    /// What the portal actually gave the last successful `create`. How the host
    /// learns a cursor overlay is never coming.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Topology restore from the first `create` (re-enable heads `exclusive`
    /// disabled). First-wins: this instance serves the pipeline retry loop and
    /// only attempt 1 finds heads to disable. The registry lifts this after a
    /// pooled `create`, so Drop is only the retry-loop backstop. Exclusive
    /// physicals stay dark for the linger window and re-light on real teardown.
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// Output the last successful `create` minted. A mid-stream resize replaces
    /// the head (create-before-drop); the next `create` carries its workspace
    /// over ([`adopt_active_workspace`]). Unset on failure so a half-created
    /// head is not the adoption source.
    prev_output: Option<String>,
    /// ScreenCast from the `create` that just ran. Registry [`session_cast_for`]
    /// takes it so the portal fd never sits on the pooled output.
    pending_cast: Option<PendingCast>,
    /// The registry requests a split output/cast lifetime around `create`.
    /// Direct callers keep the portal fd and cast on their returned output.
    handoff_cast: bool,
    /// `mode_conflict: join`: the registry shares a live head and casts it for this session.
    join_live: bool,
    /// The session negotiated BT.2020 PQ: the next `create` lights its head 10-bit HDR.
    hdr: bool,
    /// The client panel's volume, the HDR head's mastering target when known.
    client_hdr: Option<pf_frame::HdrMeta>,
}

impl Drop for HyprlandDisplay {
    fn drop(&mut self) {
        // A registry create that never completed its handoff still owns this
        // cast. Close it before the backend disappears.
        if let Some(pending) = self.pending_cast.take() {
            stop_cast(&pending.name);
        }
        // Retry-loop backstop: the registry takes a pooled restore, so this is
        // only a create that never reached acquire.
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl HyprlandDisplay {
    pub fn new() -> Result<Self> {
        Ok(HyprlandDisplay {
            client_fp: None,
            hw_cursor: false,
            last_cursor_mode: None,
            pending_restore: None,
            prev_output: None,
            pending_cast: None,
            handoff_cast: false,
            join_live: false,
            hdr: false,
            client_hdr: None,
        })
    }

    /// Attach a cast to `name`. A successful recast makes that reused output
    /// the workspace source for the next `create`.
    fn session_cast_for_with(
        &mut self,
        name: &str,
        start: impl FnOnce(&str, bool) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode)>,
    ) -> Result<Option<SessionCastParts>> {
        if let Some(pending) = self.pending_cast.take() {
            if pending.name == name {
                return Ok(Some((
                    pending.node_id,
                    Some(pending.fd),
                    Box::new(SessionCast(pending.name)),
                )));
            }
            stop_cast(&pending.name);
        }
        let (fd, node_id, cursor_mode) = start(name, self.hw_cursor)?;
        self.last_cursor_mode = Some(cursor_mode);
        self.prev_output = Some(name.to_string());
        Ok(Some((
            node_id,
            Some(fd),
            Box::new(SessionCast(name.to_string())),
        )))
    }

    /// Apply [`crate::policy::Topology`] for `ours` and stash the restore the
    /// registry runs on real teardown. Called at the END of `create` so nothing
    /// can fail after it and unwind past the hand-off. Physical heads stay lit
    /// through the portal handshake — that is also `extend`.
    fn apply_topology(&mut self, ours: &str) {
        use crate::policy::Topology;
        match crate::effective_topology(self.client_fp) {
            Topology::Extend | Topology::Auto => {}
            Topology::Primary => warn_primary_is_not_expressible(),
            Topology::Exclusive => {
                let disabled = disable_other_heads(ours);
                let prepared = (!disabled.is_empty()).then(|| {
                    Box::new(move || restore_heads(&disabled)) as Box<dyn FnOnce() + Send>
                });
                // First restore wins: the retry loop calls `create` up to eight
                // times on this instance, and only attempt 1 has heads to disable.
                // A plain assignment overwrote it with attempt 2's `None`.
                crate::backend::stash_topology_restore(&mut self.pending_restore, prepared);
            }
        }
    }

    /// Record `name` and return the output whose workspace it replaces.
    fn replace_output(&mut self, name: &str) -> Option<String> {
        self.prev_output.replace(name.to_string())
    }
}

/// Usable when a live Hyprland instance for our uid is reachable: inherited
/// `HYPRLAND_INSTANCE_SIGNATURE`, or a socket under `$XDG_RUNTIME_DIR/hypr/`
/// (the systemd `--user` host has no env import). Cheap — safe on enumeration.
///
/// Both env reads take [`crate::with_env_lock`] in one scope so the pair is one
/// consistent view. The lock is not reentrant; `read_dir` runs outside it.
pub fn is_available() -> bool {
    let (sig, runtime) = crate::with_env_lock(|| {
        (
            std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
    });
    if sig.is_some() {
        return true;
    }
    let dir = match runtime {
        Some(d) => std::path::PathBuf::from(d).join("hypr"),
        None => return false,
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join(".socket.sock").exists())
}

/// `hyprctl` must reach the compositor now, not at create-time. Warns if the
/// permission system is enforcing (silent black frames / dropped input).
pub fn probe() -> Result<()> {
    hyprctl(&["-j", "version"]).context(
        "hyprctl not reachable — is Hyprland running and HYPRLAND_INSTANCE_SIGNATURE set? (the \
         host must run inside, or be able to reach, the Hyprland session)",
    )?;
    if let Some((maj, min, pat)) = hyprland_version() {
        tracing::info!(version = %format!("{maj}.{min}.{pat}"), "Hyprland backend ready");
    }
    warn_if_permissions_enforced();
    Ok(())
}

impl VirtualDisplay for HyprlandDisplay {
    /// The trait calls this before every `create`, which is what lets the per-device
    /// topology be resolved from inside it (§6.1).
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    /// Kept out of [`casts`]: that map holds one cast per name, and a joiner must not close
    /// the owner's.
    fn join_cast(&mut self, name: &str, _node_id: u32) -> Result<Option<SessionCastParts>> {
        let stream = crate::portal_cast::stream_existing_output(&SELECTOR, name, self.hw_cursor)?;
        self.last_cursor_mode = stream.cursor_mode;
        Ok(Some(stream.into_cast()))
    }

    fn name(&self) -> &'static str {
        "hyprland"
    }

    /// A keep-alive reconnect recasts the kept head at its name, so it needs that name and
    /// this session's identity. A nameless kept head is refused: lingering a black head is
    /// worse than tearing it down. Mode already matched, since a kept head never resizes.
    fn accepts_kept(&self, identity_slot: Option<u32>, output_name: Option<&str>) -> bool {
        output_name.is_some() && identity_slot == self.last_identity_slot()
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn set_client_hdr(&mut self, hdr: Option<pf_frame::HdrMeta>) {
        self.client_hdr = hdr;
    }

    fn set_hdr(&mut self, on: bool) {
        self.hdr = on;
    }

    fn hdr(&self) -> bool {
        // Reuse key: an sRGB head handed to an HDR session would be captured as PQ.
        self.hdr
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

    fn session_cast_for(&mut self, name: &str) -> Result<Option<SessionCastParts>> {
        self.session_cast_for_with(name, start_cast)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        preflight_once();
        reclaim_leftovers_once();
        if let Some(pending) = self.pending_cast.take() {
            stop_cast(&pending.name);
        }

        let name = next_output_name();
        hyprctl_dispatch(&["output", "create", "headless", &name]).with_context(|| {
            format!("hyprctl output create headless {name} (is hyprctl reachable?)")
        })?;
        // Own from here so any later error (or drop) removes it.
        let output = OutputGuard(name.clone());
        wait_monitor_ready(&name, Duration::from_secs(5))
            .with_context(|| format!("waiting for headless output {name} to appear"))?;

        // Client mode is also the frame clock: a headless output is timer-paced from it.
        let colour =
            apply_monitor_rule(&name, mode, self.hdr.then(|| HdrRule::new(self.client_hdr)))
                .with_context(|| format!("set monitor rule for {name}"))?;

        // Portal fd stays off this output so the registry can linger the named
        // head. [`session_cast_for`] hands the fd to the session, not the pool.
        let (fd, node_id, cursor_mode) = start_cast(&name, self.hw_cursor)?;
        // On today's xdph this is `embedded` regardless of `hw_cursor`; the
        // session's cursor behaviour follows this, not the request.
        self.last_cursor_mode = Some(cursor_mode);
        let (remote_fd, direct_cast) = if self.handoff_cast {
            self.pending_cast = Some(PendingCast {
                node_id,
                fd,
                name: name.clone(),
            });
            (None, None)
        } else {
            (Some(fd), Some(SessionCast(name.clone())))
        };
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            cursor = cursor_mode.name(),
            hdr = colour.is_some(),
            "hyprland headless output ready"
        );
        // Last, so no failure path unwinds past the restore hand-off.
        self.apply_topology(&name);
        // Hyprland gives each replacement an empty workspace. Carry the active
        // workspace only after every fallible setup step has completed.
        if let Some(prev) = self.replace_output(&name) {
            adopt_active_workspace(&prev, &name);
        }
        let output_keepalive = OutputKeepalive {
            _reload: watch_config_reloads(name.clone(), mode, colour),
            _output: output,
        };
        let keepalive: Box<dyn Send> = match direct_cast {
            Some(cast) => Box::new(DirectKeepalive {
                _cast: cast,
                _output: output_keepalive,
            }),
            None => Box::new(output_keepalive),
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
            // Extend topology: this head sits beside the operator's, so absolute
            // input has to be aimed at it by name. `hyprctl`'s monitor name is
            // `wl_output.name`, which the injector matches.
            output_name: Some(name),
            input_output: None,
            seat: None,
            pid: None,
        })
    }
}

/// Named head the registry lingers. ScreenCast is [`SessionCast`], not here:
/// pooling this must not keep the portal session alive across reconnects.
struct OutputKeepalive {
    /// First so the watcher is gone before the output is removed — it must
    /// never re-apply a rule onto a head this teardown deletes.
    _reload: Option<ReloadWatcher>,
    _output: OutputGuard,
}

/// Standalone `create`: close ScreenCast before removing its named output.
struct DirectKeepalive {
    _cast: SessionCast,
    _output: OutputKeepalive,
}

/// ScreenCast parked off the pooled output until [`HyprlandDisplay::session_cast_for`]
/// takes it. The fd is session-scoped; the named head is not.
struct PendingCast {
    node_id: u32,
    fd: OwnedFd,
    name: String,
}

/// Closes the ScreenCast for `name` on drop. Token only — the real closer is
/// [`StopGuard`] in [`casts`].
struct SessionCast(String);

impl Drop for SessionCast {
    fn drop(&mut self) {
        stop_cast(&self.0);
    }
}

/// Live ScreenCast per output name. [`SessionCast`] and [`OutputGuard`] both
/// remove from here; the first drop closes, the second is a no-op.
fn casts() -> &'static Mutex<HashMap<String, StopGuard>> {
    static CASTS: OnceLock<Mutex<HashMap<String, StopGuard>>> = OnceLock::new();
    CASTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stop_cast(name: &str) {
    let guard = casts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(name);
    drop(guard);
}

/// Point xdph at `name` and park the [`StopGuard`] in [`casts`]. Closes a
/// leftover cast on the same name first so reconnect does not stack sessions.
fn start_cast(name: &str, hw_cursor: bool) -> Result<(OwnedFd, u32, crate::portal_cursor::Mode)> {
    stop_cast(name);
    focus_output(name);
    let (fd, node_id, cursor_mode, stop) = crate::portal_cast::cast(&SELECTOR, name, hw_cursor)?;
    casts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), stop);
    Ok((fd, node_id, cursor_mode))
}

/// Reads the event socket: puts the streamed head's monitor rule back after a
/// `hyprctl reload`, and bumps [`WINDOW_GEN`] when the window list moves.
///
/// A reload drops every runtime `hyprctl keyword` (see [`restore_heads`]),
/// including [`set_monitor_rule`]'s mode. The compositor does not re-apply it.
/// One subscription, not polling, so an idle session costs nothing — and one
/// socket, so the window list rides this reader rather than opening a second.
///
/// The MODE and its colour only. A reload also undoes `topology: exclusive` head
/// disables, but re-disabling them here races teardown's [`restore_heads`]
/// (`hyprctl reload` to re-light): the watcher and that restore do not share a lifetime.
fn watch_config_reloads(
    name: String,
    mode: Mode,
    colour: Option<HdrRule>,
) -> Option<ReloadWatcher> {
    let path = event_socket_path()?;
    let sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                path = %path.display(), error = %e,
                "hyprland: no event socket — a `hyprctl reload` (every theme switch, on Omarchy) \
                 will reset this stream's resolution until the client resizes"
            );
            return None;
        }
    };
    // Shutting this clone down is what unparks the blocking read below.
    let stopper = sock.try_clone().ok()?;
    WINDOW_WATCHERS.fetch_add(1, Ordering::Relaxed);
    thread::spawn(move || {
        // The count is live readers: however this thread ends, it leaves the count.
        struct Listening;
        impl Drop for Listening {
            fn drop(&mut self) {
                WINDOW_WATCHERS.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _listening = Listening;
        // Lossy: a window title that is not UTF-8 must not end the reader.
        for line in std::io::BufReader::new(sock).split(b'\n') {
            // Guard shutdown or compositor gone — nothing left to re-apply to.
            let Ok(line) = line else { return };
            let line = String::from_utf8_lossy(&line);
            if is_window_event(&line) {
                WINDOW_GEN.fetch_add(1, Ordering::Relaxed);
            }
            if !is_config_reload(&line) {
                continue;
            }
            tracing::info!(
                output = %name, w = mode.width, h = mode.height,
                "hyprland: config reloaded — re-applying the streamed head's monitor rule"
            );
            if let Err(e) = set_monitor_rule(&name, mode, colour) {
                // Errors only when the head has no framebuffer — gone after a
                // reload (teardown, or compositor restart). Stop.
                tracing::warn!(
                    output = %name, error = %format!("{e:#}"),
                    "hyprland: could not re-apply the monitor rule after a config reload — the \
                     client keeps the head's default resolution until it resizes"
                );
                return;
            }
        }
    });
    Some(ReloadWatcher(stopper))
}

/// Ends [`watch_config_reloads`]'s thread by shutting its socket down. The thread
/// leaves [`WINDOW_WATCHERS`] as it exits.
///
/// The thread is parked in a blocking read. A stop flag would leave it alive
/// until the compositor emitted an event — one stranded thread per session,
/// and sessions are minted on every mid-stream resize.
struct ReloadWatcher(UnixStream);

impl Drop for ReloadWatcher {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// Event socket for the instance we are driving. Same signature [`hyprctl_command`]
/// threads onto every child, so the watch and the commands cannot aim at
/// different compositors.
fn event_socket_path() -> Option<std::path::PathBuf> {
    let sig = crate::session::hypr_signature()?;
    let runtime = crate::with_env_lock(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!("/run/user/{}", crate::proc::current_uid()))
        });
    Some(runtime.join("hypr").join(sig).join(".socket2.sock"))
}

/// Hyprland's `.socket2.sock` speaks `<name>>><data>`. Match the NAME: a
/// `contains` would also fire on a window titled `configreloaded`, and every
/// false hit is a `hyprctl` round trip on a live stream.
fn is_config_reload(line: &str) -> bool {
    line.split(">>").next() == Some("configreloaded")
}

/// Bumped by [`watch_config_reloads`] on every event that can change the window
/// list. Free-running and shared by all heads: it is a "re-read" signal, not a
/// count of anything.
static WINDOW_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Live [`ReloadWatcher`] count. With none, [`WINDOW_GEN`] is frozen and would
/// pin a stale list forever, so [`window_gen`] admits it cannot tell.
static WINDOW_WATCHERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Does this event change what [`toplevels`] would return? Same NAME match as
/// [`is_config_reload`]: a window titled `openwindow` must not bump the token.
fn is_window_event(line: &str) -> bool {
    matches!(
        line.split(">>").next(),
        Some(
            "openwindow"
                | "closewindow"
                | "movewindow"
                | "movewindowv2"
                | "windowtitle"
                | "windowtitlev2"
                | "activewindow"
                | "activewindowv2"
                | "fullscreen"
                | "changefloatingmode"
                | "monitorremoved"
        )
    )
}

/// Re-read token for [`crate::toplevels::toplevels_token`]. `None` when no
/// watcher is on the socket — then the caller must not trust a frozen value.
pub(crate) fn window_gen() -> Option<u64> {
    (WINDOW_WATCHERS.load(Ordering::Relaxed) > 0).then(|| WINDOW_GEN.load(Ordering::Relaxed))
}

/// Remove `PF-<pid>-<n>` outputs whose owner pid is gone, once per process
/// before we create our first.
///
/// [`OutputGuard::drop`] is the only unplug, so a SIGKILLed host leaves heads
/// in the compositor for the session's life. Keyed on the owner pid so a
/// second live host (or this process) cannot have its output pulled. `Once`
/// puts the sweep strictly before this process owns anything. A dead host's
/// `exclusive` session also left the real heads disabled; they come back too.
fn reclaim_leftovers_once() {
    static RECLAIMED: Once = Once::new();
    RECLAIMED.call_once(|| {
        let Ok(names) = monitor_names() else { return };
        let mut removed = false;
        for name in &names {
            let name = name.clone();
            let Some(pid) = output_owner_pid(&name) else {
                // Not ours, or legacy `PF-<n>` with no owner — a still-running
                // older host may be streaming it.
                if is_managed_output(&name) {
                    tracing::debug!(output = %name, "a managed headless output with no owner pid in \
                         its name (an older host build) — left alone");
                }
                continue;
            };
            if pid == std::process::id() || std::path::Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            match hyprctl_dispatch(&["output", "remove", &name]) {
                Ok(()) => {
                    removed = true;
                    tracing::info!(output = %name, owner_pid = pid, "removed a headless \
                         output left behind by a host that is no longer running")
                }
                Err(e) => tracing::warn!(output = %name, owner_pid = pid, error = %format!("{e:#}"),
                    "leftover headless output not removed"),
            }
        }
        if removed {
            let dark: Vec<String> = names
                .into_iter()
                .filter(|n| matches!(head_is_enabled(n), Ok(Some(false))))
                .collect();
            if !dark.is_empty() {
                restore_heads(&dark);
            }
        }
    });
}

/// Point Hyprland's focus at the head we are about to stream.
///
/// New windows open on the focused monitor's active workspace, and
/// `output create headless` does not focus what it creates. The client's
/// pointer is confined to the streamed output, so focus-follows-mouse cannot
/// reach it. An unfocused headless output stays empty, empty produces no
/// damage, and no damage means no PipeWire frames.
///
/// Classic `hyprctl dispatch focusmonitor <name>` for hyprlang. Under the Lua
/// config manager `dispatch` is `hl.dispatch(...)`, so those bare words die
/// with `')' expected near '<name>'`. Lua spelling is
/// `hl.dsp.focus({ monitor = "<name>" })`. Try classic, then Lua.
pub(crate) fn focus_output(name: &str) {
    let classic = match hyprctl_dispatch(&focus_argv(name)) {
        Ok(()) => None,
        Err(e) => match hyprctl_dispatch(&["dispatch", &lua_focus_expr(name)]) {
            Ok(()) => None,
            Err(lua_err) => Some(format!("hyprlang: {e:#}; lua: {lua_err:#}")),
        },
    };
    match classic {
        None => tracing::info!(output = %name, "focused the streamed headless output"),
        Some(why) => tracing::warn!(
            output = %name, error = %why,
            "could not focus the streamed headless output — apps this session launches may open on \
             a physical monitor instead of on the stream, and an unfocused headless output can \
             produce no frames at all"
        ),
    }
}

/// Lua-config-manager spelling of "focus this monitor". Pure so a test pins
/// the shape: quoting and the `monitor =` key are the whole trick.
fn lua_focus_expr(name: &str) -> String {
    format!("hl.dsp.focus({{ monitor = \"{name}\" }})")
}

/// `hyprctl` argv that focuses `name`, split so a test pins its shape.
///
/// `focusmonitor` is a dispatcher — it lives behind `dispatch`. A bare
/// `hyprctl focusmonitor` answers `unknown request` at exit 0
/// ([`hyprctl_dispatch`]).
fn focus_argv(name: &str) -> [&str; 3] {
    ["dispatch", "focusmonitor", name]
}

/// `topology: primary` has no expression here. Wayland has no primary output;
/// Hyprland's nearest is the focused monitor, which [`focus_output`] already
/// points at the streamed head. Distinct from `exclusive`, which changes the desk.
fn warn_primary_is_not_expressible() {
    tracing::info!(
        "hyprland: `topology: primary` has no equivalent here — Wayland has no primary output and \
         Hyprland has only a FOCUSED monitor, which the streamed head already holds. Treating it \
         as `extend`; use `exclusive` to actually disable the operator's heads."
    );
}

/// DPMS every head that is not ours and not a sibling's, for a **gamescope**
/// session honoring `Topology::Exclusive` — see [`crate::panel_dpms`].
///
/// Distinct from [`disable_other_heads`]: disabling a Hyprland head's only
/// known undo is re-reading the operator's whole config ([`restore_heads`]),
/// dropping every runtime override. DPMS is a separate axis (`dispatch dpms on
/// <name>` does not re-enable a *disabled* head). A gamescope spawn owns no
/// Hyprland output, hence empty `ours`. No keep list: the gamescope darken
/// ignores `keep_monitors` on every compositor.
pub(crate) fn dpms_other_heads(on: bool) -> Vec<String> {
    let Ok(heads) = list_monitors() else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for name in crate::monitors::heads_to_darken(&heads, "", &[]) {
        match dpms_one(&name, on) {
            // Only a head this call moved. The dispatcher toggles, so "fixing"
            // one already in the wanted state would break it, and the re-light
            // would then toggle a head we never darkened.
            Ok(true) => changed.push(name),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "hyprland: monitor not blanked for `topology: exclusive`"
            ),
        }
    }
    changed
}

/// DPMS on for exactly `names`, the heads [`dpms_other_heads`] darkened. The ones now on.
pub(crate) fn relight_heads(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| match dpms_one(name, true) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(output = %name, error = %format!("{e:#}"), "hyprland: monitor not re-lit");
                false
            }
        })
        .cloned()
        .collect()
}

/// DPMS state Hyprland reports for `name` (`hyprctl -j monitors all`'s
/// `dpmsStatus`). `None` when unlisted or the field is missing. A DPMS-off
/// monitor stays listed — the readback [`dpms_one`] is built around.
fn monitor_dpms(name: &str) -> Option<bool> {
    monitor(name, true).ok()??.get("dpmsStatus")?.as_bool()
}

/// Put one monitor into `want_on`, reporting whether this call changed it.
///
/// The dispatcher is a toggle, not a set — it ignores the state word. A blind
/// "off" lights an already-dark head; a blind "on" at teardown darkens a lit
/// one. Read → act only if it differs → verify. That shape is also correct
/// where the call really is a set.
///
/// Classic `hyprctl dispatch dpms off <name>` dies under Lua (`dispatch` is
/// `hl.dispatch(...)`). No stable probe for which manager is loaded, so try
/// classic, then Lua. Never omit the monitor name: `hl.dsp.dpms("on")` answers
/// `ok` and toggles *something*.
fn dpms_one(name: &str, want_on: bool) -> Result<bool> {
    if monitor_dpms(name) == Some(want_on) {
        return Ok(false); // toggling would break it
    }
    let classic =
        match hyprctl_dispatch(&["dispatch", "dpms", if want_on { "on" } else { "off" }, name]) {
            Ok(()) => None,
            Err(e) => {
                let lua = lua_dpms_expr(name, want_on);
                match hyprctl_dispatch(&["dispatch", &lua]) {
                    Ok(()) => None,
                    Err(lua_err) => Some(format!("hyprlang: {e:#}; lua: {lua_err:#}")),
                }
            }
        };
    if let Some(why) = classic {
        bail!("neither dispatch form was accepted for {name} — {why}");
    }
    // Verify: a toggle that fired against a state we misread is worse than one
    // that did not fire at all.
    match monitor_dpms(name) {
        Some(now) if now == want_on => Ok(true),
        Some(now) => bail!(
            "hyprland accepted the dpms dispatch for {name} but it is now dpmsStatus={now}, \
             wanted {want_on} (the dispatcher toggles — the readback disagreed with reality)"
        ),
        None => bail!("hyprland stopped listing {name} after its dpms dispatch"),
    }
}

/// Lua-config-manager spelling of per-monitor DPMS. Pure so a test pins the
/// shape — quoting is the whole trick.
fn lua_dpms_expr(name: &str, on: bool) -> String {
    format!(
        "hl.dsp.dpms(\"{}\", \"{name}\")",
        if on { "on" } else { "off" }
    )
}

/// Active workspace id for monitor `name` (`hyprctl -j monitors`). `None` when
/// the monitor is already gone or the field is missing.
fn active_workspace_id(name: &str) -> Option<i64> {
    monitor(name, false)
        .ok()??
        .get("activeWorkspace")?
        .get("id")?
        .as_i64()
}

/// Windows on head `name`, or on every head when `name` is `None`.
///
/// Two reads: `clients` names a workspace, `workspaces` names each workspace's
/// monitor, and `clients`' own `monitor` is an index that does not survive a
/// hotplug. Empty on any failure — this list is never worth an error.
pub(crate) fn toplevels(name: Option<&str>) -> Vec<crate::toplevels::Toplevel> {
    let Ok(clients) = hyprctl_json(&["clients"]) else {
        tracing::debug!(output = ?name, "hyprland: no client list");
        return Vec::new();
    };
    let Ok(spaces) = hyprctl_json(&["workspaces"]) else {
        tracing::debug!(output = ?name, "hyprland: no workspace list");
        return Vec::new();
    };
    parse_clients(&clients, &spaces, name)
}

/// `hyprctl -j clients` + `-j workspaces` reduced to windows, filtered to head
/// `monitor` when one is named.
///
/// A window whose workspace no workspace list claims is dropped rather than
/// guessed onto a head. Unmapped and hidden windows are not on screen, so they
/// are not in a switcher.
fn parse_clients(
    clients: &serde_json::Value,
    spaces: &serde_json::Value,
    monitor: Option<&str>,
) -> Vec<crate::toplevels::Toplevel> {
    let (Some(clients), Some(spaces)) = (clients.as_array(), spaces.as_array()) else {
        return Vec::new();
    };
    clients
        .iter()
        .filter(|c| {
            c.get("mapped").and_then(|v| v.as_bool()) != Some(false)
                && c.get("hidden").and_then(|v| v.as_bool()) != Some(true)
        })
        .filter_map(|c| {
            let ws = c.get("workspace")?;
            let id = ws.get("id")?.as_i64()?;
            let on = spaces
                .iter()
                .find(|w| w.get("id").and_then(|v| v.as_i64()) == Some(id))?
                .get("monitor")?
                .as_str()?;
            monitor.is_none_or(|want| want == on).then_some(())?;
            Some(crate::toplevels::Toplevel {
                id: c.get("address")?.as_str()?.to_string(),
                title: string_field(c, "title"),
                app_id: string_field(c, "class"),
                pid: c
                    .get("pid")
                    .and_then(|v| v.as_i64())
                    .and_then(|p| u32::try_from(p).ok()),
                // Hyprland reports no `focused`; the focus stack does, and its
                // head is the focused window.
                focused: c.get("focusHistoryID").and_then(|v| v.as_i64()) == Some(0),
                fullscreen: is_fullscreen(c.get("fullscreen")),
                workspace: ws
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map_or_else(|| id.to_string(), str::to_string),
                output: on.to_string(),
            })
        })
        .collect()
}

/// A missing or unreadable string field is empty, never a dropped window: a
/// nameless window is still one the player can see and wants to reach.
fn string_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `fullscreen` is a bool on older Hyprland and a mode int on newer (0 = none).
/// Anything unreadable is "not full-screen" — the honest answer for a field
/// this host cannot parse.
fn is_fullscreen(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(v) if v.is_boolean() => v.as_bool().unwrap_or(false),
        Some(v) => v.as_i64().is_some_and(|m| m != 0),
        None => false,
    }
}

/// Run one window verb on `address`.
///
/// Classic dispatchers only. The Lua config era spells these `hl.dsp.*` and
/// this host has no instance to pin the spelling against, so that era gets a
/// refusal and one log line rather than a guessed expression.
pub(crate) fn window_action(verb: crate::toplevels::WindowVerb, address: &str) -> Result<()> {
    use crate::toplevels::WindowVerb;
    let target = format!("address:{address}");
    match verb {
        WindowVerb::Focus => hyprctl_dispatch(&["dispatch", "focuswindow", &target]),
        // No per-window fullscreen dispatcher: focus it, then act on the focused
        // window. `1` is maximize-free full-screen.
        WindowVerb::Fullscreen => {
            hyprctl_dispatch(&["dispatch", "focuswindow", &target])?;
            hyprctl_dispatch(&["dispatch", "fullscreen", "1"])
        }
    }
}

/// Carry window `address` onto head `dest`, then follow it there.
///
/// `movewindow` acts on the focused window, so focus it first. Best-effort:
/// the game stays where it opened on a refusal.
pub(crate) fn move_to_output(address: &str, dest: &str) -> Result<()> {
    let target = format!("address:{address}");
    hyprctl_dispatch(&["dispatch", "focuswindow", &target])?;
    hyprctl_dispatch(&["dispatch", "movewindow", &format!("mon:{dest}")])
}

/// Workspace this launch gets on head `name`, as `(claimed, restore)`.
///
/// `want` re-focuses the workspace an earlier session claimed for the same
/// launch (keep-alive adopt); otherwise [`crate::routing::pick_workspace`]
/// chooses. `None` when the head's workspace cannot be read or the switch is
/// refused — the launch then opens where the head already looks.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let restore = active_workspace_id(name)?;
    let id = match want {
        Some(id) => id,
        None => {
            let parsed = hyprctl_json(&["workspaces"]).ok()?;
            crate::routing::pick_workspace(&workspace_slots(&parsed, name), restore)
        }
    };
    if id == restore {
        return Some((id, restore));
    }
    match focus_workspace(id) {
        Ok(()) => Some((id, restore)),
        Err(e) => {
            tracing::warn!(
                workspace = id, output = %name, error = %format!("{e:#}"),
                "hyprland: workspace switch refused — this launch opens beside whatever the \
                 streamed head is already showing"
            );
            None
        }
    }
}

/// Workspace `id` holds no window. An unreadable count reads as occupied.
fn workspace_empty(id: i64) -> bool {
    hyprctl_json(&["workspaces"]).is_ok_and(|p| {
        workspace_slots(&p, "")
            .iter()
            .any(|w| w.id == id && w.empty)
    })
}

/// `hyprctl -j workspaces` reduced to the pick. A missing `windows` count
/// reads as occupied, so a payload this host cannot parse costs a free id and
/// never puts the game on the operator's desk.
fn workspace_slots(parsed: &serde_json::Value, monitor: &str) -> Vec<crate::routing::WsSlot> {
    let Some(arr) = parsed.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|w| {
            Some(crate::routing::WsSlot {
                id: w.get("id")?.as_i64()?,
                on_head: w.get("monitor").and_then(|m| m.as_str()) == Some(monitor),
                empty: w.get("windows").and_then(|n| n.as_i64()) == Some(0),
            })
        })
        .collect()
}

/// Switch the focused monitor to workspace `id`. Classic first, Lua on
/// rejection — the same two-era probe as [`focus_output`]. An id nothing owns
/// is minted here, empty.
pub(crate) fn focus_workspace(id: i64) -> Result<()> {
    let ws = id.to_string();
    hyprctl_dispatch_both(&evacuate_focus_argv(&ws), &lua_workspace_focus_expr(&ws))
}

/// Move the superseded head's active workspace onto the new head, then switch
/// the new head to it. Inverse of [`evacuate_workspace`].
///
/// Hyprland assigns every new monitor an empty workspace. Two dispatches:
/// `workspace.move` does not activate the moved workspace on its target. When
/// the predecessor is no longer listed this is a reconnect — nothing to carry.
fn adopt_active_workspace(prev: &str, ours: &str) {
    let Some(id) = active_workspace_id(prev) else {
        return;
    };
    if let Err(e) = workspace_to_monitor(id, ours) {
        tracing::warn!(
            workspace = id, from = %prev, to = %ours, error = %format!("{e:#}"),
            "hyprland: could not move the streamed workspace to the replacement head — the \
             client will land on an empty workspace after this resize"
        );
        return;
    }
    // Both dispatchers answer `ok` even when the era mismatch made them do
    // nothing, so the readback is the only real signal. An empty adopted
    // workspace evaporates on the move and the switch re-creates it on the
    // focused (new) head — same id, so the readback holds for that case too.
    match active_workspace_id(ours) {
        Some(now) if now == id => tracing::info!(
            workspace = id, from = %prev, to = %ours,
            "hyprland: carried the streamed workspace onto the replacement head"
        ),
        now => tracing::warn!(
            workspace = id, active = ?now, to = %ours,
            "hyprland: both workspace dispatches were accepted but the replacement head shows a \
             different active workspace — the client may land on an empty workspace"
        ),
    }
}

/// Move workspace `id` onto `dest` and switch to it there. Classic first, Lua
/// on rejection — same two-era probe as [`focus_output`].
fn workspace_to_monitor(id: i64, dest: &str) -> Result<()> {
    let ws = id.to_string();
    hyprctl_dispatch_both(
        &evacuate_move_argv(&ws, dest),
        &lua_workspace_move_expr(&ws, dest),
    )?;
    hyprctl_dispatch_both(&evacuate_focus_argv(&ws), &lua_workspace_focus_expr(&ws))?;
    Ok(())
}

fn hyprctl_dispatch_both(classic: &[&str], lua: &str) -> Result<()> {
    match hyprctl_dispatch(classic) {
        Ok(()) => Ok(()),
        Err(classic_err) => hyprctl_dispatch(&["dispatch", lua])
            .map_err(|lua_err| anyhow::anyhow!("hyprlang: {classic_err:#}; lua: {lua_err:#}")),
    }
}

/// Classic `hyprctl` argv that moves workspace `ws` onto `dest`.
fn evacuate_move_argv<'a>(ws: &'a str, dest: &'a str) -> [&'a str; 4] {
    ["dispatch", "moveworkspacetomonitor", ws, dest]
}

/// Classic `hyprctl` argv that switches to workspace `ws` on the focused monitor.
fn evacuate_focus_argv(ws: &str) -> [&str; 3] {
    ["dispatch", "workspace", ws]
}

/// Where the streamed workspace goes before the named head is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Evacuate {
    /// `moveworkspacetomonitor` then `workspace`. Never destroy the windows.
    ToPhysical { workspace: i64, dest: String },
    /// No remaining physical. Skip the move; workspaces stay in limbo.
    Limbo,
}

/// Inverse of [`adopt_active_workspace`]: a dest means re-home, else limbo.
fn evacuate_plan(workspace: Option<i64>, dest: Option<&str>) -> Evacuate {
    match (workspace, dest) {
        (Some(workspace), Some(dest)) => Evacuate::ToPhysical {
            workspace,
            dest: dest.to_string(),
        },
        _ => Evacuate::Limbo,
    }
}

/// First enabled physical that is not `ours` and not a managed sibling.
fn first_physical_dest(heads: &[crate::monitors::PhysicalMonitor], ours: &str) -> Option<String> {
    heads
        .iter()
        .find(|h| h.enabled && !h.managed && h.connector != ours)
        .map(|h| h.connector.clone())
}

/// Re-home the streamed workspace onto a remaining physical, then remove is safe.
/// Headless: skip the move. Windows are never destroyed. An empty workspace stays: it has
/// nothing to save, and switching to it would take focus from a replacement head that now
/// shows the game (a resize leaves the retired head an empty one).
fn evacuate_workspace(ours: &str) {
    let dest = list_monitors()
        .ok()
        .and_then(|heads| first_physical_dest(&heads, ours));
    let workspace = active_workspace_id(ours).filter(|&id| !workspace_empty(id));
    match evacuate_plan(workspace, dest.as_deref()) {
        Evacuate::Limbo => {}
        Evacuate::ToPhysical { workspace, dest } => {
            if let Err(e) = workspace_to_monitor(workspace, &dest) {
                tracing::warn!(
                    workspace, from = %ours, to = %dest, error = %format!("{e:#}"),
                    "hyprland: streamed workspace not re-homed onto a remaining physical — \
                     windows stay on this head until it is removed"
                );
            }
        }
    }
}

/// Lua spelling of "move workspace N to monitor M". Pure so a test pins the
/// shape: workspace is a quoted string, monitor quoted as in [`lua_focus_expr`].
fn lua_workspace_move_expr(ws: &str, monitor: &str) -> String {
    format!("hl.dsp.workspace.move({{ workspace = \"{ws}\", monitor = \"{monitor}\" }})")
}

/// Lua spelling of "switch to workspace N" — `hl.dsp.focus` with a `workspace`
/// argument. There is no `hl.dsp.workspace.*` member that switches.
fn lua_workspace_focus_expr(ws: &str) -> String {
    format!("hl.dsp.focus({{ workspace = \"{ws}\" }})")
}

/// Disable every head [`crate::monitors::darkens`] names for an `exclusive`
/// session, returning the ones actually disabled (input to [`restore_heads`]).
/// `managed` is [`is_managed_output`], so a concurrent session is never blacked
/// out, and `keep_monitors` stays lit. Best-effort per head.
fn disable_other_heads(ours: &str) -> Vec<String> {
    let heads = match list_monitors() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "hyprland: could not enumerate monitors for `topology: exclusive` — leaving the \
                 operator's heads enabled (the session still streams, as `extend`)"
            );
            return Vec::new();
        }
    };
    let keep = crate::policy::prefs().get().keep_monitors;
    let targets = crate::monitors::heads_to_darken(&heads, ours, &keep);
    if targets.is_empty() {
        tracing::info!(
            "hyprland: `topology: exclusive` had nothing to disable — no enabled head besides the \
             managed and kept ones (a headless box, or a sibling session already took the desk)"
        );
        return Vec::new();
    }
    let mut disabled = Vec::new();
    for name in targets {
        match disable_head(&name) {
            Ok(()) => disabled.push(name),
            Err(e) => tracing::warn!(
                output = %name, error = %format!("{e:#}"),
                "hyprland: head not disabled for `topology: exclusive` — it stays lit"
            ),
        }
    }
    if !disabled.is_empty() {
        tracing::info!(
            ?disabled,
            "hyprland: `topology: exclusive` — the streamed output is now the desk"
        );
        // Disabling re-homes workspaces and the compositor picks the new
        // focus. Re-assert ours so window placement still lands on the stream.
        focus_output(ours);
    }
    disabled
}

/// Disable one head, both config eras, confirming by read-back.
///
/// Same two-era shape as [`set_monitor_rule`]: `hyprctl keyword` is rejected
/// under Lua; `hyprctl eval` is rejected under hyprlang. Both at **exit 0**,
/// so the read-back — not the exit status — decides.
fn disable_head(name: &str) -> Result<()> {
    let spec = disable_rule_spec(name);
    let lua = disable_lua_expr(name);
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    let mut attempts: Vec<String> = Vec::new();
    for a in [&keyword, &eval] {
        if let Err(e) = hyprctl_dispatch(a) {
            let said = format!("{e:#}");
            tracing::debug!(output = %name, cmd = ?a, error = %said, "hyprctl rejected this disable form — trying the other config era");
            attempts.push(said);
            continue;
        }
        if wait_head_disabled(name, DISABLE_BUDGET) {
            return Ok(());
        }
        attempts.push(format!(
            "hyprctl {a:?} was accepted but the head never went disabled"
        ));
    }
    bail!("no hyprctl form disabled {name}: {}", attempts.join("; "))
}

/// Hyprlang disable rule (`hyprctl keyword monitor <this>`). `disable` is a
/// whole-rule verb — there is no `<name>,<mode>,disable`, and no `<name>,enable`
/// to undo it.
fn disable_rule_spec(name: &str) -> String {
    format!("{name},disable")
}

/// Lua disable rule (`hyprctl eval <this>`). The field is `disabled` (past
/// tense) and takes a boolean — `disable = true` and `mode = "disable"` are
/// rejected, so this is not a place to guess from the hyprlang spelling.
fn disable_lua_expr(name: &str) -> String {
    format!("hl.monitor{{ output = \"{name}\", disabled = true }}")
}

/// Poll until `name` reports `disabled: true` (the rule applies asynchronously).
fn wait_head_disabled(name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(head_is_enabled(name), Ok(Some(false))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Is head `name` enabled? `None` if absent. Reads `-j monitors all` — the
/// plain listing drops a disabled head, so it cannot distinguish disabled
/// from unplugged.
fn head_is_enabled(name: &str) -> Result<Option<bool>> {
    Ok(monitor(name, true)?.map(|m| !m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false)))
}

/// How long a `disable` (or the `reload` that undoes it) has to show up in
/// `hyprctl -j monitors all`. A miss is reported, never assumed.
const DISABLE_BUDGET: Duration = Duration::from_secs(3);

/// Re-enable the heads an `exclusive` session disabled. Run **before** that
/// member's output is removed, so Hyprland never sees zero enabled outputs.
///
/// `hyprctl reload` is the only thing that re-enables a disabled head. Re-
/// applying the mode/position/scale answers `ok` and leaves `disabled: true`;
/// a runtime `monitor` rule is additive and the `disable` in it keeps winning.
/// Only re-reading the config clears runtime rules.
///
/// A reload drops every runtime `hyprctl keyword`/`eval` override, including
/// our streamed-output rule (harmless — teardown removes the output next) and
/// any the operator set by hand; a hyprlang config re-runs `exec =` lines
/// (`exec-once` does not). Runs only when we actually disabled something.
fn restore_heads(disabled: &[String]) {
    if let Err(e) = hyprctl_dispatch(&["reload"]) {
        tracing::error!(
            ?disabled, error = %format!("{e:#}"),
            "hyprland: `hyprctl reload` failed — the heads this session disabled are still dark. \
             Re-run `hyprctl reload` by hand to get them back."
        );
        return;
    }
    // `reload` answers `ok` for "config parsed", not "the head came back" (a
    // head the operator's own config disables stays disabled). Read it back.
    let deadline = Instant::now() + DISABLE_BUDGET;
    let still_dark = loop {
        let dark: Vec<&String> = disabled
            .iter()
            .filter(|n| matches!(head_is_enabled(n), Ok(Some(false))))
            .collect();
        if dark.is_empty() || Instant::now() >= deadline {
            break dark;
        }
        thread::sleep(Duration::from_millis(50));
    };
    if still_dark.is_empty() {
        tracing::info!(
            ?disabled,
            "hyprland: re-enabled the heads `topology: exclusive` disabled"
        );
    } else {
        tracing::warn!(
            ?disabled, ?still_dark,
            "hyprland: `hyprctl reload` ran but these heads are still disabled — the operator's own \
             config may disable them, otherwise they need a manual `hyprctl reload`"
        );
    }
}

struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        // Close first: removing a head xdph is still capturing wedges its loop.
        stop_cast(&self.0);
        evacuate_workspace(&self.0);
        match hyprctl_dispatch(&["output", "remove", &self.0]) {
            Ok(_) => tracing::info!(output = %self.0, "hyprland headless output removed"),
            Err(e) => {
                tracing::warn!(output = %self.0, error = %format!("{e:#}"), "output remove failed")
            }
        }
    }
}

/// Budget for one `hyprctl` call ([`crate::proc`]). `hyprctl` waits on the
/// instance socket, so against a wedged compositor it never returns. These
/// run on the session's stream thread; a hung query wedges the session. Five
/// seconds is generous next to a healthy call (single-digit milliseconds).
const HYPRCTL_BUDGET: Duration = Duration::from_secs(5);

/// Budget for the one-shot xdph restart. `systemctl --user try-restart` waits
/// for the user manager's job; the result is already ignored.
const PORTAL_RESTART_BUDGET: Duration = Duration::from_secs(10);

/// Run `hyprctl <args>`, returning stdout. `HYPRLAND_INSTANCE_SIGNATURE` is set
/// on this child ([`hyprctl_command`]), not exported into the host. Non-zero on
/// hard failure; dispatch can print an error at status 0 — see [`hyprctl_dispatch`].
fn hyprctl(args: &[&str]) -> Result<String> {
    let mut cmd = hyprctl_command(args, crate::session::hypr_signature());
    let out = crate::proc::output_within(&mut cmd, HYPRCTL_BUDGET)
        .context("run hyprctl (is Hyprland installed?)")?;
    if !out.status.success() {
        bail!(
            "hyprctl {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `hyprctl -j <args>`, parsed.
fn hyprctl_json(args: &[&str]) -> Result<serde_json::Value> {
    let argv: Vec<&str> = std::iter::once("-j").chain(args.iter().copied()).collect();
    let raw = hyprctl(&argv)?;
    serde_json::from_str(&raw).with_context(|| format!("parse hyprctl -j {}", args.join(" ")))
}

/// Monitor `name` from `-j monitors`, or `-j monitors all` with `include_disabled` (the plain
/// listing drops a disabled head). `None` when it is not listed.
fn monitor(name: &str, include_disabled: bool) -> Result<Option<serde_json::Value>> {
    let args: &[&str] = if include_disabled {
        &["monitors", "all"]
    } else {
        &["monitors"]
    };
    let listed = hyprctl_json(args)?;
    Ok(listed.as_array().and_then(|monitors| {
        monitors
            .iter()
            .find(|m| m.get("name").and_then(|v| v.as_str()) == Some(name))
            .cloned()
    }))
}

/// `hyprctl` invocation with the live instance signature on the child.
///
/// `Command::env` gives it to exactly that child. A process-wide `set_var` was
/// a `getenv` data race with every other thread of a live host. `sig` is `None`
/// when no instance is findable: leave the child's env alone so an inherited
/// signature (host started inside the session) still wins.
fn hyprctl_command(args: &[&str], sig: Option<String>) -> Command {
    let mut cmd = Command::new("hyprctl");
    cmd.args(args);
    if let Some(sig) = sig {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
    }
    cmd
}

/// Every head Hyprland reports, for [`crate::monitors::list`].
///
/// `hyprctl -j monitors all` so disabled heads are listed too. Geometry is
/// post-transform in logical pixels, which is the space `crate::monitors`
/// documents.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let parsed = hyprctl_json(&["monitors", "all"])?;
    let mut out: Vec<_> = parsed
        .as_array()
        .context("hyprctl monitors: not an array")?
        .iter()
        .filter_map(|m| {
            let connector = m.get("name")?.as_str()?.to_string();
            let num = |k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            // `description` is already "make model (connector)"; treat it as
            // the make and let the helper drop it when empty/Unknown.
            let description = crate::monitors::describe(
                m.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "",
                &connector,
            );
            Some(crate::monitors::PhysicalMonitor {
                connector,
                description,
                width: num("width").max(0) as u32,
                height: num("height").max(0) as u32,
                // `refreshRate` is Hz as a float; we store millihertz.
                refresh_mhz: (m.get("refreshRate").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0)
                    as u32,
                x: num("x") as i32,
                y: num("y") as i32,
                scale: m
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: m.get("focused").and_then(|v| v.as_bool()).unwrap_or(false),
                enabled: !m.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false),
                // Named `PF-<pid>-<n>`; the shape is checked, not just the
                // prefix, so a user's `PF-office` stays theirs.
                managed: m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_managed_output),
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Run a `hyprctl` dispatch (`output …`, `keyword …`, `eval …`) that reports
/// success by printing `ok`. hyprctl often exits 0 on rejection, printing the
/// error to stdout — treat a known marker as failure (also how
/// [`set_monitor_rule`] tells the two config eras apart).
fn hyprctl_dispatch(args: &[&str]) -> Result<()> {
    let out = hyprctl(args)?;
    let t = out.trim();
    let lc = t.to_ascii_lowercase();
    if lc.contains("invalid")
        || lc.contains("not found")
        || lc.contains("couldn't")
        || lc.contains("could not")
        || lc.contains("unknown")
        || lc.contains("no such")
        || lc.contains("error")
        // `hyprctl eval` on hyprlang: "eval is only supported with the lua
        // config manager" — exit 0, no other marker.
        || lc.contains("only supported")
        || lc.contains("not supported")
        // Lua `keyword` answers "keyword can't work with non-legacy parsers"
        // at exit 0 — "can't", not the "couldn't" already covered. Without
        // this the wrong-era `keyword` read as success.
        || lc.contains("can't")
        || lc.contains("cannot")
    {
        bail!("hyprctl {:?} rejected: {t}", args);
    }
    Ok(())
}

/// Poll until `name` appears in `hyprctl -j monitors`. Create returns before it does.
fn wait_monitor_ready(name: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if monitor_exists(name)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("output create succeeded but monitor {name} never appeared");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Every monitor name, disabled included (`-j monitors all`). A leftover from
/// a dead host may have ended up disabled; [`reclaim_leftovers_once`] must see it.
fn monitor_names() -> Result<Vec<String>> {
    Ok(hyprctl_json(&["monitors", "all"])?
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

fn monitor_exists(name: &str) -> Result<bool> {
    Ok(monitor(name, false)?.is_some())
}

/// Colour rule for an HDR session's head: 10-bit, BT.2020 PQ, the EDID gates forced on
/// (a headless output has no EDID), SDR content at the configured nits, and the client
/// panel's volume as the mastering target when it sent one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HdrRule {
    sdr_nits: u32,
    /// Min cd/m² × 10000, max cd/m², max frame-average cd/m² — the client's panel.
    volume: Option<(u32, u32, u16)>,
}

impl HdrRule {
    fn new(client: Option<pf_frame::HdrMeta>) -> Self {
        HdrRule {
            sdr_nits: pf_host_config::config().gamescope_sdr_nits.unwrap_or(203),
            volume: client.map(|m| {
                (
                    m.min_display_mastering_luminance,
                    m.max_display_mastering_luminance / 10_000,
                    m.max_fall,
                )
            }),
        }
    }

    /// `key,value` pairs appended to the hyprlang `monitor` line.
    fn hyprlang(&self) -> String {
        let mut s = format!(
            ",bitdepth,10,cm,hdr,supports_wide_color,1,supports_hdr,1,sdr_max_luminance,{}",
            self.sdr_nits
        );
        if let Some((min, max, avg)) = self.volume {
            s.push_str(&format!(",min_luminance,{}", f64::from(min) / 10_000.0));
            if max > 0 {
                s.push_str(&format!(",max_luminance,{max}"));
            }
            if avg > 0 {
                s.push_str(&format!(",max_avg_luminance,{avg}"));
            }
        }
        s
    }

    /// `key = value` fields appended inside `hl.monitor{}`.
    fn lua(&self) -> String {
        let mut s = format!(
            ", bitdepth = 10, cm = \"hdr\", supports_wide_color = 1, supports_hdr = 1, \
             sdr_max_luminance = {}",
            self.sdr_nits
        );
        if let Some((min, max, avg)) = self.volume {
            s.push_str(&format!(", min_luminance = {}", f64::from(min) / 10_000.0));
            if max > 0 {
                s.push_str(&format!(", max_luminance = {max}"));
            }
            if avg > 0 {
                s.push_str(&format!(", max_avg_luminance = {avg}"));
            }
        }
        s
    }
}

/// A `hyprctl` `currentFormat` with ten bits per channel.
fn ten_bit_format(format: &str) -> bool {
    format.contains("2101010") || format.contains("1010102")
}

/// The head's `currentFormat` from `hyprctl -j monitors all`, or `None` if absent.
fn monitor_format(name: &str) -> Option<String> {
    monitor(name, true)
        .ok()
        .flatten()
        .and_then(|m| m.get("currentFormat")?.as_str().map(str::to_owned))
}

/// [`set_monitor_rule`], HDR first. A compositor that rejects the colour keys, or backs
/// them with an 8-bit framebuffer, gets the SDR rule instead; the latch tells the next
/// handshake, and this session ends at its first HDR capture with the reconnect advice.
fn apply_monitor_rule(name: &str, mode: Mode, colour: Option<HdrRule>) -> Result<Option<HdrRule>> {
    let Some(rule) = colour else {
        return set_monitor_rule(name, mode, None).map(|()| None);
    };
    let lit = match set_monitor_rule(name, mode, Some(rule)) {
        Ok(()) => monitor_format(name).is_some_and(|f| ten_bit_format(&f)),
        Err(e) => {
            tracing::debug!(output = %name, error = %format!("{e:#}"), "HDR monitor rule rejected");
            false
        }
    };
    if lit {
        return Ok(Some(rule));
    }
    tracing::warn!(
        output = %name,
        "Hyprland did not light the streamed head in 10-bit HDR — streaming SDR; this host \
         offers SDR until it restarts"
    );
    pf_capture::note_hdr_capture_failed(pf_capture::HdrSource::VirtualOutput);
    set_monitor_rule(name, mode, None).map(|()| None)
}

/// Set the client's exact mode on `name`, both config eras.
///
/// `hyprctl keyword monitor NAME,WxH@Hz,auto,1` is hyprlang (the default,
/// including ≥0.55 — version does not imply Lua). Fall back to
/// `hyprctl eval 'hl.monitor{…}'` only when `keyword` is gone. Either way,
/// confirm the output adopted the mode — some forms print `ok` for a command
/// they ignored. A headless output starts at 0×0; if neither form yields a
/// usable size, the compositor could not back the mode. `colour` appends the
/// HDR keys to both forms.
fn set_monitor_rule(name: &str, mode: Mode, colour: Option<HdrRule>) -> Result<()> {
    let hz = mode.refresh_hz.max(1);
    let spec = format!(
        "{name},{}x{}@{hz},auto,1{}",
        mode.width,
        mode.height,
        colour.map(|c| c.hyprlang()).unwrap_or_default()
    );
    let lua = format!(
        "hl.monitor{{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"auto\", scale = 1{} }}",
        mode.width,
        mode.height,
        colour.map(|c| c.lua()).unwrap_or_default()
    );
    let keyword: Vec<&str> = vec!["keyword", "monitor", &spec];
    let eval: Vec<&str> = vec!["eval", &lua];
    // hyprctl reports rejection in the output text. Dropping it left the
    // failure below guessing at GBM when the compositor had named the cause.
    let mut attempts: Vec<String> = Vec::new();
    for a in [&keyword, &eval] {
        // Wrong-era command (`keyword` gone under Lua, or `eval` under
        // hyprlang) — skip to the other form.
        if let Err(e) = hyprctl_dispatch(a) {
            let said = format!("{e:#}");
            tracing::debug!(output = %name, cmd = ?a, error = %said, "hyprctl rejected this monitor-rule form — trying the other config era");
            attempts.push(said);
            continue;
        }
        if wait_exact_mode(name, mode, Duration::from_millis(1500)) {
            tracing::debug!(output = %name, cmd = ?a, w = mode.width, h = mode.height, "monitor adopted the requested mode");
            return Ok(());
        }
        attempts.push(format!(
            "hyprctl {a:?} was accepted but the mode never took effect"
        ));
    }
    let said = if attempts.is_empty() {
        "nothing (no form was attempted)".to_string()
    } else {
        attempts.join("; ")
    };
    // Distinguish "usable but different size" (stream anyway) from "0×0 /
    // gone" (no framebuffer at all).
    match monitor_size(name)? {
        Some((w, h)) if w > 0 && h > 0 => {
            tracing::warn!(
                output = %name,
                requested = %format!("{}x{}", mode.width, mode.height),
                got = %format!("{w}x{h}"),
                hyprctl = %said,
                "Hyprland did not adopt the exact requested mode — streaming at the output's current size"
            );
            Ok(())
        }
        // Lead with what hyprctl said: if every form was rejected, no
        // allocation was attempted. Only an accepted form that left 0×0
        // points at the compositor failing to back the mode.
        _ => bail!(
            "headless output {name} never got a framebuffer (stayed 0x0) after the monitor rule for \
             {}x{}@{hz}. hyprctl said: {said}. If a form was accepted, the compositor could not back \
             the mode — likely a headless GBM/dmabuf allocation failure (GPU driver; cf. \
             Sunshine#4197). Check the Hyprland log.",
            mode.width,
            mode.height
        ),
    }
}

/// Poll until `name` reports exactly `mode`'s width×height (the rule applies
/// asynchronously). `false` on timeout.
fn wait_exact_mode(name: &str, mode: Mode, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(monitor_size(name), Ok(Some((w, h))) if w == mode.width as u64 && h == mode.height as u64)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// `(width, height)` from `hyprctl -j monitors all` (includes disabled), or
/// `None` if absent. A fresh headless output reports `0×0` until a mode commits.
fn monitor_size(name: &str) -> Result<Option<(u64, u64)>> {
    let dim = |m: &serde_json::Value, k: &str| m.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(monitor(name, true)?.map(|m| (dim(&m, "width"), dim(&m, "height"))))
}

/// Running Hyprland `(major, minor, patch)` from `hyprctl -j version`, for a
/// diagnostic log — the mode-rule path is version-independent.
fn hyprland_version() -> Option<(u16, u16, u16)> {
    let json = hyprctl_json(&["version"]).ok()?;
    parse_version_tag(json.get("tag").and_then(|t| t.as_str())?)
}

/// Parse a Hyprland `tag` (`v0.55.4`, or a dev `v0.41.2-13-gabcdef`).
fn parse_version_tag(tag: &str) -> Option<(u16, u16, u16)> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let mut it = t.split(['.', '-', '_', '+']);
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Permission-system caveat at most once per process: with
/// `ecosystem.enforce_permissions = true` (0.49+, off by default), denial is
/// silent black frames / dropped input, not an error.
fn preflight_once() {
    static WARNED: Once = Once::new();
    WARNED.call_once(warn_if_permissions_enforced);
}

fn warn_if_permissions_enforced() {
    let on = hyprctl_json(&["getoption", "ecosystem:enforce_permissions"])
        .ok()
        .and_then(|j| j.get("int").and_then(|v| v.as_i64()))
        .is_some_and(|v| v != 0);
    if on {
        tracing::warn!(
            "Hyprland ecosystem.enforce_permissions is ON — screencopy/virtual-input may be denied \
             as SILENT black frames / dropped input. Grant the host with hl.permission rules \
             (screencopy + virtual pointer/keyboard) — see docs/hyprland."
        );
    }
}

/// Point xdph at our custom picker: install the shim and write the managed
/// config, restarting xdph if the config changed (it reads config only at
/// startup).
///
/// The picker is borrowed, not taken. `custom_picker_binary` is one key; a
/// distro that ships its own (every Chromium share on the box) would have
/// every share pointed at us. The shim delegates: with no selection pending
/// it `exec`s whatever was configured before us. The config edit records
/// what it replaced, so [`restore_xdph_config`] can put it back.
fn ensure_xdph_config() -> Result<()> {
    let path = xdph_config_path()?;
    // Prior value: our marker if we have already taken over, else whatever
    // is in the file. Marker first, or a second takeover records our shim
    // as "theirs".
    let (current, prior) = crate::portal_config::peek(&path, XDPH_BLOCK, XDPH_PICKER_KEY);
    let fallback = match prior {
        Some(p) => p,
        None => current,
    }
    .filter(|c| picker_is_plain(c));

    // Install the picker shim (idempotent — content is fixed for a given fallback).
    let shim = picker_shim_path();
    let sel = selection_file();
    // `-s` not `-f`: empty file means "no selection". Unquoted `exec` is
    // deliberate — a picker may carry flags, and `picker_is_plain` is what
    // makes word-splitting the only thing that can happen.
    let shim_body = match &fallback {
        Some(cmd) => format!(
            "#!/bin/sh\n# Managed by punktfunk. Hands xdph the output this host is streaming; with\n# no selection pending, defers to the picker configured before us.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\nexec {cmd} \"$@\"\n"
        ),
        None => format!(
            "#!/bin/sh\n# Managed by punktfunk.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\nexit 0\n"
        ),
    };
    if std::fs::read_to_string(&shim).is_ok_and(|c| c == shim_body) {
    } else {
        // Mode at creation, not chmod after: xdph executes this file, and
        // write-then-chmod leaves it briefly at the umask default.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(&shim)
            .with_context(|| format!("write {shim}"))?;
        f.write_all(shim_body.as_bytes())
            .with_context(|| format!("write {shim}"))?;
    }

    // One key, in place. Overwriting the whole file would destroy every
    // other xdph setting the user owned.
    let changed = crate::portal_config::ensure_key(&path, XDPH_BLOCK, XDPH_PICKER_KEY, &shim)?;
    if !changed {
        return Ok(());
    }
    tracing::info!(
        path = %path.display(),
        defers_to = fallback.as_deref().unwrap_or("(xdph's own fallback)"),
        "pointed xdg-desktop-portal-hyprland at the managed picker shim"
    );
    restart_xdph();
    Ok(())
}

/// Hand `custom_picker_binary` back and restart xdph (it reads config only at
/// startup). Called from the host's shutdown path, never per cast: a ScreenCast
/// bound across the restart never delivers a buffer. Safe on a box we never
/// touched (no-op).
///
/// The restart is the cost: xdph cannot tell us whether another application's
/// cast is live, so a share started during our session can be cut. Leaving
/// xdph pointed at a shim whose selection file is gone breaks screen sharing
/// until the next login.
pub(crate) fn restore_picker_on_shutdown() {
    restore_xdph_config();
}

fn restore_xdph_config() {
    let Ok(path) = xdph_config_path() else { return };
    match crate::portal_config::restore_key(&path, XDPH_BLOCK, XDPH_PICKER_KEY) {
        Ok(false) => return, // not ours; nothing to undo
        Ok(true) => tracing::info!(
            path = %path.display(),
            "restored the screen-share picker xdg-desktop-portal-hyprland had before this host"
        ),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"),
                "previous screen-share picker not restored");
            return;
        }
    }
    restart_xdph();
}

/// Bounded: `systemctl --user` blocks on the user manager's job queue, and
/// this runs on the session's stream thread. A timeout just means xdph picks
/// the new config up whenever it next starts.
fn restart_xdph() {
    let _ = crate::proc::status_within(
        Command::new("systemctl").args([
            "--user",
            "try-restart",
            "xdg-desktop-portal-hyprland.service",
        ]),
        PORTAL_RESTART_BUDGET,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The re-apply hangs off this one-line match. Too strict and a reload
    /// still resets the stream; too loose (`contains`) and a window titled
    /// `configreloaded` triggers a `hyprctl` round trip on every retitle.
    #[test]
    fn only_the_config_reload_event_re_applies_the_monitor_rule() {
        assert!(is_config_reload("configreloaded>>"));
        // Real `.socket2.sock` lines, none of which is a reload.
        assert!(!is_config_reload("monitoradded>>PF-1234-1"));
        assert!(!is_config_reload("monitorremovedv2>>3,PF-1234-1,PF-1234-1"));
        assert!(!is_config_reload("activewindow>>kitty,~/src"));
        // The `contains` trap: the word is in the DATA, not the event name.
        assert!(!is_config_reload("activewindowv2>>title: configreloaded"));
        assert!(!is_config_reload("workspace>>configreloaded"));
    }

    /// Lua config manager parses a `dispatch` argument as a Lua expression,
    /// so both arguments must be quoted. Pinning the shape because quoting is
    /// the entire difference between working and silently doing nothing.
    #[test]
    fn the_lua_dpms_expression_quotes_both_arguments() {
        assert_eq!(
            lua_dpms_expr("HDMI-A-1", false),
            r#"hl.dsp.dpms("off", "HDMI-A-1")"#
        );
        assert_eq!(lua_dpms_expr("DP-2", true), r#"hl.dsp.dpms("on", "DP-2")"#);
        // Never omit the monitor name: the no-name form answers `ok` and
        // toggles, which would flip a just-restored head back off.
        assert!(lua_dpms_expr("DP-2", true).contains("\"DP-2\""));
    }

    /// Lua spellings of the resize workspace hand-off. Both dispatchers answer
    /// `ok` on the wrong era, so a drifted string fails silently. Workspace id
    /// is a quoted string in both; `hl.dsp.workspace.move` requires `monitor`,
    /// and there is no `hl.dsp.workspace` member that merely switches — the
    /// switch half goes through `hl.dsp.focus`.
    #[test]
    fn the_lua_workspace_expressions_quote_their_arguments() {
        assert_eq!(
            lua_workspace_move_expr("2", "PF-1234-2"),
            r#"hl.dsp.workspace.move({ workspace = "2", monitor = "PF-1234-2" })"#
        );
        assert_eq!(
            lua_workspace_focus_expr("2"),
            r#"hl.dsp.focus({ workspace = "2" })"#
        );
    }

    #[test]
    fn version_tag_parses_release_and_dev_builds() {
        assert_eq!(parse_version_tag("v0.55.0"), Some((0, 55, 0)));
        assert_eq!(parse_version_tag("0.41.2"), Some((0, 41, 2)));
        // Dev builds tack the commit distance + hash on with a dash.
        assert_eq!(parse_version_tag("v0.41.2-13-gabcdef"), Some((0, 41, 2)));
        // Missing patch defaults to 0; garbage is rejected.
        assert_eq!(parse_version_tag("v1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version_tag("wat"), None);
    }

    /// `focusmonitor` is a dispatcher, so it must go through `hyprctl dispatch`.
    /// A bare `hyprctl focusmonitor NAME` is not a subcommand and hyprctl
    /// reports it with exit 0.
    #[test]
    fn focus_goes_through_the_dispatch_subcommand() {
        assert_eq!(
            focus_argv("PF-1234-1"),
            ["dispatch", "focusmonitor", "PF-1234-1"]
        );
    }

    /// Lua-era spelling. The key is `monitor` (the compositor lists the
    /// alternatives when it is anything else) and the name must be quoted —
    /// unquoted is the classic form's failure, `')' expected near 'PF'`.
    #[test]
    fn the_lua_focus_expression_quotes_the_monitor_name() {
        assert_eq!(
            lua_focus_expr("PF-1234-1"),
            "hl.dsp.focus({ monitor = \"PF-1234-1\" })"
        );
        // The two eras must not converge on one string: each is rejected by
        // the other's parser, which is what makes "try one, then the other" safe.
        assert_ne!(lua_focus_expr("PF-1"), focus_argv("PF-1").join(" "));
    }

    /// `HYPRLAND_INSTANCE_SIGNATURE` reaches `hyprctl` as a per-child override,
    /// never as a `set_var` on the host — that write was a `getenv` data race
    /// with every other thread of a live session. A discovered signature is
    /// set on the child; an undiscoverable one leaves the child's env untouched.
    #[test]
    fn the_instance_signature_travels_on_the_child_not_the_process_env() {
        let overrides = |sig: Option<String>| -> Vec<(String, Option<String>)> {
            hyprctl_command(&["-j", "version"], sig)
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
            overrides(Some("abc123".to_string())),
            [(
                "HYPRLAND_INSTANCE_SIGNATURE".to_string(),
                Some("abc123".to_string())
            )]
        );
        assert!(overrides(None).is_empty());
    }

    #[test]
    fn output_names_are_unique_and_prefixed() {
        let a = next_output_name();
        let b = next_output_name();
        assert!(a.starts_with("PF-") && b.starts_with("PF-"));
        assert_ne!(a, b);
    }

    /// The name carries the creating host's pid, which is what makes a leftover
    /// attributable. A reclaim that could not tell whose it was would have to
    /// remove a live sibling's or nothing at all.
    #[test]
    fn a_name_carries_its_owner_pid_and_only_ours_does() {
        let mine = next_output_name();
        assert_eq!(output_owner_pid(&mine), Some(std::process::id()));
        assert!(is_managed_output(&mine));

        // Legacy `PF-<n>`: recognisably managed, but no owner — report, never
        // reclaim on a guess.
        assert!(is_managed_output("PF-1"));
        assert_eq!(output_owner_pid("PF-1"), None);

        // A user's own monitor that happens to start with the prefix, and the
        // connectors every wlr-family compositor mints.
        for theirs in ["PF-office", "PF-", "PF-12-abc", "HEADLESS-1", "DP-1", ""] {
            assert!(!is_managed_output(theirs), "{theirs:?} is not ours");
            assert_eq!(output_owner_pid(theirs), None, "{theirs:?} has no owner");
        }
    }

    /// The backend hands the picker exactly what [`crate::portal_picker`] says.
    /// That module owns the format and its tests run on every platform.
    #[test]
    fn picker_line_is_the_shared_selection_format() {
        assert_eq!((SELECTOR.line)("PF-1"), "[SELECTION]/screen:PF-1\n");
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
            // Real `list_monitors` derives this with `is_managed_output`;
            // mirror it so the fixture cannot drift from the backend's rule.
            managed: is_managed_output(connector),
        }
    }

    /// `exclusive` disables the operator's heads and only those. A sibling
    /// session's output — ours or another host's, both `PF-<pid>-<n>` — must
    /// survive, or the second exclusive session blacks out the first.
    #[test]
    fn exclusive_disables_the_operators_heads_and_never_a_managed_sibling() {
        let ours = "PF-4242-1";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            // A concurrent session's output, and one from a second host — both managed.
            head("PF-4242-2", true),
            head("PF-99-1", true),
            // Already off: must not end up in the restore list, or teardown
            // would switch on a head the operator had left dark.
            head("DP-3", false),
        ];
        assert_eq!(
            crate::monitors::heads_to_darken(&heads, ours, &[]),
            vec!["DP-1", "HDMI-A-1"]
        );
    }

    /// A box with no physical head has nothing to disable, so no restore is
    /// prepared and teardown never runs a `hyprctl reload`.
    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "PF-4242-1";
        assert!(crate::monitors::heads_to_darken(&[head(ours, true)], ours, &[]).is_empty());
    }

    /// Both config eras, pinned. `hyprctl` answers a wrong-era or malformed
    /// rule at exit 0, so a typo here reads as success and the operator's
    /// screen stays lit under `exclusive`.
    #[test]
    fn disable_rules_are_pinned_for_both_config_eras() {
        assert_eq!(disable_rule_spec("DP-1"), "DP-1,disable");
        assert_eq!(
            disable_lua_expr("DP-1"),
            r#"hl.monitor{ output = "DP-1", disabled = true }"#
        );
    }

    /// Real `hyprctl -j clients` shape, trimmed to the fields the list reads.
    /// Two eras of `fullscreen` (bool and mode int) ride here on purpose.
    const CLIENTS: &str = r#"[
      {"address":"0x55a1","mapped":true,"hidden":false,"workspace":{"id":3,"name":"3"},
       "monitor":0,"class":"steam_app_570","title":"Dota 2","pid":4242,
       "focusHistoryID":0,"fullscreen":2},
      {"address":"0x55a2","mapped":true,"hidden":false,"workspace":{"id":3,"name":"3"},
       "monitor":0,"class":"steam","title":"Steam","pid":99,
       "focusHistoryID":1,"fullscreen":false},
      {"address":"0x55a3","mapped":true,"hidden":false,"workspace":{"id":1,"name":"1"},
       "monitor":1,"class":"discord","title":"Private call — Ada","pid":7,
       "focusHistoryID":2,"fullscreen":false},
      {"address":"0x55a4","mapped":false,"hidden":false,"workspace":{"id":3,"name":"3"},
       "class":"ghost","title":"not mapped","pid":8,"focusHistoryID":3}
    ]"#;

    /// Only the streamed head's windows leave this parser. The operator's own
    /// monitor carries the titles a guest must never be handed.
    #[test]
    fn a_window_list_holds_the_streamed_head_and_nothing_else() {
        let clients: serde_json::Value = serde_json::from_str(CLIENTS).unwrap();
        let spaces: serde_json::Value = serde_json::from_str(WORKSPACES).unwrap();
        let list = parse_clients(&clients, &spaces, Some("PF-1234-1"));
        assert_eq!(
            list.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["0x55a1", "0x55a2"],
            "an unmapped window and the operator's desk are both out"
        );
        let game = &list[0];
        assert_eq!(game.title, "Dota 2");
        assert_eq!(game.app_id, "steam_app_570");
        assert_eq!(game.pid, Some(4242));
        assert_eq!(game.workspace, "3");
        assert_eq!(game.output, "PF-1234-1");
        assert!(game.focused, "focus stack head is the focused window");
        assert!(game.fullscreen, "mode 2 is full-screen");
        assert!(!list[1].focused);
        // The private title is on DP-1, and DP-1 is not what this session streams.
        assert!(!list.iter().any(|w| w.title.contains("Private")));
    }

    /// A workspace no `workspaces` payload claims has no head we can prove, so
    /// the window is dropped rather than guessed onto the streamed one.
    #[test]
    fn a_window_on_an_unknown_workspace_is_never_assumed_to_be_ours() {
        let clients: serde_json::Value = serde_json::from_str(
            r#"[{"address":"0x1","workspace":{"id":77,"name":"77"},"class":"x","title":"t"}]"#,
        )
        .unwrap();
        let spaces: serde_json::Value = serde_json::from_str(WORKSPACES).unwrap();
        assert!(parse_clients(&clients, &spaces, Some("PF-1234-1")).is_empty());
    }

    /// Both spellings of `fullscreen`, and the field missing entirely.
    #[test]
    fn fullscreen_reads_the_bool_era_and_the_mode_era() {
        assert!(!is_fullscreen(None));
        assert!(!is_fullscreen(Some(&serde_json::json!(false))));
        assert!(is_fullscreen(Some(&serde_json::json!(true))));
        assert!(!is_fullscreen(Some(&serde_json::json!(0))));
        assert!(is_fullscreen(Some(&serde_json::json!(1))));
        assert!(is_fullscreen(Some(&serde_json::json!(2))));
    }

    /// The window token must see window events and ignore the reload the same
    /// reader is there for — and neither may fire on a window merely titled so.
    #[test]
    fn the_window_token_matches_event_names_not_window_titles() {
        assert!(is_window_event("openwindow>>55a1,3,kitty,kitty"));
        assert!(is_window_event("windowtitlev2>>55a1,Dota 2"));
        assert!(is_window_event("activewindow>>kitty,~/src"));
        assert!(!is_window_event("configreloaded>>"));
        // A NAME match, so an event whose PAYLOAD names a window event does not
        // count — the same trap `is_config_reload` exists to avoid.
        assert!(!is_window_event("workspace>>openwindow"));
        assert!(!is_window_event("createworkspace>>closewindow"));
        // The reload path must not have moved.
        assert!(is_config_reload("configreloaded>>"));
        assert!(!is_config_reload("openwindow>>55a1,3,kitty,kitty"));
    }

    /// With no watcher on the socket the token is frozen, and a frozen token
    /// would pin one stale list for the session's life.
    #[test]
    fn the_window_token_admits_when_no_reader_is_listening() {
        assert_eq!(window_gen(), None, "no watcher was spawned in this test");
    }

    /// Real `hyprctl -j workspaces` shape, trimmed to the fields the pick reads.
    const WORKSPACES: &str = r#"[
      {"id":1,"name":"1","monitor":"DP-1","windows":3,"lastwindowtitle":"Discord"},
      {"id":2,"name":"2","monitor":"DP-1","windows":0,"lastwindowtitle":""},
      {"id":3,"name":"3","monitor":"PF-1234-1","windows":2,"lastwindowtitle":"kitty"},
      {"id":-99,"name":"special:magic","monitor":"DP-1","windows":1}
    ]"#;

    /// The streamed head's own workspaces decide; DP-1's empty one is the
    /// operator's, and an unreadable `windows` count must read as occupied.
    #[test]
    fn a_launch_lands_on_an_empty_workspace_of_the_streamed_head() {
        let parsed: serde_json::Value = serde_json::from_str(WORKSPACES).unwrap();
        let slots = workspace_slots(&parsed, "PF-1234-1");
        // Nothing empty on our head: the next free id, minted empty on focus.
        assert_eq!(crate::routing::pick_workspace(&slots, 3), 4);
        // Same payload, the operator's head: its own empty 2 wins.
        let slots = workspace_slots(&parsed, "DP-1");
        assert_eq!(crate::routing::pick_workspace(&slots, 1), 2);
        // A head nothing reports has no empty workspace to reuse.
        let slots = workspace_slots(&parsed, "PF-9999-1");
        assert_eq!(crate::routing::pick_workspace(&slots, 7), 4);
    }

    /// A payload without the count is not "every workspace is free".
    #[test]
    fn a_workspace_with_no_window_count_is_never_treated_as_empty() {
        let parsed: serde_json::Value =
            serde_json::from_str(r#"[{"id":1,"monitor":"PF-1"}]"#).unwrap();
        let slots = workspace_slots(&parsed, "PF-1");
        assert!(!slots[0].empty);
        assert_eq!(crate::routing::pick_workspace(&slots, 1), 2);
    }

    /// Classic argv for the re-home that runs before `output remove`. Both
    /// dispatchers answer `ok` on the wrong era, so a drifted string is silent.
    #[test]
    fn evacuate_argv_moves_the_workspace_then_focuses_it() {
        assert_eq!(
            evacuate_move_argv("2", "DP-1"),
            ["dispatch", "moveworkspacetomonitor", "2", "DP-1"]
        );
        assert_eq!(evacuate_focus_argv("2"), ["dispatch", "workspace", "2"]);
        assert_eq!(
            lua_workspace_move_expr("2", "DP-1"),
            r#"hl.dsp.workspace.move({ workspace = "2", monitor = "DP-1" })"#
        );
        assert_eq!(
            lua_workspace_focus_expr("2"),
            r#"hl.dsp.focus({ workspace = "2" })"#
        );
    }

    /// A remaining physical gets the streamed workspace. Headless (or an
    /// already-gone workspace) skips the move — windows stay in limbo.
    #[test]
    fn evacuate_rehomes_onto_a_physical_and_skips_when_headless() {
        assert_eq!(
            evacuate_plan(Some(3), Some("HDMI-A-1")),
            Evacuate::ToPhysical {
                workspace: 3,
                dest: "HDMI-A-1".into(),
            }
        );
        assert_eq!(evacuate_plan(Some(3), None), Evacuate::Limbo);
        assert_eq!(evacuate_plan(None, Some("DP-1")), Evacuate::Limbo);
        assert_eq!(evacuate_plan(None, None), Evacuate::Limbo);
    }

    /// Re-home aims at the operator's first enabled physical, never a managed
    /// sibling and never a head exclusive already darkened.
    #[test]
    fn evacuate_picks_the_first_remaining_physical() {
        let ours = "PF-4242-1";
        let heads = [
            head("DP-1", false),
            head("PF-99-1", true),
            head(ours, true),
            head("HDMI-A-1", true),
            head("DP-2", true),
        ];
        assert_eq!(
            first_physical_dest(&heads, ours).as_deref(),
            Some("HDMI-A-1")
        );
        assert!(first_physical_dest(&[head(ours, true)], ours).is_none());
    }

    /// Reconnect recasts only a named head of its own identity; anything else creates.
    #[test]
    fn a_reconnect_reuses_only_a_named_head_of_its_own_identity() {
        let display = HyprlandDisplay::new().unwrap();
        assert!(display.accepts_kept(None, Some("PF-1-1")));
        assert!(!display.accepts_kept(None, None));
        assert!(!display.accepts_kept(Some(1), Some("PF-1-1")));
    }

    /// A reconnect backend must adopt from the pooled head when a later mode
    /// rebuild creates its replacement.
    #[test]
    fn reconnect_then_rebuild_adopts_the_reused_outputs_workspace() {
        let mut display = HyprlandDisplay::new().unwrap();
        let cast = display
            .session_cast_for_with("PF-1-1", |_, _| {
                let (fd, peer) = UnixStream::pair()?;
                drop(peer);
                Ok((fd.into(), 42, crate::portal_cursor::Mode::Embedded))
            })
            .unwrap()
            .unwrap();

        assert_eq!(display.replace_output("PF-1-2").as_deref(), Some("PF-1-1"));
        assert_eq!(display.prev_output.as_deref(), Some("PF-1-2"));
        drop(cast);
    }

    /// `hyprctl keyword` under Lua answers "keyword can't work with non-legacy
    /// parsers. Use eval." at exit 0. Without this marker the wrong-era form
    /// reports success.
    #[test]
    fn a_wrong_era_rejection_is_an_error_not_a_success() {
        for said in [
            "keyword can't work with non-legacy parsers. Use eval.",
            "eval is only supported with the lua config manager",
            "invalid resolution ",
        ] {
            let lc = said.to_ascii_lowercase();
            assert!(
                lc.contains("can't")
                    || lc.contains("cannot")
                    || lc.contains("only supported")
                    || lc.contains("invalid"),
                "{said:?} must match a marker in hyprctl_dispatch"
            );
        }
    }
}

#[cfg(test)]
mod hdr_rule_tests {
    use super::{ten_bit_format, HdrRule};

    #[test]
    fn the_hdr_rule_forces_the_edid_gates_a_headless_output_cannot_pass() {
        let rule = HdrRule {
            sdr_nits: 203,
            volume: None,
        };
        assert_eq!(
            rule.hyprlang(),
            ",bitdepth,10,cm,hdr,supports_wide_color,1,supports_hdr,1,sdr_max_luminance,203"
        );
        assert_eq!(
            rule.lua(),
            ", bitdepth = 10, cm = \"hdr\", supports_wide_color = 1, supports_hdr = 1, \
             sdr_max_luminance = 203"
        );
    }

    #[test]
    fn the_client_panel_volume_becomes_the_mastering_target() {
        // 0.005 cd/m² min, 1000 cd/m² max, 400 cd/m² frame average.
        let rule = HdrRule {
            sdr_nits: 203,
            volume: Some((50, 1000, 400)),
        };
        assert!(rule
            .hyprlang()
            .ends_with(",min_luminance,0.005,max_luminance,1000,max_avg_luminance,400"));
        assert!(rule
            .lua()
            .ends_with(", min_luminance = 0.005, max_luminance = 1000, max_avg_luminance = 400"));
        // Unknown max and average (0) are left to Hyprland's defaults.
        let unknown = HdrRule {
            sdr_nits: 203,
            volume: Some((50, 0, 0)),
        };
        assert!(unknown.hyprlang().ends_with(",min_luminance,0.005"));
    }

    #[test]
    fn only_a_ten_bit_current_format_counts_as_lit() {
        assert!(ten_bit_format("XBGR2101010"));
        assert!(ten_bit_format("ABGR2101010"));
        assert!(ten_bit_format("RGBA1010102"));
        assert!(!ten_bit_format("XRGB8888"));
        assert!(!ten_bit_format("Invalid"));
    }
}
