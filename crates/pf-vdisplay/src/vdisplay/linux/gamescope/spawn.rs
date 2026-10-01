//! Bare headless gamescope spawn: compositor argv, the nested wrapper script, and the
//! process handle whose drop tears the spawn down.

use super::*;
use std::process::{Child, Stdio};

/// Per-spawn id so two coexisting gamescopes never parse each other's log for a node id.
pub(super) static SPAWN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// This spawn's log under [`crate::session::runtime_dir`]. Never `/tmp`: concurrent spawns must
/// not clobber each other's `stream available on node ID:` line, and `/tmp` is world-writable.
pub(super) fn spawn_log_path(inst: u64) -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join(format!("punktfunk-gamescope-{inst}.log"))
}

/// Nested refresh: client's rate, capped by `PUNKTFUNK_MAX_FPS` (off by default). This is the
/// game's clamp, not the session — encode still repeats the held frame at the client's rate.
pub(super) fn game_hz(session_hz: u32) -> u32 {
    pf_host_config::config().game_fps(session_hz).max(1)
}

/// Add the compositor-side arguments shared by every bare gamescope spawn. `steam_mode` belongs
/// before the `--` terminator; [`PUNKTFUNK_GAMESCOPE_APP`](spawn()) configures the nested command
/// after it and therefore cannot enable gamescope's Steam integration itself.
///
/// `-r` is the rate the GAME sees and is clamped to, which is why the frame limiter lives here
/// (see [`game_hz`]) and nowhere near the session: capping it makes the game stop rendering
/// frames nobody asked for, while capture and the wire keep running at the client's own rate.
fn add_bare_gamescope_args(
    command: &mut Command,
    w: u32,
    h: u32,
    hz: u32,
    steam_mode: bool,
    grab_cursor: bool,
    hdr: bool,
) {
    command
        .args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &game_hz(hz).to_string()]);
    if steam_mode {
        command.arg("--steam");
    }
    if grab_cursor {
        command.arg("--force-grab-cursor");
    }
    command.args(our_flags(hdr, game_hz(hz)));
    // `-r` is already the reported refresh. This adds the rest of the advertised set.
    command.args(refresh_rate_args(hz));
    command.args(["--xwayland-count", "1", "--"]);
}

/// Our compositor flags, the set [`verify_managed_spawn_flags`] checks. Every spawn path passes
/// them; the refresh list travels separately (argv or `CUSTOM_REFRESH_RATES`).
pub(super) fn our_flags(hdr: bool, game_hz: u32) -> Vec<String> {
    let mut flags = hdr_args(hdr);
    flags.extend(cursor_args());
    flags.extend(adaptive_sync_args(game_hz));
    flags
}

/// Shared by all three spawn paths — a kept display is keyed on `hdr`. Headless hardcodes
/// `SupportsHDR() == false`; `--hdr-debug-force-support` is the bypass. SDR nits: see
/// [`SDR_REFERENCE_WHITE_NITS`].
fn hdr_args(hdr: bool) -> Vec<String> {
    if !hdr {
        return Vec::new();
    }
    let nits = pf_host_config::config()
        .gamescope_sdr_nits
        .unwrap_or(SDR_REFERENCE_WHITE_NITS);
    vec![
        "--hdr-enabled".to_string(),
        "--hdr-debug-force-support".to_string(),
        "--hdr-sdr-content-nits".to_string(),
        nits.to_string(),
    ]
}

/// BT.2408 HDR Reference White, the level clients anchor SDR white at. It is the starting value:
/// from `+pfhdr16` the capture follows Steam's SDR brightness setting when Steam changes it.
const SDR_REFERENCE_WHITE_NITS: u32 = 203;

/// Must agree with [`crate::gamescope_composites_cursor`] — both read the same probe.
fn cursor_args() -> Vec<String> {
    let mut args = Vec::new();
    if gamescope_can_composite_cursor() {
        args.push("--pipewire-composite-cursor".to_string());
    }
    // No host-side fallback: the host cannot reconstruct another process's overlay window.
    if gamescope_can_composite_external_overlay() {
        args.push("--pipewire-composite-external-overlay".to_string());
    }
    args
}

/// Paint-on-commit + `--framerate-limit` at the same rate as `-r`. The two travel together: VRR
/// stops pacing to the refresh grid, so a FIFO game would otherwise run unbounded. Gated on the
/// probe so argv means what it says. `PUNKTFUNK_GAMESCOPE_VRR=0` opts out.
fn adaptive_sync_args(game_hz: u32) -> Vec<String> {
    if !pf_host_config::config().gamescope_vrr || !gamescope_paints_on_commit() {
        return Vec::new();
    }
    vec![
        "--adaptive-sync".to_string(),
        "--framerate-limit".to_string(),
        game_hz.to_string(),
    ]
}

/// gamescope reads only `XKB_DEFAULT_*`, never `localectl`'s xorg.conf.d. Empty when unconfigured
/// so we do not invent a layout. Headless still needs the stub-keyboard patch or Xwayland stays US.
fn xkb_env() -> Vec<(&'static str, String)> {
    let resolved = pf_host_config::layout::system_layout();
    let pairs = resolved.names.env_pairs();
    if pairs.is_empty() {
        return pairs;
    }
    if gamescope_honours_xkb_env() {
        tracing::info!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "gamescope session: handing it the box's keyboard layout"
        );
    } else {
        tracing::warn!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "gamescope session: this build ignores XKB_DEFAULT_* (needs punktfunk-gamescope \
             +pfhdr8) — the session will type US characters whatever the box is configured for"
        );
    }
    pairs
}

pub(super) fn xkb_setenv_args() -> Vec<String> {
    xkb_env()
        .into_iter()
        .map(|(name, value)| format!("--setenv={name}={value}"))
        .collect()
}

/// Trailing newline so whatever follows in the drop-in body still parses.
pub(super) fn xkb_unit_lines() -> String {
    xkb_env()
        .into_iter()
        .map(|(name, value)| format!("Environment={name}={value}\n"))
        .collect()
}

/// Headless advertises one rate unless this is passed. `session_hz` is always in the list.
pub(super) fn refresh_rate_args(session_hz: u32) -> Vec<String> {
    if !gamescope_can_offer_refresh_rates() {
        return Vec::new();
    }
    vec![
        "--custom-refresh-rates".to_string(),
        refresh_rate_list(
            session_hz,
            &pf_host_config::config().gamescope_refresh_rates,
        ),
    ]
}

/// No whitespace: both managed paths interpolate this into unquoted `${PF_HDR_ARGS}`.
fn refresh_rate_list(session_hz: u32, configured: &[u32]) -> String {
    let mut rates = configured.to_vec();
    if !rates.contains(&session_hz) {
        rates.push(session_hz);
    }
    rates.sort_unstable();
    rates.dedup();
    rates
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Same answer `create` gates Steam-free on and `spawn` turns into `--steam`. Blank cmd falls
/// through to env.
pub(super) fn resolved_spawn_app(cmd: Option<&str>) -> Option<String> {
    cmd.map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        // Read the env fallback under the shared env lock so it can't race a concurrent session's
        // `set_var` of the same key.
        .or_else(|| crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_APP").ok()))
        .filter(|s| !s.trim().is_empty())
}

/// `None` app is `sleep infinity`. The wrapper relays `LIBEI_SOCKET`, applies the
/// nested environment, and runs the launch value as the shell command its source promises.
/// The WSI layer stays out of gamescope's own Vulkan process.
#[allow(clippy::too_many_arguments)] // one cohesive spawn spec, one call site
pub(super) fn spawn(
    w: u32,
    h: u32,
    hz: u32,
    app: Option<String>,
    log: &std::path::Path,
    hdr: bool,
    iso: Option<&crate::SessionIsolation>,
    seat_home: Option<&std::path::Path>,
) -> Result<Child> {
    // Real app vs `sleep infinity` keep-alive: scopes the game-only cursor-grab flag below.
    let game_launch = app.is_some();
    let app = app.unwrap_or_else(|| "sleep infinity".to_string());
    let app = shape_dedicated_command(&app);
    // A shortcut's launch never rides Steam's command line ([`deferred_shortcut_uri`]).
    let deferred = deferred_shortcut_uri(&app);
    let app = match deferred.as_deref() {
        Some(uri) => without_uri(&app, uri),
        None => app,
    };
    // Isolated: per-session relay so a concurrent spawn cannot overwrite the injector's socket.
    let relay = iso
        .map(|i| i.ei_relay.clone())
        .unwrap_or_else(ei_socket_file);
    let _ = std::fs::remove_file(&relay); // stale socket path from a previous session
                                          // `--steam` when launching Steam; the global knob still forces it on for every spawn.
    let steam_mode = pf_host_config::config().gamescope_steam || is_steam_launch(&app);
    // Default off: forces relative mode, which would break absolute-pointer games/menus.
    let grab_cursor = game_launch && pf_host_config::config().gamescope_grab_cursor;
    // Without a painting client gamescope pushes no capture buffers.
    let splash_exe = pf_host_config::config()
        .gamescope_splash
        .then(std::env::current_exe)
        .and_then(|r| {
            r.map_err(|e| tracing::warn!(error = %e, "gamescope: current_exe failed — no splash"))
                .ok()
        });
    let mut cmd = Command::new(gamescope_bin());
    if let Some(path) = discovery::reaper_path_env() {
        cmd.env("PATH", path);
    }
    add_bare_gamescope_args(&mut cmd, w, h, hz, steam_mode, grab_cursor, hdr);
    let wsi = WsiPlan::resolve();
    if wsi == WsiPlan::DistroDisabled {
        // `hdr` is a field, not a gate: the layer is missing either way, and so is the fix.
        tracing::warn!(
            hdr,
            "gamescope: this box's VkLayer_FROG_gamescope_wsi was built for a different gamescope \
             than the one we run, so it is disabled for this session and no nested game can get \
             an HDR10 swapchain. The punktfunk-gamescope package ships a matching layer."
        );
    }
    // The seat home is the NESTED command's, never gamescope's: the compositor keeps the box's
    // runtime dir, where PipeWire, Wayland and the EIS relay live. A seat we are about to fill
    // holds no Steam yet, so this is the launch's shape alone.
    let nested_seat_home = seat_home.filter(|_| is_steam_launch(&app));
    // Before the Steam under it writes a line: a later launch into this kept session reads the
    // marker past this offset to tell that Steam is up.
    if let Some(home) = nested_seat_home {
        mark_seat_steam_log(home);
    }
    let mut nested_env = wsi.env(hdr);
    if let Some(w) = nested_wayland_display(&app) {
        nested_env.push(("WAYLAND_DISPLAY", w.to_string()));
    }
    if let Some(home) = nested_seat_home {
        nested_env.extend(seat::env(home));
    }
    let script = nested_wrapper_script(
        &relay,
        splash_exe.is_some(),
        &nested_env,
        &seat_sandbox_argv(iso, nested_seat_home.is_some()),
    );
    cmd.args(["sh", "-c", &script, "sh"]);
    if let Some(exe) = &splash_exe {
        cmd.arg(exe);
    }
    // Env-pinned Pulse does not follow default-sink churn across concurrent sessions.
    if let Some(iso) = iso {
        if let Some(sink) = &iso.sink {
            cmd.env("PULSE_SINK", sink);
        }
        if let Some(src) = &iso.mic_source {
            cmd.env("PULSE_SOURCE", src);
        }
    }
    cmd.arg(&app)
        // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
        .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia")
        // The box's keyboard layout — see [`xkb_env`]. Empty on an unconfigured box.
        .envs(xkb_env())
        // Distro WSI off on the compositor. Ours is in the nested wrapper, not here.
        .envs(wsi.compositor_env())
        // Headless must not attach. Stale WAYLAND_DISPLAY in the manager env aborts gamescope
        // before its PipeWire node appears. Nested apps get gamescope's own DISPLAY.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    if let Ok(logf) = std::fs::File::create(log) {
        if let Ok(log2) = logf.try_clone() {
            cmd.stdout(Stdio::from(logf)).stderr(Stdio::from(log2));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    tracing::info!(
        w, h, hz, steam_mode, hdr, ?wsi,
        bin = %gamescope_bin(),
        splash = splash_exe.is_some(),
        %app,
        held_launch = deferred.as_deref().unwrap_or("-"),
        log = %log.display(),
        "spawning gamescope (headless)"
    );
    let child = cmd
        .spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")?;
    if let Some(uri) = deferred {
        // The home the nested Steam just took, so the forwarder reaches that one's pipe.
        hand_launch_to_steam_when_up(
            uri,
            child.id(),
            nested_seat_home.map(std::path::Path::to_path_buf),
        );
    }
    Ok(child)
}

/// The `bwrap` prefix this seat's nested command runs behind, empty when it runs unfiltered.
/// Every reason it does not apply says so once, on the spawn that would have used it.
fn seat_sandbox_argv(iso: Option<&crate::SessionIsolation>, seat_home: bool) -> Vec<String> {
    let (bwrap, dev) = match sandbox::plan(iso, seat_home) {
        sandbox::Plan::Off => return Vec::new(),
        sandbox::Plan::NoSeatHome => {
            tracing::info!(
                "gamescope: this launch runs under the box's own Steam home, so its Steam sees \
                 every pad on the box"
            );
            return Vec::new();
        }
        sandbox::Plan::NoBwrap => {
            tracing::info!(
                "gamescope: bwrap is not installed, so this seat's Steam sees every pad on the \
                 box — install bubblewrap"
            );
            return Vec::new();
        }
        sandbox::Plan::On { bwrap, dev } => (bwrap, dev),
    };
    if let Err(e) = sandbox::ensure_dirs(&dev) {
        tracing::warn!(seat_dev = %dev.display(), error = %e,
            "gamescope: seat device directory not created, so this seat's Steam sees every pad \
             on the box");
        return Vec::new();
    }
    tracing::info!(seat_dev = %dev.display(),
        "gamescope: this seat's Steam is shown only the pads the host creates for it");
    sandbox::argv(
        &bwrap,
        &dev,
        &sandbox::aux_nodes(std::path::Path::new("/dev")),
    )
}

/// Builds the nested wrapper. Its first remaining argument is one shell command,
/// kept intact until the inner shell parses quoting and operators. `sandbox` is the seat's
/// device filter; empty runs the command as the session's own child, as it always did.
fn nested_wrapper_script(
    relay: &std::path::Path,
    with_splash: bool,
    nested_env: &[(&'static str, String)],
    sandbox: &[String],
) -> String {
    let env_kv = nested_env
        .iter()
        .map(|(k, v)| shell_word(&format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join(" ");
    let relay = shell_word(&relay.to_string_lossy());
    // Only the launch runs behind the filter. The splash and the relay write are the session's.
    let filter = sandbox
        .iter()
        .map(|a| shell_word(a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut run = String::from("exec");
    if !filter.is_empty() {
        run.push_str(&format!(" {filter}"));
    }
    if !env_kv.is_empty() {
        run.push_str(&format!(" env {env_kv}"));
    }
    run.push_str(" sh -c \"$1\"");
    if with_splash {
        let splash = if env_kv.is_empty() {
            "\"$1\" gamescope-splash &".to_string()
        } else {
            format!("env {env_kv} \"$1\" gamescope-splash &")
        };
        format!("printf %s \"$LIBEI_SOCKET\" > {relay}; {splash} shift; {run}")
    } else {
        format!("printf %s \"$LIBEI_SOCKET\" > {relay}; {run}")
    }
}

/// Quotes one POSIX shell word, including embedded apostrophes.
pub(super) fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(super) struct GamescopeProc {
    pub(super) child: Child,
    pub(super) log: std::path::PathBuf,
    /// The relay file THIS spawn's wrapper wrote — the global path, or the session's per-instance
    /// one when isolated — so teardown clears its own file and never a concurrent session's.
    pub(super) relay: std::path::PathBuf,
    /// Home of the Steam this spawn nests, when it runs one: the seat's, or the box's own.
    pub(super) steam_home: Option<std::path::PathBuf>,
}

impl Drop for GamescopeProc {
    fn drop(&mut self) {
        // A Steam that loses its compositor dies mid-write; ask it to quit first.
        if let Some(home) = &self.steam_home {
            let nested = home_steam_pid(home).filter(|&pid| descends_from(pid, self.child.id()));
            if let Some(pid) = nested {
                let box_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                let seat = (box_home.as_ref() != Some(home)).then_some(home.as_path());
                shut_steam_down(pid, STEAM_STOP_WAIT, seat);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Clear the relayed EIS socket name so an injector can't reconnect to this now-dead
        // session's socket between sessions (the stale path is the "Connection refused").
        let _ = std::fs::remove_file(&self.relay);
        // Drop this spawn's per-instance log so `$XDG_RUNTIME_DIR` doesn't accumulate them.
        let _ = std::fs::remove_file(&self.log);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both HDR spawn flags are required: `--hdr-enabled` alone does nothing on the headless
    /// backend, whose connector hardcodes `SupportsHDR() == false`. Their absence is
    /// indistinguishable from a capture negotiation failure.
    #[test]
    fn hdr_spawn_flags_are_both_present_and_absent_for_sdr() {
        assert!(
            hdr_args(false).is_empty(),
            "an SDR spawn takes no HDR flags"
        );
        let args = hdr_args(true);
        assert!(args.iter().any(|a| a == "--hdr-enabled"));
        assert!(
            args.iter().any(|a| a == "--hdr-debug-force-support"),
            "without the force flag the headless connector reports no HDR support, so the WSI \
             layer advertises no HDR surfaces and games render SDR"
        );
    }

    /// The rate set rides into both managed sessions inside an unquoted `${PF_HDR_ARGS}`, which the
    /// shim's shell word-splits. Whitespace would split one flag into two argv entries and gamescope
    /// would reject the launch. The session rate must also survive: it is the rate the session runs at.
    #[test]
    fn the_refresh_rate_list_is_word_split_safe_and_keeps_the_session_rate() {
        let list = refresh_rate_list(240, &[60, 120]);
        assert_eq!(list, "60,120,240", "sorted, deduped, session rate appended");
        assert!(
            !list.contains(char::is_whitespace),
            "an unquoted ${{PF_HDR_ARGS}} word-splits on whitespace"
        );
        assert_eq!(
            refresh_rate_list(60, &[60]),
            "60",
            "a configured set that already holds the session rate gains no duplicate"
        );
        assert_eq!(
            refresh_rate_list(90, &[]),
            "90",
            "unset, we advertise exactly the rate the client asked for"
        );
    }

    #[test]
    fn nested_wrapper_script_shapes() {
        let relay = std::path::Path::new("/run/user/1000/pf-ei");
        // Plain: relay + shell command, no splash machinery.
        let plain = nested_wrapper_script(relay, false, &[], &[]);
        assert!(plain.contains("/run/user/1000/pf-ei"));
        assert!(plain.ends_with("exec sh -c \"$1\""));
        assert!(!plain.contains("gamescope-splash"));
        // Splash shifts its executable, leaving the command in `$1`.
        let splash = nested_wrapper_script(relay, true, &[], &[]);
        assert!(splash.contains("\"$1\" gamescope-splash &"));
        assert!(splash.contains("shift; exec sh -c \"$1\""));
        let wsi = nested_wrapper_script(
            relay,
            false,
            &[("PUNKTFUNK_GAMESCOPE_WSI", "1".to_string())],
            &[],
        );
        assert!(wsi.contains("exec env 'PUNKTFUNK_GAMESCOPE_WSI=1' sh -c \"$1\""));
        assert_eq!(shell_word("a b'c;$HOME"), "'a b'\"'\"'c;$HOME'");

        // The seat's device filter wraps the launch alone: the relay write and the splash are
        // the session's, and the seat env is applied inside the sandbox.
        let seat_env = [("HOME", "/seats/cafe0123".to_string())];
        let filtered = nested_wrapper_script(
            relay,
            true,
            &seat_env,
            &[
                "/usr/bin/bwrap".to_string(),
                "--die-with-parent".to_string(),
            ],
        );
        assert!(filtered.contains("env 'HOME=/seats/cafe0123' \"$1\" gamescope-splash &"));
        assert!(filtered.contains(
            "shift; exec '/usr/bin/bwrap' '--die-with-parent' env 'HOME=/seats/cafe0123' \
             sh -c \"$1\""
        ));
        // No filter is byte-for-byte the launch this host ran before seats had one.
        assert_eq!(
            nested_wrapper_script(relay, true, &seat_env, &[]),
            "printf %s \"$LIBEI_SOCKET\" > '/run/user/1000/pf-ei'; \
             env 'HOME=/seats/cafe0123' \"$1\" gamescope-splash & \
             shift; exec env 'HOME=/seats/cafe0123' sh -c \"$1\""
        );
        assert!(seat_sandbox_argv(None, true).is_empty(), "the knob is off");

        let out = std::process::Command::new("sh")
            .args([
                "-c",
                &nested_wrapper_script(std::path::Path::new("/dev/null"), false, &[], &[]),
                "sh",
            ])
            .arg("printf '%s' 'quoted command stays whole'")
            .env("LIBEI_SOCKET", "test")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"quoted command stays whole");

        // The same through a filter — `env` stands in for bwrap, which a test box need not have.
        let out = std::process::Command::new("sh")
            .args([
                "-c",
                &nested_wrapper_script(
                    std::path::Path::new("/dev/null"),
                    false,
                    &[],
                    &["env".into(), "-u".into(), "PF_UNSET".into()],
                ),
                "sh",
            ])
            .arg("printf '%s' 'quoted command stays whole'")
            .env("LIBEI_SOCKET", "test")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"quoted command stays whole");
    }

    #[test]
    fn game_hz_is_the_session_rate_until_the_limiter_is_set() {
        // The env is process-wide and `config()` is parsed once, so this asserts the default
        // (nothing set): every host must keep handing gamescope the client's own rate. `game_fps`'s
        // own unit test in pf-host-config covers the capping arithmetic without needing the env.
        if pf_host_config::config().max_fps.is_none() {
            for hz in [30, 60, 120, 144, 240] {
                assert_eq!(game_hz(hz), hz);
            }
        }
        // Never zero, whatever the inputs: gamescope would reject `-r 0`.
        assert!(game_hz(0) >= 1);
    }
}
