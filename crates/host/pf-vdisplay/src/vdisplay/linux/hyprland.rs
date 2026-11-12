//! Hyprland virtual-output backend via `hyprctl` IPC and the xdg ScreenCast portal
//! (xdg-desktop-portal-hyprland / xdph). See `design/hyprland-support.md`.
//!
//! Distinct from [`super::wlroots`]: Hyprland names headless outputs explicitly
//! (`hyprctl output create headless <name>`, [`head_name`]), so there is no before/after
//! diff, and the output is journaled ([`crate::topology_journal`]) before it exists.
//! The heads `exclusive` disables are journaled before the first goes dark.
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
use crate::monitors::DISABLE_BUDGET;
use crate::portal_cast::StopGuard;
use crate::topology_journal::{self as journal, Head, Undo};
use anyhow::{bail, Context, Result};
use std::io::BufRead;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Once;
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

/// Can `cmd` sit bare on the shim's `exec` line? Anything else is quoted ([`exec_line`]). A
/// newline would split the generated script; this is robustness, not a privilege boundary.
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

/// `PF-<pid>-<n>`: the name of a head with no slot, and of a second head while the device's
/// slot head is still up (a rebuild creates before it drops). The pid is the reclaim's key for
/// these; a slot head's is the journal. niri mints its outputs from the same counter.
pub(crate) fn next_output_name() -> String {
    format!(
        "PF-{}-{}",
        std::process::id(),
        OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// The name a create gives its head. A device with a slot takes its standing name `PF-<slot>`,
/// which compositor config can key on, unless the compositor lists it already (the device's own
/// head during a rebuild, or a leftover the journal has not taken back); then, as with no slot,
/// the counter's. Nothing is removed here: a listed head may be the registry's.
pub(crate) fn head_name(slot: Option<u32>, listed: impl FnOnce(&str) -> bool) -> String {
    match slot.map(|s| format!("PF-{s}")) {
        Some(standing) if !listed(&standing) => standing,
        _ => next_output_name(),
    }
}

/// `PF-<slot>` or `PF-<pid>-<n>`. A user's own `PF-office` must not match.
pub(crate) fn is_managed_output(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("PF-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// One named headless output per [`create`](VirtualDisplay::create). Stateless
/// besides the fields below.
pub struct HyprlandDisplay {
    /// Whose display this is. Set by `set_client_identity` before `create`, so the
    /// per-device topology (`design/web-console-overhaul.md` §6.1) can be resolved here.
    client_fp: Option<[u8; 32]>,
    /// The session's topology ([`VirtualDisplay::set_topology`]).
    topology: Option<crate::policy::Topology>,
    /// The identity slot `create` resolves for `client_fp`, peeked by `set_client_identity`
    /// before it: the owner key of this output once kept, and the budget's own-display exemption.
    last_slot: Option<u32>,

    /// Out-of-band cursor request. On: prefer portal `CursorMode::Metadata`.
    /// Off: prefer `Embedded` (compositor paints the pointer). Both are only a
    /// preference — [`crate::portal_cursor`] settles against what xdph advertises;
    /// an unadvertised mode fails the portal call. Current xdph advertises
    /// `Hidden|Embedded` only, so every session here resolves to `Embedded`.
    hw_cursor: bool,
    /// What the portal actually gave the last successful `create`. How the host
    /// learns a cursor overlay is never coming.
    last_cursor_mode: Option<crate::portal_cursor::Mode>,
    /// Journal seqs of the heads `exclusive` disabled, across every `create` of this
    /// instance: one instance serves the host retry loop, and only attempt 1 finds heads
    /// to disable. The registry takes them after a pooled `create` and runs them when the
    /// group empties, so the heads stay dark for the linger window; [`Drop`] runs what a
    /// create that never reached the pool left.
    pending_undo: Vec<u64>,
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
    /// The last `create`'s head and its rule, which [`apply_position`] re-issues at a place.
    ///
    /// [`apply_position`]: VirtualDisplay::apply_position
    placed: Option<Placed>,
}

/// One head's monitor rule: name, mode, colour, and the place a reload re-applies.
struct Placed {
    name: String,
    mode: Mode,
    colour: Option<HdrRule>,
    at: Position,
}

/// Where a head sits on the desktop; `None` lets Hyprland place it (`auto`).
type Position = std::sync::Arc<std::sync::Mutex<Option<(i32, i32)>>>;

impl Drop for HyprlandDisplay {
    fn drop(&mut self) {
        // A registry create that never completed its handoff still owns this cast: close it
        // before the undos run.
        drop(self.pending_cast.take());
        let seqs = std::mem::take(&mut self.pending_undo);
        journal::apply(&seqs, self);
    }
}

impl HyprlandDisplay {
    pub fn new() -> Result<Self> {
        Ok(HyprlandDisplay {
            client_fp: None,
            topology: None,
            last_slot: None,
            hw_cursor: false,
            last_cursor_mode: None,
            pending_undo: Vec::new(),
            prev_output: None,
            pending_cast: None,
            handoff_cast: false,
            join_live: false,
            hdr: false,
            client_hdr: None,
            placed: None,
        })
    }

    /// Attach a cast to `name`: the one `create` parked for it, else a new one; a parked cast of
    /// another name closes. A successful recast makes that reused output the workspace source
    /// for the next `create`.
    fn session_cast_for_with(
        &mut self,
        name: &str,
        start: impl FnOnce(&str, bool) -> Result<CastParts>,
    ) -> Result<Option<SessionCastParts>> {
        if let Some(pending) = self.pending_cast.take() {
            if pending.name == name {
                return Ok(Some((
                    pending.node_id,
                    Some(pending.fd),
                    Box::new(pending.stop),
                )));
            }
        }
        let (fd, node_id, cursor_mode, stop) = start(name, self.hw_cursor)?;
        self.last_cursor_mode = Some(cursor_mode);
        self.prev_output = Some(name.to_string());
        Ok(Some((node_id, Some(fd), Box::new(stop))))
    }

    /// Record `name` and return the output whose workspace it replaces.
    fn replace_output(&mut self, name: &str) -> Option<String> {
        self.prev_output.replace(name.to_string())
    }
}

/// Usable when a live Hyprland instance for our uid is reachable: inherited
/// `HYPRLAND_INSTANCE_SIGNATURE`, or a socket under the session's runtime dir `hypr/`
/// (the systemd `--user` host has no env import). Cheap — safe on enumeration.
pub fn is_available() -> bool {
    if crate::with_env_lock(|| std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")).is_some() {
        return true;
    }
    let dir = match pf_host_config::session::runtime_dir() {
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
    /// Re-issue the last head's rule at `(x, y)`; a config reload keeps the place.
    fn apply_position(&mut self, x: i32, y: i32) {
        let Some(p) = self.placed.as_ref() else {
            return;
        };
        *p.at.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y));
        if let Err(e) = set_monitor_rule(&p.name, p.mode, p.colour, Some((x, y))) {
            tracing::warn!(output = %p.name, x, y, error = %format!("{e:#}"), "hyprland: head did not move");
        }
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

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    /// A cast of its own beside the owner's. Both are registered under the head's name, so its
    /// [`OutputGuard`] closes them before the remove.
    fn join_cast(&mut self, name: &str, _node_id: u32) -> Result<Option<SessionCastParts>> {
        let stream = crate::portal_cast::stream_existing_output(&SELECTOR, name, self.hw_cursor)?;
        self.last_cursor_mode = stream.cursor_mode;
        Ok(Some(stream.into_cast()))
    }

    fn name(&self) -> &'static str {
        "hyprland"
    }

    /// A keep-alive reconnect recasts the kept head at its name, for the device that owns it.
    /// A nameless kept head is refused: lingering a black head is worse than tearing it down.
    /// Mode already matched, since a kept head never resizes.
    fn accepts_kept(&self, identity_slot: Option<u32>, output_name: Option<&str>) -> bool {
        output_name.is_some() && crate::identity::may_reuse(identity_slot, self.client_fp)
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

    fn take_undo_seqs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending_undo)
    }

    fn heads(&self) -> Result<Vec<crate::monitors::PhysicalMonitor>> {
        list_monitors()
    }

    /// Heads come back through `hyprctl reload`, run only when one is disabled, then out of
    /// DPMS. The output goes only if this Hyprland named it: another instance may have reused
    /// the name.
    fn undo(&mut self, undo: &Undo) -> Result<()> {
        match undo {
            Undo::EnableHeads { heads } => restore_heads(heads),
            Undo::RemoveOutput { name, instance } => {
                if journal::foreign_output("hyprland", name, instance, &hypr_instance()) {
                    return Ok(());
                }
                if monitor(name, true)?.is_none() {
                    return Ok(());
                }
                evacuate_workspace(name);
                hyprctl_dispatch(&["output", "remove", name])
                    .with_context(|| format!("hyprctl output remove {name}"))
            }
            Undo::Takeover { .. } => bail!("no undo in hyprland for {undo:?}"),
        }
    }

    fn set_session_cast_handoff(&mut self, enabled: bool) {
        self.handoff_cast = enabled;
    }

    fn session_cast_for(&mut self, name: &str) -> Result<Option<SessionCastParts>> {
        self.session_cast_for_with(name, start_cast)
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        preflight_once();
        // A retry's parked cast from the last attempt closes before a new head exists.
        self.pending_cast = None;
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );

        // A head `hyprctl` cannot list counts as listed: the create fails on that anyway.
        let name = head_name(self.last_slot, |n| !matches!(monitor(n, true), Ok(None)));
        let seq = journal::push(
            "hyprland",
            Undo::RemoveOutput {
                name: name.clone(),
                instance: hypr_instance(),
            },
        );
        // Owned before the dispatch: a create that times out may still run in Hyprland, and
        // every error from here on drops the guard, whose remove takes that head back.
        let output = OutputGuard {
            name: name.clone(),
            seq,
        };
        hyprctl_dispatch(&["output", "create", "headless", &name]).with_context(|| {
            format!("hyprctl output create headless {name} (is hyprctl reachable?)")
        })?;
        wait_monitor_ready(&name, Duration::from_secs(5))
            .with_context(|| format!("waiting for headless output {name} to appear"))?;

        // Client mode is also the frame clock: a headless output is timer-paced from it.
        let colour =
            apply_monitor_rule(&name, mode, self.hdr.then(|| HdrRule::new(self.client_hdr)))
                .with_context(|| format!("set monitor rule for {name}"))?;

        // Portal fd stays off this output so the registry can linger the named
        // head. [`session_cast_for`] hands the fd to the session, not the pool.
        let (fd, node_id, cursor_mode, stop) = start_cast(&name, self.hw_cursor)?;
        // On today's xdph this is `embedded` regardless of `hw_cursor`; the
        // session's cursor behaviour follows this, not the request.
        self.last_cursor_mode = Some(cursor_mode);
        let (remote_fd, direct_cast) = if self.handoff_cast {
            self.pending_cast = Some(PendingCast {
                node_id,
                fd,
                name: name.clone(),
                stop,
            });
            (None, None)
        } else {
            (Some(fd), Some(stop))
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
        // Last: nothing fails after this, so no path darkens heads and then unwinds.
        if crate::backend::exclusive_wanted("hyprland", self.topology, self.client_fp) {
            disable_other_heads(&name, &mut self.pending_undo);
        }
        // Hyprland gives each replacement an empty workspace. Carry the active
        // workspace only after every fallible setup step has completed.
        if let Some(prev) = self.replace_output(&name) {
            adopt_active_workspace(&prev, &name);
        }
        let at = Position::default();
        self.placed = Some(Placed {
            name: name.clone(),
            mode,
            colour,
            at: at.clone(),
        });
        let output_keepalive = OutputKeepalive {
            _reload: watch_config_reloads(name.clone(), mode, colour, at),
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

/// Named head the registry lingers. ScreenCast is the session's [`StopGuard`], not here:
/// pooling this must not keep the portal session alive across reconnects.
struct OutputKeepalive {
    /// First so the watcher is gone before the output is removed — it must
    /// never re-apply a rule onto a head this teardown deletes.
    _reload: Option<ReloadWatcher>,
    _output: OutputGuard,
}

/// Standalone `create`: close ScreenCast before removing its named output.
struct DirectKeepalive {
    _cast: StopGuard,
    _output: OutputKeepalive,
}

/// ScreenCast parked off the pooled output until [`HyprlandDisplay::session_cast_for`]
/// takes it. The fd is session-scoped; the named head is not.
struct PendingCast {
    node_id: u32,
    fd: OwnedFd,
    name: String,
    stop: StopGuard,
}

/// One cast's portal fd, node, negotiated cursor mode and closer.
type CastParts = (OwnedFd, u32, crate::portal_cursor::Mode, StopGuard);

/// Point xdph at `name` and cast it. The cast is registered under `name`, so the head's
/// [`OutputGuard`] closes it before the remove.
fn start_cast(name: &str, hw_cursor: bool) -> Result<CastParts> {
    focus_output(name);
    crate::portal_cast::cast(&SELECTOR, name, hw_cursor)
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
    at: Position,
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
            let place = *at.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = set_monitor_rule(&name, mode, colour, place) {
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
    let runtime = pf_host_config::session::runtime_dir()
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

/// DPMS off every head but a sibling's for a **gamescope** `exclusive` stream
/// ([`crate::monitors::dpms_others`]), with `record(plan)` run before the first goes dark.
/// Returns only heads [`dpms_one`] moved: the dispatcher toggles, so a re-light of an
/// untouched head would darken it.
///
/// Not [`disable_other_heads`]: a disabled Hyprland head's only undo is re-reading the
/// whole config ([`restore_heads`]). `dispatch dpms on` does not re-enable a disabled head.
pub(crate) fn dpms_other_heads(record: impl FnOnce(&[String])) -> Vec<String> {
    let heads = list_monitors();
    if let Ok(listed) = &heads {
        record(&crate::monitors::heads_to_darken(listed, "", &[]));
    }
    crate::monitors::dpms_others("hyprland", heads, |n| dpms_one(n, false))
}

/// DPMS on for exactly `names`, the heads [`dpms_other_heads`] darkened. The ones now on.
pub(crate) fn relight_heads(names: &[String]) -> Vec<String> {
    crate::monitors::relight("hyprland", names, |n| dpms_one(n, true))
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
/// `want` is the workspace an earlier session claimed for the same launch. A
/// head that went away left it on another monitor, where `workspace` would only
/// switch that monitor, so it is moved here. Otherwise
/// [`crate::routing::pick_workspace`] chooses. `None` when the head's workspaces
/// cannot be read or the switch is refused — the launch then opens where the
/// head already looks.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let restore = active_workspace_id(name)?;
    let slots = workspace_slots(&hyprctl_json(&["workspaces"]).ok()?, name);
    let id = want.unwrap_or_else(|| crate::routing::pick_workspace(&slots, restore));
    if id == restore {
        return Some((id, restore));
    }
    let switched = if crate::routing::held_elsewhere(&slots, id) {
        workspace_to_monitor(id, name)
    } else {
        focus_workspace(id)
    };
    match switched {
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

/// Disable the heads an `exclusive` session darkens ([`crate::monitors::disable_for_exclusive`]),
/// with the plan journaled onto `pending` before the first goes dark: a planned head that
/// refused is lit, so its undo is a no-op. `managed` is [`is_managed_output`], so a
/// concurrent session is never blacked out; `keep_monitors` stays lit.
fn disable_other_heads(ours: &str, pending: &mut Vec<u64>) {
    let keep = crate::policy::prefs().get().keep_monitors;
    crate::monitors::disable_for_exclusive(
        "hyprland",
        list_monitors(),
        ours,
        &keep,
        disable_head,
        focus_output,
        |plan| pending.push(journal::push_heads("hyprland", plan)),
    );
}

/// The Hyprland `hyprctl` talks to, as its instance signature: what a journal entry records
/// as its `instance`. Empty when none is known, which an undo reads as "not this one".
fn hypr_instance() -> String {
    crate::session::hypr_signature().unwrap_or_default()
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

/// Re-enable the heads an `exclusive` session disabled, and wake the ones a gamescope stream
/// blanked. Runs **before** that member's output is removed, so Hyprland never sees zero
/// enabled outputs.
///
/// `hyprctl reload` is the only thing that re-enables a disabled head: a runtime `monitor`
/// rule is additive and the `disable` in it keeps winning. A reload drops every runtime
/// `keyword`/`eval` override, the operator's included, and re-runs `exec =` lines, so it runs
/// only when one of `heads` is disabled. It answers `ok` for "config parsed": the journal reads
/// the heads back, and one the operator's own config disables stays dark there.
///
/// Input does not wake a blanked head, so each one still listed with DPMS off goes through
/// [`dpms_one`]; a lit or unplugged head gets no dispatch.
fn restore_heads(heads: &[Head]) -> Result<()> {
    for Head { name, .. } in heads {
        if head_is_enabled(name)? == Some(false) {
            hyprctl_dispatch(&["reload"]).context("hyprctl reload")?;
            break;
        }
    }
    let mut dark = Vec::new();
    for Head { name, .. } in heads {
        if monitor_dpms(name) == Some(false) {
            if let Err(e) = dpms_one(name, true) {
                dark.push(format!("{name} ({e:#})"));
            }
        }
    }
    if dark.is_empty() {
        return Ok(());
    }
    bail!(
        "hyprland did not wake {}; `hyprctl dispatch dpms on <name>` puts it back",
        dark.join(", ")
    )
}

/// The headless output of one `create`, removed on drop. It exists before the create dispatch,
/// so a create that failed may name a head Hyprland never made: its remove answers `output not
/// found`. `seq` is its journal entry: the remove, or a listing without the head, settles it; a
/// refused remove of a listed head leaves it for the next host's reclaim.
struct OutputGuard {
    name: String,
    seq: u64,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        // Every cast of it closes first, a joiner's too: removing a head xdph is still
        // capturing wedges its loop.
        crate::portal_cast::stop_casts(&self.name);
        evacuate_workspace(&self.name);
        match hyprctl_dispatch(&["output", "remove", &self.name]) {
            Ok(_) => {
                journal::done(self.seq);
                tracing::info!(output = %self.name, "hyprland headless output removed")
            }
            Err(_) if matches!(monitor(&self.name, true), Ok(None)) => journal::done(self.seq),
            Err(e) => {
                tracing::warn!(output = %self.name, error = %format!("{e:#}"), "output remove failed")
            }
        }
    }
}

/// Budget for one `hyprctl` call ([`crate::proc`]). `hyprctl` waits on the
/// instance socket, so against a wedged compositor it never returns. These
/// run on the session's stream thread; a hung query wedges the session. Five
/// seconds is generous next to a healthy call (single-digit milliseconds).
const HYPRCTL_BUDGET: Duration = Duration::from_secs(5);

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
                // Ours by shape ([`is_managed_output`]), not by prefix alone, so a
                // user's `PF-office` stays theirs.
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
        return set_monitor_rule(name, mode, None, None).map(|()| None);
    };
    let lit = match set_monitor_rule(name, mode, Some(rule), None) {
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
    pf_frame::hdr::note_hdr_capture_failed(pf_frame::hdr::HdrSource::VirtualOutput);
    set_monitor_rule(name, mode, None, None).map(|()| None)
}

/// Set the client's exact mode on `name`, both config eras.
///
/// `hyprctl keyword monitor NAME,WxH@Hz,auto,1` is hyprlang (the default,
/// including ≥0.55 — version does not imply Lua). Fall back to
/// `hyprctl eval 'hl.monitor{…}'` only when `keyword` is gone. Either way,
/// confirm the output adopted the mode — some forms print `ok` for a command
/// they ignored. A headless output starts at 0×0; if neither form yields a
/// usable size, the compositor could not back the mode. `colour` appends the
/// HDR keys to both forms; `at` places the head, `None` lets Hyprland (`auto`).
fn set_monitor_rule(
    name: &str,
    mode: Mode,
    colour: Option<HdrRule>,
    at: Option<(i32, i32)>,
) -> Result<()> {
    let hz = mode.refresh_hz.max(1);
    let pos = at.map_or_else(|| "auto".to_string(), |(x, y)| format!("{x}x{y}"));
    let spec = format!(
        "{name},{}x{}@{hz},{pos},1{}",
        mode.width,
        mode.height,
        colour.map(|c| c.hyprlang()).unwrap_or_default()
    );
    let lua = format!(
        "hl.monitor{{ output = \"{name}\", mode = \"{}x{}@{hz}\", position = \"{pos}\", scale = 1{} }}",
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

/// What the shim defers to: the picker recorded before our takeover, else the one in the
/// file now; never the shim itself — a config that lost our marker would otherwise have the
/// shim exec itself on every foreign share.
fn fallback_picker(
    prior: Option<Option<String>>,
    current: Option<String>,
    shim: &str,
) -> Option<String> {
    prior
        .unwrap_or(current)
        .filter(|c| !c.is_empty() && c != shim)
}

/// The shim line that runs `cmd` with xdph's arguments. A plain command sits bare, so its
/// flags word-split; anything else (`~/bin/pick`, `$HOME/…`) is single-quoted into `sh -c`,
/// where it expands as a shell would and cannot break the script.
fn exec_line(cmd: &str) -> String {
    if picker_is_plain(cmd) {
        return format!("exec {cmd} \"$@\"");
    }
    let script = format!("{cmd} \"$@\"").replace('\'', r"'\''");
    format!("exec sh -c '{script}' sh \"$@\"")
}

/// The shim's script: our selection while one is pending, else `fallback` ([`exec_line`]),
/// else exit 0 (xdph's own fallback). `-s`, not `-f`: an empty file means no selection.
fn shim_body(sel: &str, fallback: Option<&str>) -> String {
    match fallback {
        Some(cmd) => format!(
            "#!/bin/sh\n# Managed by punktfunk. Hands xdph the output this host is streaming; with\n# no selection pending, defers to the picker configured before us.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\n{}\n",
            exec_line(cmd)
        ),
        None => format!(
            "#!/bin/sh\n# Managed by punktfunk.\n[ -s \"{sel}\" ] && exec cat \"{sel}\"\nexit 0\n"
        ),
    }
}

/// Install the shim for `fallback` at `shim` unless it already holds that script. `true` when
/// written. Mode at creation, not chmod after: xdph executes this file, and write-then-chmod
/// leaves it at the umask default for a moment. A quoted fallback is named once per write.
fn write_shim(shim: &str, sel: &str, fallback: Option<&str>) -> Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let body = shim_body(sel, fallback);
    if std::fs::read_to_string(shim).is_ok_and(|c| c == body) {
        return Ok(false);
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(shim)
        .with_context(|| format!("write {shim}"))?;
    f.write_all(body.as_bytes())
        .with_context(|| format!("write {shim}"))?;
    if let Some(picker) = fallback.filter(|c| !picker_is_plain(c)) {
        tracing::warn!(%picker, %shim, "prior xdph picker quoted into the shim's `sh -c`");
    }
    Ok(true)
}

/// Startup: put the shim back when the xdph config still names it and the file is gone, as
/// after a reboot under a host that died holding the picker. With no session the shim defers
/// to the recorded picker, so a share on the box works before any connect. xdph runs the shim
/// per share, so nothing restarts.
pub(crate) fn reconcile_picker_shim() {
    let Ok(path) = xdph_config_path() else { return };
    let shim = picker_shim_path();
    match reconcile_shim_at(&path, &shim, &selection_file()) {
        Ok(false) => {}
        Ok(true) => tracing::info!(%shim, "rewrote the xdph picker shim its config still names"),
        Err(e) => tracing::warn!(%shim, error = %format!("{e:#}"),
            "xdph picker shim not rewritten — screen sharing on this box fails until a stream starts"),
    }
}

/// [`reconcile_picker_shim`] over explicit paths. Writes only when our marker is in `config`,
/// its key names `shim`, and nothing is at `shim`; `true` when it wrote.
fn reconcile_shim_at(config: &std::path::Path, shim: &str, sel: &str) -> Result<bool> {
    let (current, prior) = crate::portal_config::peek(config, XDPH_BLOCK, XDPH_PICKER_KEY);
    if prior.is_none() || current.as_deref() != Some(shim) || std::path::Path::new(shim).exists() {
        return Ok(false);
    }
    let fallback = fallback_picker(prior, current, shim);
    write_shim(shim, sel, fallback.as_deref())
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
    let shim = picker_shim_path();
    let fallback = fallback_picker(prior, current, &shim);
    write_shim(&shim, &selection_file(), fallback.as_deref())?;

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

/// Bounded and fire-and-forget: a timeout means xdph reads the new config when it next starts.
fn restart_xdph() {
    crate::gamescope::systemctl_user(&["try-restart", "xdg-desktop-portal-hyprland.service"]);
}

#[cfg(test)]
mod tests {
    /// The slot the registry keys owner reuse and the budget on comes from the identity map
    /// here as on KWin: peeked at `set_client_identity`, none for a device with no identity.
    #[test]
    fn hyprland_reports_the_slot_the_identity_map_holds_for_its_device() {
        crate::fake::in_child(
            module_path!(),
            "hyprland_reports_the_slot_the_identity_map_holds_for_its_device",
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
                let mut d = super::HyprlandDisplay::new().unwrap();
                d.set_client_identity(fp);
                assert_eq!(d.last_identity_slot(), slot);
                d.set_client_identity(None);
                assert_eq!(d.last_identity_slot(), None);
            },
        );
    }

    #[test]
    fn the_shim_never_falls_back_to_itself() {
        let shim = "/run/user/1000/punktfunk-xdph-picker.sh";
        assert_eq!(super::fallback_picker(None, Some(shim.into()), shim), None);
        assert_eq!(
            super::fallback_picker(Some(Some("/usr/bin/pick".into())), Some(shim.into()), shim)
                .as_deref(),
            Some("/usr/bin/pick")
        );
        assert_eq!(
            super::fallback_picker(Some(None), Some("/usr/bin/pick".into()), shim),
            None
        );
    }

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

    /// A dead host's output is removed only from the Hyprland that named it. With no match
    /// the entry is settled and nothing runs: no `hyprctl` here, and none is needed.
    #[test]
    fn an_output_of_another_hyprland_instance_is_left_alone() {
        let mut vd = HyprlandDisplay::new().expect("stateless");
        for instance in ["0123456789abcdef_1700000000_123456", ""] {
            let undo = Undo::RemoveOutput {
                name: "PF-4242-1".into(),
                instance: instance.into(),
            };
            assert!(vd.undo(&undo).is_ok(), "{instance:?}");
        }
        assert!(
            vd.take_undo_seqs().is_empty(),
            "nothing journaled by an undo"
        );
    }

    /// A device with a slot gets its standing name while the compositor does not list it; a
    /// listed one (its own head during a rebuild, or a leftover) and no slot take the counter's.
    #[test]
    fn a_slot_head_takes_its_standing_name_only_while_free() {
        let counter = format!("PF-{}-", std::process::id());
        assert_eq!(head_name(Some(3), |_| false), "PF-3");
        let taken = head_name(Some(3), |n| n == "PF-3");
        assert!(taken.starts_with(&counter), "{taken}");
        assert!(head_name(None, |_| false).starts_with(&counter));
    }

    /// The counter name carries the creating host's pid, so a second host's names never
    /// collide with ours. A slot head `PF-<n>` is managed too.
    #[test]
    fn a_name_carries_its_owner_pid_and_only_ours_is_managed() {
        let mine = next_output_name();
        assert!(mine.starts_with(&format!("PF-{}-", std::process::id())));
        assert!(is_managed_output(&mine));
        assert!(is_managed_output("PF-1"));

        // A user's own monitor that happens to start with the prefix, and the
        // connectors every wlr-family compositor mints.
        for theirs in ["PF-office", "PF-", "PF-12-abc", "HEADLESS-1", "DP-1", ""] {
            assert!(!is_managed_output(theirs), "{theirs:?} is not ours");
        }
    }

    /// The backend hands the picker exactly what [`crate::portal_picker`] says.
    /// That module owns the format and its tests run on every platform.
    #[test]
    fn picker_line_is_the_shared_selection_format() {
        assert_eq!((SELECTOR.line)("PF-1"), "[SELECTION]/screen:PF-1\n");
    }

    /// A picker the shim cannot paste bare is quoted into `sh -c`, where it still expands and
    /// gets xdph's arguments: `~/bin/pick` and `$HOME/bin/pick` reach the user's picker.
    #[test]
    fn a_non_plain_picker_is_quoted_into_sh() {
        use std::os::unix::fs::PermissionsExt;
        let home = Scratch::new("quoted");
        std::fs::create_dir_all(home.0.join("bin")).unwrap();
        let pick = home.0.join("bin/pick");
        std::fs::write(&pick, "#!/bin/sh\necho \"picked $*\"\n").unwrap();
        std::fs::set_permissions(&pick, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (sel, shim) = (home.path("no-selection"), home.path("shim.sh"));
        let shim_path = "/run/user/1000/punktfunk-xdph-picker.sh";
        for (cmd, line, picked) in [
            (
                "~/bin/pick",
                r#"exec sh -c '~/bin/pick "$@"' sh "$@""#,
                "picked --allow-token\n",
            ),
            (
                "$HOME/bin/pick",
                r#"exec sh -c '$HOME/bin/pick "$@"' sh "$@""#,
                "picked --allow-token\n",
            ),
            (
                "$HOME/bin/pick --title 'a b'",
                r#"exec sh -c '$HOME/bin/pick --title '\''a b'\'' "$@"' sh "$@""#,
                "picked --title a b --allow-token\n",
            ),
        ] {
            let kept = fallback_picker(Some(Some(cmd.into())), None, shim_path);
            assert_eq!(kept.as_deref(), Some(cmd), "a non-plain prior is kept");
            let body = shim_body(&sel, Some(cmd));
            assert!(body.ends_with(&format!("{line}\n")), "{body}");
            std::fs::write(&shim, &body).unwrap();
            let out = std::process::Command::new("sh")
                .args([shim.as_str(), "--allow-token"])
                .env("HOME", &home.0)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), picked, "{cmd}");
        }
        assert_eq!(
            exec_line("/usr/bin/pick --flag"),
            r#"exec /usr/bin/pick --flag "$@""#
        );
    }

    /// A scratch dir for one test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(test: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("pf-vd-xdph-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `screencopy` holding `picker`, then taken over for `shim` as `ensure_xdph_config` does.
    fn taken_config(picker: &str, shim: &str) -> String {
        let theirs = format!("screencopy {{\n    custom_picker_binary = {picker}\n}}\n");
        crate::portal_config::upsert(&theirs, XDPH_BLOCK, XDPH_PICKER_KEY, shim)
    }

    /// After a reboot the config still names our shim and the runtime dir is empty: startup
    /// writes the shim back, deferring to the recorded picker. A config we never took and a
    /// shim already there are left alone.
    #[test]
    fn startup_rewrites_a_shim_the_config_names_and_nothing_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("reconcile");
        let (config, shim, sel) = (dir.path("xdph.conf"), dir.path("shim.sh"), dir.path("sel"));
        let config = std::path::Path::new(&config);

        std::fs::write(config, taken_config("/usr/bin/pick", &shim)).unwrap();
        assert!(reconcile_shim_at(config, &shim, &sel).unwrap());
        let body = std::fs::read_to_string(&shim).unwrap();
        assert_eq!(body, shim_body(&sel, Some("/usr/bin/pick")));
        assert!(body.contains("exec /usr/bin/pick \"$@\""), "{body}");
        let mode = std::fs::metadata(&shim).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        std::fs::write(&shim, "#!/bin/sh\nexit 3\n").unwrap();
        assert!(!reconcile_shim_at(config, &shim, &sel).unwrap());
        assert_eq!(
            std::fs::read_to_string(&shim).unwrap(),
            "#!/bin/sh\nexit 3\n"
        );

        std::fs::remove_file(&shim).unwrap();
        let theirs = "screencopy {\n    custom_picker_binary = /usr/bin/pick\n}\n";
        std::fs::write(config, theirs).unwrap();
        assert!(!reconcile_shim_at(config, &shim, &sel).unwrap());
        assert!(!std::path::Path::new(&shim).exists());

        std::fs::remove_file(config).unwrap();
        assert!(!reconcile_shim_at(config, &shim, &sel).unwrap());
        assert!(!std::path::Path::new(&shim).exists());
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

    /// Reconnect recasts only a named head; a nameless one creates. Whose head it may take is
    /// `registry::linux::tests::every_pooled_backend_reuses_by_owner`.
    #[test]
    fn a_reconnect_reuses_only_a_named_head() {
        let display = HyprlandDisplay::new().unwrap();
        assert!(display.accepts_kept(None, Some("PF-1-1")));
        assert!(!display.accepts_kept(None, None));
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
                let stop = crate::portal_cast::test_cast("PF-1-1", || {});
                Ok((fd.into(), 42, crate::portal_cursor::Mode::Embedded, stop))
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

    const MODE: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };

    /// A fake `hyprctl`: logs each call to `$HOME/hyprctl.log`, refuses `output create`, lists
    /// no monitor, and answers a remove with `$HOME/remove-answer`.
    const FAKE_HYPRCTL: &str = r#"echo "$*" >> "$HOME/hyprctl.log"
case "$*" in
  "output create headless "*) exit 1 ;;
  "output remove "*) cat "$HOME/remove-answer" ;;
  "-j "*) echo '[]' ;;
  *) echo ok ;;
esac
"#;

    /// A refused or timed-out create dispatch may still have run in Hyprland, so the head is
    /// removed after it. The journal entry goes on `ok` and on `output not found` alike.
    #[test]
    fn a_create_that_fails_at_its_dispatch_removes_the_head() {
        let test = "a_create_that_fails_at_its_dispatch_removes_the_head";
        let (bin, path) = crate::fake::fake_tool(test, "hyprctl", FAKE_HYPRCTL);
        crate::fake::in_child(module_path!(), test, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            for answer in ["ok", "output not found"] {
                std::fs::write(home.join("remove-answer"), answer).unwrap();
                let _ = std::fs::remove_file(home.join("hyprctl.log"));
                let mut vd = HyprlandDisplay::new().unwrap();
                let err = vd.create(MODE).err().expect("the dispatch is refused");
                let err = format!("{err:#}");
                assert!(
                    err.starts_with("hyprctl output create headless PF-"),
                    "{err}"
                );
                let log = crate::fake::home_lines("hyprctl.log");
                let at = |line: &str| log.iter().position(|l| l.starts_with(line));
                let created = at("output create headless PF-").expect("dispatched");
                let head = log[created].rsplit(' ').next().unwrap().to_string();
                let removed = at(&format!("output remove {head}")).expect("removed");
                assert!(created < removed, "{log:?}");
                let journal = crate::fake::journal_text();
                assert!(!journal.contains(&head), "{answer}: {journal}");
            }
        });
        let _ = std::fs::remove_dir_all(bin);
    }

    /// A head's casts, the session's and a joiner's, close before its remove: removing a head
    /// xdph still captures wedges the portal.
    #[test]
    fn the_casts_of_a_head_close_before_its_remove() {
        let test = "the_casts_of_a_head_close_before_its_remove";
        let (bin, path) = crate::fake::fake_tool(test, "hyprctl", FAKE_HYPRCTL);
        crate::fake::in_child(module_path!(), test, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            std::fs::write(home.join("remove-answer"), "ok").unwrap();
            let note = |line: &'static str| move || crate::fake::note_home("hyprctl.log", line);
            let _session = crate::portal_cast::test_cast("PF-1-9", note("closed session"));
            let _join = crate::portal_cast::test_cast("PF-1-9", note("closed join"));
            let name = "PF-1-9".to_string();
            let instance = hypr_instance();
            let seq = journal::push("hyprland", Undo::RemoveOutput { name, instance });
            drop(OutputGuard {
                name: "PF-1-9".into(),
                seq,
            });
            let log = crate::fake::home_lines("hyprctl.log");
            let at = |line: &str| log.iter().position(|l| l == line).expect(line);
            let removed = at("output remove PF-1-9");
            assert!(at("closed session") < removed, "{log:?}");
            assert!(at("closed join") < removed, "{log:?}");
            assert_eq!(
                crate::fake::journal_text(),
                "",
                "the remove settles the entry"
            );
        });
        let _ = std::fs::remove_dir_all(bin);
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
