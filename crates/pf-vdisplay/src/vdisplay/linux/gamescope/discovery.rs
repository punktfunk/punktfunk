//! Read-side gamescope probes: PipeWire node, EIS socket, version, capability, game-exit, and a
//! session's display env from `/proc`.
//!
//! Nothing here spawns or stops a gamescope. The capture target is the log line
//! `stream available on node ID: N`; `pw-dump` is a last resort scoped to this
//! spawn, because `node.name=gamescope` exists on both the adapter and the stream.
//!
//! Capability answers come from the resolved binary's `+pfhdr<N>` banner
//! (`packaging/gamescope/README.md`) and the process-wide `FLAGS_LOST` latch.
//! Every helper that shells out is bounded; a miss is `None`/`false`, never a hang.

use super::*;
use std::process::Child;

/// Unbounded `pw-dump` is polled every 300–500 ms from 45 s loops on the session
/// stream thread; below [`MIN_GAMESCOPE`] a wedged link never returns.
const PW_DUMP_BUDGET: Duration = Duration::from_secs(2);

/// `--version` only loads the binary; hitting this bound means it cannot run.
const VERSION_PROBE_BUDGET: Duration = Duration::from_secs(2);

/// gamescope exits with its nested app. 1.5 s of absence is the confirmation vs a
/// PipeWire hiccup; another gamescope's node must not mask that.
pub(crate) fn game_session_exited(node_id: u32) -> bool {
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        if gamescope_node_present(node_id) {
            return false;
        }
        if Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SteamGameWatch {
    /// Seen running, then gone — emit APP_EXITED.
    Exited,
    /// Cancelled, or never started within the grace. Leave the session up.
    Cancelled,
}

/// `None` unless the first token is `steam` and a `steam://rungameid/<digits>` URI is present.
pub(crate) fn steam_appid_from_launch(cmd: &str) -> Option<u32> {
    if cmd.split_whitespace().next() != Some("steam") {
        return None;
    }
    const MARKER: &str = "steam://rungameid/";
    let tail = &cmd[cmd.find(MARKER)? + MARKER.len()..];
    let digits: String = tail
        .chars()
        .take_while(|c: &char| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// [`SteamGameWatch::Exited`] only after the game was seen on screen; a cold Steam boot is
/// [`Cancelled`]. Both edges come from gamescope's own root atoms, never from process shape:
/// Steam wraps its pre-launch work (shader precompile, the install-script evaluator) in the same
/// `reaper SteamLaunch AppId=` the game gets, so a `/proc` match adopts a tree that was never the
/// game and reads its exit as the game exiting.
pub(crate) fn wait_for_steam_game_exit(
    appid: u32,
    seat: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
) -> SteamGameWatch {
    use std::sync::atomic::Ordering;
    // Shader precompile can delay the game by minutes. A miss leaves the session up; node-death
    // still covers the Steam client itself dying.
    const START_GRACE: Duration = Duration::from_secs(300);
    const POLL: Duration = Duration::from_secs(1);
    // A few polls: a brief process swap must not fire [`SteamGameWatch::Exited`].
    const EXIT_CONFIRM: Duration = Duration::from_secs(3);

    let start_deadline = Instant::now() + START_GRACE;
    // Coming up is `GAMESCOPE_FOCUSED_APP`, which also picks the display: a seat runs its own
    // gamescope with its own Xwayland, and only the one actually running this appid focuses it.
    // (`seat` has already narrowed it; this stays correct if the key is ever unavailable.)
    let dpy = loop {
        if let Some(d) = display_presenting(appid, seat) {
            break d;
        }
        if cancel.load(Ordering::Relaxed) || Instant::now() >= start_deadline {
            return SteamGameWatch::Cancelled;
        }
        std::thread::sleep(POLL);
    };
    tracing::info!(appid, dpy = %dpy, "gamescope: the launched game is on screen");
    let mut gone_since: Option<Instant> = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return SteamGameWatch::Cancelled;
        }
        // Going away is baselayer membership, not focus: opening the Steam overlay moves focus
        // to the client UI while the game is still running, and that must not read as an exit.
        // A vanished display (gamescope gone) also ends it — nothing left to present into.
        if baselayer_has(&dpy, appid) {
            gone_since = None;
        } else if gone_since.get_or_insert_with(Instant::now).elapsed() >= EXIT_CONFIRM {
            return SteamGameWatch::Exited;
        }
        std::thread::sleep(POLL);
    }
}

/// The gamescope Xwayland presenting `appid` on this seat, when one is.
pub(crate) fn display_presenting(appid: u32, seat: Option<&str>) -> Option<String> {
    xwayland_cursor_targets(seat)
        .into_iter()
        .map(|(d, _xauth)| d)
        .find(|d| focused_app(d) == Some(appid))
}

/// Connect to gamescope's Xwayland root. Never `setenv` XAUTHORITY to reach it: glibc rewrites
/// process-global `environ` and `getenv` takes no lock. Gamescope starts Xwayland with no
/// `-auth`, so an empty token connects — the same fallback `pf-capture`'s cursor source uses.
fn root_atoms(display: &str) -> Option<(x11rb::rust_connection::RustConnection, u32)> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::{DefaultStream, RustConnection};
    let parsed =
        x11rb::reexports::x11rb_protocol::parse_display::parse_display(Some(display)).ok()?;
    let screen = usize::from(parsed.screen);
    let stream = parsed
        .connect_instruction()
        .into_iter()
        .find_map(|addr| DefaultStream::connect(&addr).ok())
        .map(|(s, _peer)| s)?;
    let conn =
        RustConnection::connect_to_stream_with_auth_info(stream, screen, Vec::new(), Vec::new())
            .ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    Some((conn, root))
}

/// One `CARDINAL` list from `display`'s root window. `None` once the display is gone.
fn root_cardinals(display: &str, name: &[u8]) -> Option<Vec<u32>> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let (conn, root) = root_atoms(display)?;
    let atom = conn.intern_atom(true, name).ok()?.reply().ok()?.atom;
    if atom == 0 {
        return None; // never interned: this gamescope is not in --steam mode
    }
    let prop = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 16)
        .ok()?
        .reply()
        .ok()?;
    prop.value32().map(|v| v.collect())
}

/// The appid `display` is presenting, or `None` when it says nothing.
fn focused_app(display: &str) -> Option<u32> {
    root_cardinals(display, b"GAMESCOPE_FOCUSED_APP")?
        .first()
        .copied()
}

/// Is `appid` in `GAMESCOPECTRL_BASELAYER_APPID`? Steam adds it as the launch starts and removes
/// it within a second of the game process going (measured on .21). The list also carries ids no
/// game is running, so this only decides *going away* for an appid already seen focused.
fn baselayer_has(display: &str, appid: u32) -> bool {
    root_cardinals(display, b"GAMESCOPECTRL_BASELAYER_APPID").is_some_and(|v| v.contains(&appid))
}

/// Managed/SteamOS is single-session and logs to journald, so this is unscoped.
pub(super) fn poll_managed_node(timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = find_gamescope_node() {
            return Some(id);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Log line first: `node.name=gamescope` sits on both the adapter and the stream.
/// `&mut Child` so a `vkCreateDevice` death (under a second) returns `None` now;
/// polling the corpse for the full timeout blamed the GPU.
pub(super) fn wait_for_node(
    timeout: Duration,
    log: &std::path::Path,
    child: &mut Child,
) -> Option<u32> {
    let child_pid = child.id();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(id) = node_from_log(log) {
            return Some(id);
        }
        // Node first, then death: a publish-and-exit in the same tick still yields the id.
        // The keepalive `Child` owns what happens after that.
        match child.try_wait() {
            Ok(None) => {}
            // Last look: the node line may have landed between the two reads. Then stop.
            Ok(Some(status)) => {
                tracing::warn!(
                    pid = child_pid,
                    %status,
                    log = %log.display(),
                    "gamescope: the spawned process exited before publishing a PipeWire node — \
                     not waiting out the rest of the budget"
                );
                return node_from_log(log).or_else(|| find_gamescope_node_scoped(Some(child_pid)));
            }
            // ECHILD: the child was reaped elsewhere. Do not invent a death.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return find_gamescope_node_scoped(Some(child_pid));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Digits only: the per-instance `stream available on node ID:` line is ANSI-colored.
fn node_from_log(log: &std::path::Path) -> Option<u32> {
    let log = std::fs::read_to_string(log).ok()?;
    for line in log.lines().rev() {
        if let Some(pos) = line.find("stream available on node ID:") {
            let tail = &line[pos + "stream available on node ID:".len()..];
            let digits: String = tail.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// This instance's `GAMESCOPE_WAYLAND_DISPLAY` name, from `Running compositor on wayland
/// display 'gamescope-N'`. It is the seat key: every discovery below filters on it, because a
/// concurrent seat's gamescope is equally discoverable and taking the first match delivers one
/// seat's launch (and cursor) to another's screen.
pub(super) fn wayland_name_from_log(log: &std::path::Path) -> Option<String> {
    const MARK: &str = "Running compositor on wayland display ";
    let text = std::fs::read_to_string(log).ok()?;
    for line in text.lines().rev() {
        if let Some(pos) = line.find(MARK) {
            let tail = &line[pos + MARK.len()..];
            let name: String = tail
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// A kept gamescope node vanishes when the nested game exits — missing id means recreate.
pub(super) fn gamescope_node_present(node_id: u32) -> bool {
    let Ok(out) = crate::proc::output_within(
        Command::new("pw-dump").arg(node_id.to_string()),
        PW_DUMP_BUDGET,
    ) else {
        // `pw-dump` unavailable: do not block reuse. `mark_failed` is the backstop.
        return true;
    };
    dump_has_node(out.status.success(), &out.stdout, node_id)
}

/// `pw-dump <id>` answers with the object, or with nothing at all (PipeWire 1.6) for an id that
/// is gone. Unparseable output counts as present.
fn dump_has_node(success: bool, stdout: &[u8], node_id: u32) -> bool {
    if success && stdout.trim_ascii().is_empty() {
        return false;
    }
    let Ok(dump) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return true;
    };
    dump.as_array()
        .map(|objs| {
            objs.iter().any(|o| {
                o.get("id").and_then(|i| i.as_u64()) == Some(node_id as u64)
                    && o.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Node")
            })
        })
        .unwrap_or(true)
}

/// `node.name=gamescope` is on the adapter and the inner stream; only `Video/Source` is capturable.
/// Bare name match is the fallback for older gamescope that omits `media.class`.
pub(super) fn find_gamescope_node() -> Option<u32> {
    find_gamescope_node_scoped(None)
}

fn find_gamescope_node_scoped(scope: Option<u32>) -> Option<u32> {
    let out = crate::proc::output_within(&mut Command::new("pw-dump"), PW_DUMP_BUDGET).ok()?;
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let nodes = dump.as_array()?;
    let node_props = |obj: &serde_json::Value| -> Option<(u32, String, String, Option<u32>)> {
        if obj.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            return None;
        }
        let id = obj.get("id").and_then(|i| i.as_u64())? as u32;
        let props = obj.get("info").and_then(|i| i.get("props"));
        let name = props
            .and_then(|p| p.get("node.name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let class = props
            .and_then(|p| p.get("media.class"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        // PipeWire records the owning pid as a string or an int, depending on version.
        let pid = props
            .and_then(|p| p.get("application.process.id"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .map(|n| n as u32)
            });
        Some((id, name, class, pid))
    };
    // Absent `application.process.id` stays in-scope; the per-instance log is the primary address.
    let in_scope = |pid: Option<u32>| -> bool {
        match scope {
            None => true,
            Some(root) => pid.map(|p| descends_from(p, root)).unwrap_or(true),
        }
    };
    for obj in nodes {
        if let Some((id, name, class, pid)) = node_props(obj) {
            if class == "Video/Source"
                && (name == "gamescope" || name.contains("gamescope"))
                && in_scope(pid)
            {
                return Some(id);
            }
        }
    }
    for obj in nodes {
        if let Some((id, name, _, pid)) = node_props(obj) {
            if name == "gamescope" && in_scope(pid) {
                tracing::warn!(
                    node_id = id,
                    "gamescope node has no media.class=Video/Source tag — capturing it anyway"
                );
                return Some(id);
            }
        }
    }
    None
}

/// Live EIS socket name under `XDG_RUNTIME_DIR` (`gamescope-<display>-ei`).
/// Stale sockets linger, so only a successful `connect()` counts; newest mtime wins.
/// Returns the bare name — the injector resolves it the same way libei resolves `LIBEI_SOCKET`.
pub(super) fn find_gamescope_eis_socket() -> Option<String> {
    // `set_var` of `XDG_RUNTIME_DIR` races glibc getenv (UB; see crate `lib.rs`). The lock is
    // not reentrant — take the read here, not in a caller. `point_injector_at_eis` holds nothing;
    // `ei_socket_file()` takes and releases the same lock separately.
    let runtime = crate::with_env_lock(|| std::env::var("XDG_RUNTIME_DIR").ok())?;
    let mut live: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in std::fs::read_dir(&runtime).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The EIS socket itself, not its `.lock` sidecar or the bare Wayland socket.
        if !(name.starts_with("gamescope-") && name.ends_with("-ei")) {
            continue;
        }
        // Connectable == a live listener; a dead session's socket refuses.
        if std::os::unix::net::UnixStream::connect(entry.path()).is_err() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        live.push((mtime, name));
    }
    live.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    live.into_iter().next().map(|(_, n)| n)
}

/// No version warning — that belongs on the create path.
pub(crate) fn is_available() -> bool {
    crate::proc::output_within(
        Command::new(gamescope_bin()).arg("--version"),
        VERSION_PROBE_BUDGET,
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Absolute path: the session-plus wrapper and the SteamOS PATH shim run outside this `PATH`.
pub(crate) fn gamescope_bin() -> &'static str {
    static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        // Shared env lock: a concurrent session `set_var` must not race this read.
        let over = crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_BIN").ok())
            .filter(|s| !s.trim().is_empty());
        if let Some(path) = over {
            tracing::info!(bin = %path, "gamescope: PUNKTFUNK_GAMESCOPE_BIN override");
            return path;
        }
        if let Some(path) = which_in_path("punktfunk-gamescope") {
            tracing::info!(
                bin = %path,
                "gamescope: using the punktfunk build (10-bit HDR capture available)"
            );
            return path;
        }
        which_in_path("gamescope").unwrap_or_else(|| "gamescope".to_string())
    })
    .as_str()
}

/// Where our packages put `gamescopereaper`, which gamescope `execvp`s for every child it starts.
/// The distro's gamescope owns `/usr/bin/gamescopereaper`, so ours stays out of `/usr/bin`.
pub(super) const REAPER_DIR: &str = "/usr/lib/punktfunk/gamescope";

/// `PATH` for a gamescope we start, [`REAPER_DIR`] first; `None` when no package installed it.
pub(super) fn reaper_path_env() -> Option<String> {
    if !std::path::Path::new(REAPER_DIR)
        .join("gamescopereaper")
        .is_file()
    {
        return None;
    }
    let path = crate::with_env_lock(|| std::env::var("PATH").ok()).unwrap_or_default();
    Some(prepend_path_dir(REAPER_DIR, &path))
}

fn prepend_path_dir(dir: &str, path: &str) -> String {
    if path.is_empty() {
        dir.to_owned()
    } else {
        format!("{dir}:{path}")
    }
}

/// A bare name would look resolved and then fail at spawn.
fn which_in_path(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::with_env_lock(|| std::env::var("PATH").ok())?;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let cand = std::path::Path::new(dir).join(name);
        let Ok(md) = std::fs::metadata(&cand) else {
            continue;
        };
        if md.is_file() && md.permissions().mode() & 0o111 != 0 {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

/// Process-cached `+pfhdr<N>` (`packaging/gamescope/README.md`). `0` is stock.
/// Bit depth and cursor compositing are locked before the display exists, so this
/// is asked once. Monotonic: 1 HDR, 2 cursor, 3 custom refresh, 4 overlay,
/// 8 XKB on the seat, 9 paint-on-commit.
fn gamescope_patch_level() -> u32 {
    static LEVEL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        let Ok(out) = crate::proc::output_within(
            Command::new(gamescope_bin()).arg("--version"),
            VERSION_PROBE_BUDGET,
        ) else {
            return 0;
        };
        // Banner is stderr on some builds, stdout on others (same split as the version gate).
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let level = parse_patch_level(&text);
        if level > 0 {
            tracing::info!(
                bin = %gamescope_bin(),
                level,
                "gamescope carries the punktfunk patch set — HDR capture, and (level 2+) the \
                 cursor composited into the capture stream"
            );
        } else {
            // INFO, matching the capable branch. A DEBUG miss hid why `capture_supports_hdr` was false.
            tracing::info!(
                bin = %gamescope_bin(),
                "gamescope has no {PFHDR_MARKER} marker — sessions on this backend stay 8-bit SDR \
                 with a host-composited cursor (install punktfunk-gamescope for HDR)"
            );
        }
        level
    })
}

/// 10-bit BT.2020/PQ on the PipeWire node (patch level ≥ 1).
pub(crate) fn gamescope_hdr_capable() -> bool {
    gamescope_patch_level() >= 1 && !flags_lost()
}

/// `--pipewire-composite-cursor`. Then the host skips XFixes blend, which is what
/// lets the encoder take the zero-CSC RGB-direct source (no blend stage).
pub(crate) fn gamescope_can_composite_cursor() -> bool {
    gamescope_patch_level() >= 2 && !flags_lost()
}

/// The seat publishes the `XKB_DEFAULT_*` keymap. Below this, `wlserver_keyboardfocus()`
/// re-binds a keymap-less stub, so every client keeps built-in `us`. Headless has no
/// libinput device to recover; injected keys are US-positional.
pub(crate) fn gamescope_honours_xkb_env() -> bool {
    gamescope_patch_level() >= 8 && !flags_lost()
}

/// `--custom-refresh-rates`. Below this, a headless connector returns empty
/// `GetModes()` / `GetValidDynamicRefreshRates()` and reports INTERNAL, so Steam
/// shows one rate (the `-r` / 60 Hz default) and no resolutions.
pub(crate) fn gamescope_can_offer_refresh_rates() -> bool {
    gamescope_patch_level() >= 3 && !flags_lost()
}

/// `--pipewire-composite-external-overlay`. No host-side substitute: mangoapp
/// lives in someone else's overlay window.
pub(crate) fn gamescope_can_composite_external_overlay() -> bool {
    gamescope_patch_level() >= 4 && !flags_lost()
}

/// Paint-on-commit with a persistent game-rate limit. Level 9 loses the CLI limit during
/// refresh updates; only level 10+ may receive the paired `adaptive_sync_args` flags.
pub(crate) fn gamescope_paints_on_commit() -> bool {
    gamescope_patch_level() >= 10 && !flags_lost()
}

/// The capture node lists tiled modifiers and fixates the one the consumer keeps. Below
/// this its single LINEAR choice fixates our default, so a tiled default fails the link.
pub(crate) fn gamescope_offers_tiled_capture() -> bool {
    gamescope_patch_level() >= 21 && !flags_lost()
}

/// A planar (NV12/P010) capture composites through device memory. Below this its RGB
/// intermediate sits in system RAM, and on a discrete GPU every frame crosses PCIe twice.
pub(crate) fn gamescope_planar_capture_in_vram() -> bool {
    gamescope_patch_level() >= 25 && !flags_lost()
}

/// `GAMESCOPE_SET_OUTPUT_MODE`. Below this a spawn serves the one mode it was started at, and
/// a client asking for another retires it.
pub(crate) fn gamescope_can_resize_output() -> bool {
    gamescope_patch_level() >= 19 && !flags_lost()
}

/// The resize lands on the compositor's next pass, so this is tens of milliseconds. The
/// fallback costs a cold spawn, which is Steam's 13–30 s boot again.
const RESIZE_BUDGET: Duration = Duration::from_secs(2);

/// Move a kept gamescope to `mode`, on `seat`'s own Xwayland. `false` leaves the caller its
/// retire-and-spawn: no seat key, no display of that seat, a compositor that never interned the
/// atom, or the budget spent.
///
/// `seat` is required. Every gamescope on the box answers this atom, and resizing the first one
/// found resizes another player's screen.
pub(crate) fn resize_kept_output(seat: Option<&str>, mode: crate::Mode) -> bool {
    let Some(seat) = seat else {
        return false;
    };
    // `-r` at spawn is `game_hz`, so the resize asks for the rate a fresh spawn would.
    let want = [
        mode.width,
        mode.height,
        super::game_hz(mode.refresh_hz.max(1)) * 1000,
    ];
    xwayland_cursor_targets(Some(seat))
        .into_iter()
        .any(|(dpy, _xauth)| set_output_mode(&dpy, want))
}

/// Ask `display`'s gamescope for `want` (width, height, refresh in mHz) and wait for it to
/// report that mode in effect. The feedback atom is published from the pass that remade the
/// output and nudged the capture, so this is the resize landing, not the request being taken.
fn set_output_mode(display: &str, want: [u32; 3]) -> bool {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;
    let Some((conn, root)) = root_atoms(display) else {
        return false;
    };
    let intern = |name: &[u8]| -> u32 {
        conn.intern_atom(true, name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom)
            .unwrap_or(0)
    };
    // Never interned: this gamescope carries no runtime resize, whatever its banner said.
    let (ask, feedback) = (
        intern(b"GAMESCOPE_SET_OUTPUT_MODE"),
        intern(b"GAMESCOPE_OUTPUT_MODE_FEEDBACK"),
    );
    if ask == 0 || feedback == 0 {
        return false;
    }
    let sent = conn
        .change_property32(PropMode::REPLACE, root, ask, AtomEnum::CARDINAL, &want)
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !sent {
        return false;
    }
    let deadline = Instant::now() + RESIZE_BUDGET;
    loop {
        let applied: Option<Vec<u32>> = conn
            .get_property(false, root, feedback, AtomEnum::CARDINAL, 0, 3)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|p| p.value32().map(|v| v.collect()));
        if applied.as_deref() == Some(&want[..]) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Latched when a spawn's gamescope did not receive our flags.
/// Indirect modes (`GAMESCOPE_BIN` / PATH shim) can exec the distro binary;
/// the retry then plans SDR host-composited instead of promising HDR it lacks.
fn flags_lost() -> bool {
    FLAGS_LOST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Latch [`flags_lost`]. One-way: nothing we can observe proves the next session is better.
pub(crate) fn note_spawn_flags_lost() {
    FLAGS_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
}

static FLAGS_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `+pfhdr` prefix in the `--version` banner (`packaging/gamescope/README.md`).
const PFHDR_MARKER: &str = "+pfhdr";

/// `+pfhdr<N>` in `banner`, or `0`. Too low drops HDR; too high promises a cursor nobody paints.
fn parse_patch_level(banner: &str) -> u32 {
    banner
        .find(PFHDR_MARKER)
        .map(|i| &banner[i + PFHDR_MARKER.len()..])
        .map(|tail| {
            tail.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// `X.Y.Z` of `bin`. Split from [`check_gamescope_version`] so the WSI check can compare
/// two binaries; `None` there means leave the layer alone, not assume old.
pub(super) fn gamescope_version_of(bin: &std::path::Path) -> Option<(u32, u32, u32)> {
    let out = crate::proc::output_within(Command::new(bin).arg("--version"), VERSION_PROBE_BUDGET)
        .ok()?;
    // Same stdout/stderr split as the version gate.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_version(&text)
}

/// Below 3.16.22, headless PipeWire capture deadlocks against PipeWire ≥ 1.6 and head-blocks the daemon.
const MIN_GAMESCOPE: (u32, u32, u32) = (3, 16, 22);

/// First version whose `paint_pipewire()` includes the Steam overlay (`ccd62074` + `f8b33d38`).
/// Below this the overlay never reaches the node. Cursor and mangoapp stay out on stock
/// gamescope; those are patch flags, not this floor.
const MIN_GAMESCOPE_OVERLAY: (u32, u32, u32) = (3, 16, 23);

/// Warn below [`MIN_GAMESCOPE`] / [`MIN_GAMESCOPE_OVERLAY`]. Parse failure is silent — diagnostic, not a gate.
pub(super) fn check_gamescope_version() -> Option<(u32, u32, u32)> {
    let out = crate::proc::output_within(
        Command::new(gamescope_bin()).arg("--version"),
        VERSION_PROBE_BUDGET,
    )
    .ok()?;
    // Banner is stderr on some builds, stdout on others.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ver = parse_version(&text)?;
    if ver < MIN_GAMESCOPE {
        tracing::warn!(
            found = %format!("{}.{}.{}", ver.0, ver.1, ver.2),
            min = %format!("{}.{}.{}", MIN_GAMESCOPE.0, MIN_GAMESCOPE.1, MIN_GAMESCOPE.2),
            "gamescope is older than the minimum for reliable headless capture — expect a \
             capture deadlock against PipeWire ≥ 1.6 (a wedged link head-blocks the daemon); \
             upgrade gamescope or use PUNKTFUNK_COMPOSITOR=kwin|mutter"
        );
    } else if ver < MIN_GAMESCOPE_OVERLAY {
        tracing::warn!(
            found = %format!("{}.{}.{}", ver.0, ver.1, ver.2),
            min = %format!(
                "{}.{}.{}",
                MIN_GAMESCOPE_OVERLAY.0, MIN_GAMESCOPE_OVERLAY.1, MIN_GAMESCOPE_OVERLAY.2
            ),
            "gamescope is older than the first version that paints the Steam overlay (Shift+Tab / \
             Quick Access Menu) into its PipeWire node — the overlay will be absent from the \
             stream until you upgrade gamescope (the cursor is composited host-side regardless)"
        );
    }
    Some(ver)
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let mut parts = token.split('.');
        let (a, b, c) = (parts.next()?, parts.next(), parts.next());
        let (Some(b), Some(c)) = (b, c) else { continue };
        if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), c.parse()) {
            return Some((a, b, c));
        }
    }
    None
}

/// A gamescope we didn't spawn, for this uid. Our own bare-spawns are children of this process
/// (ppid walk), so one client's nested gamescope never makes the next client attach to it.
pub fn foreign_gamescope_running() -> bool {
    let our_pid = std::process::id();
    crate::proc::own_pids().any(|(pid, path)| {
        // Resolved name: nixpkgs wraps gamescope, so the kernel reports `.gamescope-wrap`.
        crate::proc::match_name(&path)
            .is_some_and(|comm| matches!(comm.as_str(), "gamescope" | "gamescope-wl"))
            // A killed gamescope its parent has not reaped serves no node to attach to.
            && crate::proc::pid_alive(pid)
            && !descends_from(pid, our_pid)
    })
}

/// Walk the ppid chain. Hop cap so a racing/exiting process cannot loop us.
pub(super) fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Some(ppid) = crate::proc::ppid(pid) else {
            return false;
        };
        pid = ppid;
    }
    false
}

/// Every nested Xwayland `(DISPLAY, XAUTHORITY)` ONE seat exposes. Gaming Mode uses two
/// (`--xwayland-count`); the pointer lives on whichever is focused, so the XFixes source connects
/// to all of that seat's. `seat` is the instance's `GAMESCOPE_WAYLAND_DISPLAY`; `None` keeps every
/// gamescope, which is only right when the caller has no seat to be wrong about.
#[cfg(target_os = "linux")]
pub(crate) fn xwayland_cursor_targets(seat: Option<&str>) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for (_, path) in crate::proc::own_pids() {
        let Some(env) = crate::proc::display_env(&path) else {
            continue;
        };
        // A sandboxed client rewrites the seat name to a bind path (`/run/pressure-vessel/…`), so
        // it never matches. That is fine: the un-sandboxed members — the wrapper, `steam.sh`, the
        // splash — all carry the real name, and one is enough.
        let on_seat = env
            .gamescope_wayland
            .as_deref()
            .is_some_and(|v| seat.is_none_or(|want| v == want));
        let (true, Some(d)) = (on_seat, env.display) else {
            continue;
        };
        // Distinct DISPLAY only; prefer the first non-empty XAUTHORITY seen for it.
        match out.iter_mut().find(|(dd, _)| *dd == d) {
            Some((_, xa)) if xa.is_none() => *xa = env.xauthority,
            Some(_) => {}
            None => out.push((d, env.xauthority)),
        }
    }
    out
}

/// `(DISPLAY, WAYLAND_DISPLAY, XAUTHORITY)` from a same-uid process on `seat` — matched by its
/// `GAMESCOPE_WAYLAND_DISPLAY`. Any one can be absent. Without the key this took the first
/// gamescope in `/proc`, which on a multi-seat host launches into somebody else's session.
pub(super) fn discover_session_display_env(
    seat: Option<&str>,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    crate::proc::own_pids().find_map(|(_, path)| {
        let env = crate::proc::display_env(&path)?;
        let gs_wayland = env
            .gamescope_wayland
            .filter(|v| seat.is_none_or(|want| v == want))?;
        Some((env.display, Some(gs_wayland), env.xauthority))
    })
}

#[cfg(test)]
mod live_probe {
    /// Reads the atoms off a real `--steam` gamescope. Ignored: needs one running.
    #[test]
    #[ignore]
    fn steam_atoms_are_readable() {
        let targets = super::xwayland_cursor_targets(None);
        assert!(!targets.is_empty(), "no gamescope Xwayland found");
        assert!(
            targets.iter().any(|(d, _)| super::focused_app(d).is_some()),
            "no display answered GAMESCOPE_FOCUSED_APP — the empty-token connect failed"
        );
    }

    /// With two gamescopes up, the seat key must select exactly one. Ignored: needs both.
    #[test]
    #[ignore]
    fn seat_key_selects_one_gamescope() {
        let all = super::xwayland_cursor_targets(None);
        assert!(all.len() >= 2, "need two live gamescopes for this probe");
        for var in ["PF_SEAT_A", "PF_SEAT_B"] {
            let Ok(seat) = std::env::var(var) else {
                continue;
            };
            let scoped = super::xwayland_cursor_targets(Some(&seat));
            assert_eq!(scoped.len(), 1, "{seat} did not select exactly one display");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_patch_level, parse_version, prepend_path_dir, steam_appid_from_launch, MIN_GAMESCOPE,
        MIN_GAMESCOPE_OVERLAY,
    };

    #[test]
    fn patch_level_parses_the_marker_and_nothing_else() {
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25-1-g8c676c3+pfhdr2 (gcc 15.2.0)"),
            2
        );
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25+pfhdr1 (clang 20)"),
            1
        );
        assert_eq!(
            parse_patch_level("gamescope version 3.16.25 (gcc 15.2.0)"),
            0
        );
        assert_eq!(parse_patch_level(""), 0);
        // Multi-digit revisions must not truncate to their first digit.
        assert_eq!(parse_patch_level("3.16.25+pfhdr10 (gcc)"), 10);
        // The runtime-resize floor: 18 keeps a kept spawn at one mode, 19 lets it move.
        assert_eq!(
            parse_patch_level("3.16.25-32-ga10c9f9e+pfhdr19 (gcc 14.2.0)"),
            19
        );
        // A marker with no number is not a capability claim.
        assert_eq!(parse_patch_level("3.16.25+pfhdr (gcc)"), 0);
        // The version triple must never be mistaken for the level.
        assert_eq!(parse_patch_level("gamescope version 3.16.25"), 0);
    }

    #[test]
    fn a_gone_node_is_an_empty_dump() {
        let node = br#"[{ "id": 150, "type": "PipeWire:Interface:Node" }]"#;
        assert!(super::dump_has_node(true, node, 150));
        assert!(!super::dump_has_node(true, node, 151));
        assert!(!super::dump_has_node(true, b"", 150));
        assert!(!super::dump_has_node(true, b"[]\n", 150));
        assert!(super::dump_has_node(false, b"", 150));
    }

    #[test]
    fn reaper_dir_goes_first_on_path() {
        assert_eq!(prepend_path_dir("/r", "/usr/bin:/bin"), "/r:/usr/bin:/bin");
        assert_eq!(prepend_path_dir("/r", ""), "/r");
    }

    /// The seat key comes off the spawn log gamescope already writes, ANSI colouring and all.
    #[test]
    fn reads_the_seat_key_from_the_spawn_log() {
        let dir = std::env::temp_dir().join(format!("pf-seat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("gs.log");
        std::fs::write(
            &log,
            "[gs] \u{1b}[0;34mInfo\u{1b}[0m wlserver: [wayland] unable to lock lockfile\n\
             [gs] Info  wlserver: Running compositor on wayland display 'gamescope-1'\n\
             [gs] Info  wlserver: Starting Xwayland on :3\n",
        )
        .unwrap();
        assert_eq!(
            super::wayland_name_from_log(&log).as_deref(),
            Some("gamescope-1")
        );
        std::fs::write(&log, "no such line here\n").unwrap();
        assert_eq!(super::wayland_name_from_log(&log), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_steam_appid_from_launch() {
        assert_eq!(
            steam_appid_from_launch("steam steam://rungameid/570"),
            Some(570)
        );
        assert_eq!(
            steam_appid_from_launch("steam -silent steam://rungameid/1091500"),
            Some(1091500)
        );
        assert_eq!(steam_appid_from_launch("lutris lutris:rungameid/42"), None);
        assert_eq!(steam_appid_from_launch("steam -gamepadui"), None);
        assert_eq!(steam_appid_from_launch("vkcube"), None);
        // A `steam://` URI whose first token is not `steam` is not a dedicated launch.
        assert_eq!(
            steam_appid_from_launch("firefox steam://rungameid/570"),
            None
        );
    }

    #[test]
    fn parses_version_banner() {
        assert_eq!(
            parse_version("gamescope version 3.16.22"),
            Some((3, 16, 22))
        );
        assert_eq!(
            parse_version("gamescope: version v3.15.9 (no PipeWire)"),
            Some((3, 15, 9))
        );
        assert_eq!(parse_version("3.16.20-1.fc41"), Some((3, 16, 20)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("only 3.16 here"), None); // needs a full triple
    }

    #[test]
    fn flags_known_bad_versions() {
        // 3.16.20 is the PipeWire 1.6 deadlock; 3.16.22 is the floor.
        assert!(parse_version("gamescope version 3.16.20").unwrap() < MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.16.22").unwrap() >= MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.17.0").unwrap() >= MIN_GAMESCOPE);
    }

    #[test]
    fn overlay_threshold_brackets_the_fix() {
        // 3.16.22 captures; 3.16.23 is the first overlay-in-node paint.
        assert!(parse_version("gamescope version 3.16.22").unwrap() >= MIN_GAMESCOPE);
        assert!(parse_version("gamescope version 3.16.22").unwrap() < MIN_GAMESCOPE_OVERLAY);
        assert!(parse_version("gamescope version 3.16.23").unwrap() >= MIN_GAMESCOPE_OVERLAY);
        assert!(parse_version("gamescope version 3.16.25").unwrap() >= MIN_GAMESCOPE_OVERLAY);
    }
}
