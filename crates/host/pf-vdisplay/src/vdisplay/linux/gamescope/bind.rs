//! The `GAMESCOPE_BIN` wrapper, and the `/usr/bin/gamescope` bind for session scripts that
//! hardcode that path: the plan, the X11 socket-directory fix, and the probe that gates it.

use super::*;

/// Path of the host-written `GAMESCOPE_BIN` wrapper (per-user, in tmpfs).
fn gamescope_bin_wrapper_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gamescope-bin")
}

/// Injects `--nested-refresh $PF_HZ` (session-plus does not expose it). `$PF_HDR_ARGS` unquoted:
/// our own flag list, must word-split.
pub(super) fn write_gamescope_bin_wrapper() -> Result<std::path::PathBuf> {
    let path = gamescope_bin_wrapper_path();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec {} --nested-refresh \"${{PF_HZ:-60}}\" ${{PF_HDR_ARGS}} \"$@\"\n",
            gamescope_bin()
        ),
    )
    .with_context(|| format!("write GAMESCOPE_BIN wrapper {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod the GAMESCOPE_BIN wrapper {}", path.display()))?;
    Ok(path)
}

/// Hardcoded by some session scripts instead of `GAMESCOPE_BIN`. Env, PATH shim, and
/// `PUNKTFUNK_GAMESCOPE_BIN` all miss at once.
pub(super) const DISTRO_GAMESCOPE_PATH: &str = "/usr/bin/gamescope";

/// The socket directory every Xwayland — and so every gamescope — insists on before it will open a
/// display. See [`SessionBind`] for why the host has to care about a path it never reads itself.
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// Mentions `GAMESCOPE_BIN` ⇒ env lever exists, no bind. Names the absolute path and not the var
/// ⇒ bind is the only lever. Main script only; `sessions.d` still lands in [`verify_managed_spawn_flags`].
fn script_hardcodes_gamescope(script: &str) -> bool {
    !script.contains("GAMESCOPE_BIN") && names_the_distro_binary(script)
}

/// Complete path, not prefix of `gamescopectl` / `gamescope-session-plus`.
fn names_the_distro_binary(script: &str) -> bool {
    script.match_indices(DISTRO_GAMESCOPE_PATH).any(|(at, _)| {
        let after = &script[at + DISTRO_GAMESCOPE_PATH.len()..];
        !after
            .starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    })
}

/// The session script's text, read once per process. `None` when it cannot be read at all, which
/// [`plan_bind`] treats as "do not arm" — a box we cannot inspect keeps the behaviour that works.
fn session_script() -> Option<&'static str> {
    static SCRIPT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            // Bounded read: this is a shell script (~30 KiB). An unbounded read of an arbitrary
            // path a package could replace is not a thing to do on the connect path.
            let bytes = std::fs::read(SESSION_PLUS_BIN).ok()?;
            if bytes.len() > 1 << 20 {
                return None;
            }
            Some(String::from_utf8_lossy(&bytes).into_owned())
        })
        .as_deref()
}

/// Why the host is NOT redirecting [`DISTRO_GAMESCOPE_PATH`] for this launch. Each arm is a
/// different sentence to an operator reading the log, which is the whole reason it is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOff {
    /// The resolved binary IS the distro path — a bind over itself redirects nothing.
    SameBinary,
    /// The resolved gamescope is a bare NAME, not an absolute path, so the wrapper would resolve it
    /// through `PATH` inside the unit — onto the path we just bound the wrapper over.
    UnresolvedBinary,
    /// The session script honours `GAMESCOPE_BIN` (or never names the absolute path): the ordinary
    /// env lever already reaches gamescope, so the namespace would be cost without benefit.
    EnvLeverSuffices,
    /// The session script could not be read — fail closed rather than arm a mechanism we cannot
    /// show is needed.
    ScriptUnreadable,
    /// `PUNKTFUNK_GAMESCOPE_BIND=0`.
    OperatorOff,
    /// [`note_bind_hazard`] fired: a session launched with the bind armed never came up.
    Disarmed,
}

/// What the session unit's `/usr/bin/gamescope` redirect should be for this launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindPlan {
    /// No redirect, and therefore NO mount namespace for the unit at all.
    Off(BindOff),
    /// Redirect. `x11` additionally binds a user-owned socket directory over [`X11_SOCKET_DIR`],
    /// which is what stops the namespace from killing the session (see [`SessionBind`]).
    Arm { x11: bool },
}

/// Correctness first (no-op / re-exec), then operator knob, then backstop, then need.
/// `Some(true)` skips the script probe but does not outrank `disarmed`.
fn plan_bind(
    resolved_bin: &str,
    script: Option<&str>,
    enabled: Option<bool>,
    disarmed: bool,
    x11_dir_uid: Option<u32>,
    our_uid: u32,
) -> BindPlan {
    if resolved_bin == DISTRO_GAMESCOPE_PATH {
        return BindPlan::Off(BindOff::SameBinary);
    }
    // Bare name resolves through PATH inside the unit onto the wrapper we just bound. Re-exec loop.
    if !resolved_bin.starts_with('/') {
        return BindPlan::Off(BindOff::UnresolvedBinary);
    }
    if enabled == Some(false) {
        return BindPlan::Off(BindOff::OperatorOff);
    }
    if disarmed {
        return BindPlan::Off(BindOff::Disarmed);
    }
    if enabled != Some(true) {
        let Some(script) = script else {
            return BindPlan::Off(BindOff::ScriptUnreadable);
        };
        if !script_hardcodes_gamescope(script) {
            return BindPlan::Off(BindOff::EnvLeverSuffices);
        }
    }
    // Replace only a root-owned socket dir; ours maps, absent is created inside by us.
    BindPlan::Arm {
        x11: x11_dir_uid.is_some_and(|uid| uid != our_uid),
    }
}

/// Shared systemd spelling for the `/usr/bin/gamescope` redirect, so the transient unit and the
/// box drop-in cannot drift.
///
/// A user-unit mount namespace is also a user namespace (`uid_map` maps one id). Root-owned
/// `/tmp/.X11-unix` then reads as `nobody`; wlroots refuses every display and the short-session
/// tracker rewrites Game Mode to plasma. Bind a 0700 `$XDG_RUNTIME_DIR` dir read-write over it.
/// The XFixes reader is never spawned on this bind (patch paints the pointer); attach never arms
/// it. x11rb 0.14 dropped the abstract socket, so the filesystem path is the only one.
pub(super) struct SessionBind {
    wrapper: std::path::PathBuf,
    /// The user-owned directory bound over [`X11_SOCKET_DIR`], or `None` when the real one is
    /// already ours and needs no replacing.
    x11_dir: Option<std::path::PathBuf>,
}

impl SessionBind {
    /// The `[Service]` settings this bind is, one per line — the shared spelling behind both
    /// renderers below.
    fn properties(&self) -> Vec<String> {
        let mut props = vec![format!(
            "BindReadOnlyPaths={}:{DISTRO_GAMESCOPE_PATH}",
            self.wrapper.display()
        )];
        if let Some(dir) = &self.x11_dir {
            // Read-WRITE: Xwayland creates its socket in here.
            props.push(format!("BindPaths={}:{X11_SOCKET_DIR}", dir.display()));
        }
        props
    }

    /// `systemd-run --property=` arguments (the transient unit).
    pub(super) fn run_args(&self) -> Vec<String> {
        self.properties()
            .into_iter()
            .map(|p| format!("--property={p}"))
            .collect()
    }

    /// Drop-in body lines (the box's own unit).
    pub(super) fn unit_lines(&self) -> String {
        self.properties()
            .into_iter()
            .map(|p| format!("{p}\n"))
            .collect()
    }
}

/// Owner of [`X11_SOCKET_DIR`] as the host sees it. `lstat`, matching wlroots' own check — a
/// symlink there is not a directory it will accept either.
fn x11_socket_dir_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(X11_SOCKET_DIR)
        .ok()
        .map(|md| md.uid())
}

/// 0700: wlroots wants root-or-us and not group/other-writable.
fn session_x11_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-x11")
}

/// Create (or re-assert) [`session_x11_dir`] and drop dead sockets from it.
fn ensure_session_x11_dir() -> Result<std::path::PathBuf> {
    let dir = session_x11_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    prune_stale_x11_sockets(&dir);
    Ok(dir)
}

/// SIGKILL never unlinks Xwayland sockets. wlroots walks 0..32 and then gives up.
fn prune_stale_x11_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::symlink_metadata(&path).is_ok_and(|md| md.file_type().is_socket()) {
            continue;
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            continue; // somebody is listening — not ours to remove
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Probe inside a throwaway unit: our uid on [`X11_SOCKET_DIR`] ⇒ wlroots will accept it.
/// Anything else (including a probe that cannot answer) is a refusal. Cached; budgeted.
fn bind_survives_namespace(bind: &SessionBind) -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--wait", "--collect", "--pipe", "--quiet"]);
        for arg in bind.run_args() {
            cmd.arg(arg);
        }
        // Absolute: the probe unit's PATH is the user manager's, not ours, and a bare name could
        // resolve through the bind we just made.
        cmd.args(["--", "/usr/bin/stat", "-c", "%u", X11_SOCKET_DIR]);
        let out = crate::proc::output_within(&mut cmd, BIND_PROBE_BUDGET);
        let owner = match &out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok(),
            _ => None,
        };
        let ours = crate::proc::current_uid();
        match owner {
            Some(uid) if uid == ours => {
                tracing::debug!(
                    uid,
                    "gamescope: probed the session unit's namespace — {X11_SOCKET_DIR} reads as \
                     ours inside it, so gamescope's Xwayland will accept it"
                );
                true
            }
            Some(uid) => {
                tracing::warn!(
                    uid,
                    ours,
                    "gamescope: probed the session unit's namespace and {X11_SOCKET_DIR} reads as \
                     uid {uid} inside it, not ours — gamescope's Xwayland would refuse it and the \
                     session would never start. NOT redirecting {DISTRO_GAMESCOPE_PATH}; the \
                     session runs the distro's stock gamescope instead (no HDR, no in-node cursor, \
                     games see 60 Hz) but it starts."
                );
                false
            }
            None => {
                tracing::warn!(
                    error = ?out.as_ref().err(),
                    status = ?out.as_ref().ok().map(|o| o.status),
                    "gamescope: could not probe the session unit's namespace (systemd-run --user \
                     failed, or it rejected one of the bind properties) — NOT redirecting \
                     {DISTRO_GAMESCOPE_PATH}, since whatever stopped the probe would have stopped \
                     the session too"
                );
                false
            }
        }
    })
}

/// How long [`bind_survives_namespace`] may take. One transient unit that runs `stat`; a wedged
/// user manager costs a connect a moment, not the session.
const BIND_PROBE_BUDGET: Duration = Duration::from_secs(10);

/// One-way latch: a crash-loop here rewrites Game Mode to the desktop.
fn bind_disarmed() -> bool {
    BIND_DISARMED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn note_bind_hazard(failed_unit: &str) {
    BIND_DISARMED.store(true, std::sync::atomic::Ordering::Relaxed);
    match session_log_refusal() {
        Some(marker) => tracing::error!(
            unit = failed_unit,
            evidence = marker,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed, \
             and its log carries the signature of the reason — a mount namespace in a systemd USER \
             unit is also a USER namespace, in which only this uid is mapped, so root-owned \
             {X11_SOCKET_DIR} reads as `nobody` and gamescope's Xwayland refuses to open a display. \
             Disarming the bind and relaunching without it: the session runs the distro's stock \
             gamescope (no HDR, no in-node cursor, games see 60 Hz) but it STARTS. The bind stays \
             off until this host process restarts."
        ),
        None => tracing::warn!(
            unit = failed_unit,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed. \
             Nothing in the session log names the known cause (the user namespace that comes with \
             the unit's mount namespace, which makes root-owned {X11_SOCKET_DIR} read as `nobody` \
             to gamescope's Xwayland), so this may be an unrelated failure — disarming and \
             relaunching without the bind anyway, because a session that will not start is worse \
             than one without our flags. The bind stays off until this host process restarts."
        ),
    }
}

static BIND_DISARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The lines wlroots prints when the socket-directory check fails, and the one it prints after it
/// has failed for every display number. Either is proof this is the namespace bug and not, say, a
/// Steam that would not start.
const XWAYLAND_REFUSAL_MARKERS: [&str; 2] = [
    "not owned by root or us",
    "No display available in the first",
];

/// Which refusal marker (if any) a session log carries. Pure so the matching is testable against
/// the exact field lines.
fn xwayland_refusal_marker(log: &str) -> Option<&'static str> {
    XWAYLAND_REFUSAL_MARKERS
        .into_iter()
        .find(|marker| log.contains(marker))
}

/// Scan the logs `gamescope-session-plus` writes for the Xwayland refusal — evidence that a failed
/// launch was THIS bug. Best-effort and bounded: a missing or unreadable log just means we cannot
/// confirm it, never that we assume the opposite.
fn session_log_refusal() -> Option<&'static str> {
    let home = std::env::var("HOME").ok()?;
    for name in [".gamescope-stderr.log", ".gamescope-stdout.log"] {
        let path = std::path::Path::new(&home).join(name);
        if let Some(marker) = tail_of(&path, 64 << 10)
            .as_deref()
            .and_then(xwayland_refusal_marker)
        {
            return Some(marker);
        }
    }
    None
}

/// The last `max` bytes of a file, lossily as text. Bounded because these logs are written by
/// someone else's script and a chatty gamescope can make them large.
fn tail_of(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Resolve [`plan_bind`] against the live box, log the outcome, and return the armed bind.
/// `None` means the unit gets no mount namespace — the state every non-hardcoding distro stays in.
pub(super) fn arm_session_bind(wrapper: &std::path::Path) -> Option<SessionBind> {
    let plan = plan_bind(
        gamescope_bin(),
        session_script(),
        pf_host_config::config().gamescope_bind,
        bind_disarmed(),
        x11_socket_dir_owner(),
        crate::proc::current_uid(),
    );
    let x11 = match plan {
        BindPlan::Arm { x11 } => x11,
        // Only the arms an operator could act on are worth a line each; `SameBinary` and
        // `EnvLeverSuffices` are the ordinary answer on almost every box.
        BindPlan::Off(BindOff::SameBinary | BindOff::EnvLeverSuffices) => {
            tracing::debug!(
                bin = %gamescope_bin(),
                "gamescope: no {DISTRO_GAMESCOPE_PATH} redirect needed for this session"
            );
            return None;
        }
        BindPlan::Off(BindOff::UnresolvedBinary) => {
            tracing::warn!(
                bin = %gamescope_bin(),
                "gamescope: the resolved gamescope is a bare name, not an absolute path — not \
                 redirecting {DISTRO_GAMESCOPE_PATH}, because the wrapper resolves that name \
                 through PATH inside the unit and would land back on the redirect itself. Put the \
                 binary on the host's PATH, or set PUNKTFUNK_GAMESCOPE_BIN to an absolute path."
            );
            return None;
        }
        BindPlan::Off(BindOff::ScriptUnreadable) => {
            tracing::debug!(
                script = SESSION_PLUS_BIN,
                "gamescope: cannot read the session script, so cannot show a {DISTRO_GAMESCOPE_PATH} \
                 redirect is needed — not arming one"
            );
            return None;
        }
        BindPlan::Off(BindOff::OperatorOff) => {
            tracing::info!(
                "gamescope: PUNKTFUNK_GAMESCOPE_BIND=0 — never redirecting {DISTRO_GAMESCOPE_PATH}. \
                 On a distro whose gamescope-session-plus hardcodes that path the session runs the \
                 distro's stock gamescope: no HDR, no in-node cursor, and games see gamescope's \
                 60 Hz headless default."
            );
            return None;
        }
        BindPlan::Off(BindOff::Disarmed) => {
            tracing::info!(
                "gamescope: the {DISTRO_GAMESCOPE_PATH} redirect is disarmed for this host process \
                 — an earlier session did not come up with it armed. Running the distro's stock \
                 gamescope (no HDR, no in-node cursor). Restart punktfunk-host to try it again."
            );
            return None;
        }
    };
    let x11_dir = if x11 {
        match ensure_session_x11_dir() {
            Ok(dir) => Some(dir),
            // No socket directory we own ⇒ no way to survive the user namespace the redirect
            // costs ⇒ do not arm it. Degraded beats a session that cannot start.
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "gamescope: could not prepare a user-owned {X11_SOCKET_DIR} for the session's \
                     mount namespace — NOT redirecting {DISTRO_GAMESCOPE_PATH}, because the \
                     namespace alone would stop gamescope's Xwayland from opening a display. The \
                     session runs the distro's stock gamescope instead."
                );
                return None;
            }
        }
    } else {
        None
    };
    let bind = SessionBind {
        wrapper: wrapper.to_path_buf(),
        x11_dir,
    };
    // Everything above is a decision about what SHOULD work. This is the one check that asks the
    // box, and it is the last gate before a unit whose failure costs the user their session.
    if !bind_survives_namespace(&bind) {
        return None;
    }
    match &bind.x11_dir {
        Some(dir) => tracing::info!(
            bin = %gamescope_bin(),
            x11_dir = %dir.display(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. A \
             user-owned {X11_SOCKET_DIR} rides along because the mount namespace that redirect \
             costs brings a USER namespace with it, in which the real (root-owned) socket directory \
             reads as `nobody` and gamescope's Xwayland refuses to open a display."
        ),
        None => tracing::info!(
            bin = %gamescope_bin(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. \
             {X11_SOCKET_DIR} is already ours, so the unit's user namespace maps it unchanged and \
             it needs no replacing."
        ),
    }
    Some(bind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script that hardcodes `/usr/bin/gamescope` needs a bind; one that honours `GAMESCOPE_BIN`
    /// does not. Getting that backwards costs every other distro a mount namespace it has no use for.
    #[test]
    fn only_a_script_that_hardcodes_the_path_needs_the_bind() {
        let nobara = "if [ -z \"$GAMESCOPECMD\" ]; then\n    \
                      GAMESCOPECMD=\"/usr/bin/gamescope \\\n";
        assert!(script_hardcodes_gamescope(nobara));

        // Upstream / Bazzite: the env var is right there in the script.
        let upstream = "GAMESCOPE_BIN=${GAMESCOPE_BIN:-/usr/bin/gamescope}\n\
                        GAMESCOPECMD=\"$GAMESCOPE_BIN \\\n";
        assert!(
            !script_hardcodes_gamescope(upstream),
            "a script that reads GAMESCOPE_BIN needs no namespace to reach our binary"
        );

        // Mentions the var but not the path, and vice versa in the other direction: neither is the
        // shape the bind fixes, so neither arms it.
        assert!(!script_hardcodes_gamescope(
            "exec ${GAMESCOPE_BIN} \"$@\"\n"
        ));
        assert!(!script_hardcodes_gamescope("exec gamescope \"$@\"\n"));

        // The two longer paths that start with the one we redirect. A plain `contains` would read
        // either as a hardcode and buy the box a mount namespace for nothing.
        assert!(!script_hardcodes_gamescope(
            "/usr/bin/gamescopectl takescreenshot\n"
        ));
        assert!(!script_hardcodes_gamescope(
            "exec /usr/bin/gamescope-session-plus \"$@\"\n"
        ));
        // The real path still counts, whether a quote, a space or a newline follows it.
        assert!(script_hardcodes_gamescope("GS=\"/usr/bin/gamescope\"\n"));
        assert!(script_hardcodes_gamescope("exec /usr/bin/gamescope\n"));
        assert!(script_hardcodes_gamescope(
            "exec /usr/bin/gamescope -W 1920\n"
        ));
    }

    /// The decision matrix, in the order the arms are meant to win. Every `Off` arm here is a box
    /// that gets no mount namespace — the state every box was in before the redirect existed.
    #[test]
    fn bind_is_armed_only_where_it_is_needed_and_survivable() {
        const OURS: &str = "/usr/bin/punktfunk-gamescope";
        const AUTO: Option<bool> = None;
        const OFF: Option<bool> = Some(false);
        const FORCE: Option<bool> = Some(true);
        let hardcoded = Some("GAMESCOPECMD=\"/usr/bin/gamescope -W\"");
        let honours = Some("GAMESCOPECMD=\"${GAMESCOPE_BIN} -W\"");

        // The case the mechanism exists for: hardcoding script, our binary, root-owned socket dir
        // ⇒ redirect AND replace the socket directory, or the namespace kills Xwayland.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );

        // A socket directory already owned by us maps to us inside the user namespace too, so
        // there is nothing to compensate for — and an absent one is created, inside, by us.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(1000), 1000),
            BindPlan::Arm { x11: false }
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, None, 1000),
            BindPlan::Arm { x11: false }
        );

        // Nothing to redirect: the resolved binary IS the distro path. Checked before everything
        // else, because it is true regardless of what the script or the operator says.
        assert_eq!(
            plan_bind(
                DISTRO_GAMESCOPE_PATH,
                hardcoded,
                FORCE,
                false,
                Some(0),
                1000
            ),
            BindPlan::Off(BindOff::SameBinary)
        );

        // A bare name — `gamescope_bin`'s fallback when its PATH walk finds nothing. Binding
        // around it makes the wrapper `exec` a name that now resolves to the wrapper, so not even
        // a forcing operator gets it.
        assert_eq!(
            plan_bind("gamescope", hardcoded, FORCE, false, Some(0), 1000),
            BindPlan::Off(BindOff::UnresolvedBinary)
        );

        // The ordinary answer on Bazzite/SteamOS-likes: the env lever lands, so no namespace.
        assert_eq!(
            plan_bind(OURS, honours, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::EnvLeverSuffices)
        );

        // Fail closed on a box we cannot inspect.
        assert_eq!(
            plan_bind(OURS, None, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::ScriptUnreadable)
        );

        // Both retreats outrank the need: an operator's `=0`, and the runtime backstop's latch.
        assert_eq!(
            plan_bind(OURS, hardcoded, OFF, false, Some(0), 1000),
            BindPlan::Off(BindOff::OperatorOff)
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );

        // `=1` skips the script probe (for a GAMESCOPE_BIN defeated in a `sessions.d` fragment the
        // host cannot read) — but it does not outrank the backstop, or a forced box that
        // crash-loops would never stop.
        assert_eq!(
            plan_bind(OURS, honours, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, None, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, honours, FORCE, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );
    }

    /// The two renderers must spell the same settings — the transient unit takes them as
    /// `systemd-run --property=`, the box's own unit as drop-in lines. A session that got only half
    /// of them is the crash this mechanism is guarding.
    #[test]
    fn both_bind_renderers_carry_the_socket_directory() {
        let bind = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: Some(std::path::PathBuf::from("/run/user/1000/punktfunk-x11")),
        };
        assert_eq!(
            bind.run_args(),
            vec![
                format!(
                    "--property=BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}"
                ),
                format!("--property=BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}"),
            ]
        );
        assert_eq!(
            bind.unit_lines(),
            format!(
                "BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}\n\
                 BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}\n"
            )
        );
        // The socket-directory bind must be read-WRITE (Xwayland creates its socket in there); a
        // read-only one would fail exactly as loudly as no bind at all.
        assert!(bind.unit_lines().contains("BindPaths="));

        // No compensation needed ⇒ exactly one setting, and the unit line ends in a newline so the
        // `Environment=` lines after it in the drop-in body still parse.
        let plain = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: None,
        };
        assert_eq!(plain.run_args().len(), 1);
        assert!(plain.unit_lines().ends_with('\n'));
        assert!(!plain.unit_lines().contains("BindPaths="));
    }

    /// The lines wlroots prints when the socket-directory check fails. They turn the backstop's
    /// message from "something went wrong" into a named cause.
    #[test]
    fn the_xwayland_refusal_is_recognised_from_a_real_log() {
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:100] /tmp/.X11-unix not owned by root or us\n"
            ),
            Some("not owned by root or us")
        );
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:217] No display available in the first 33\n"
            ),
            Some("No display available in the first")
        );
        // A session that failed for some other reason must not be reported as this bug — the
        // backstop still disarms, but it says so differently.
        assert_eq!(
            xwayland_refusal_marker("steam.sh: line 1: pipewire: command not found\n"),
            None
        );
    }
}
