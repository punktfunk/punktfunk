//! The device view a seat's nested Steam gets: a `/dev/input` and a `/dev/hidraw*` of its own.
//!
//! Routing already keeps a client's input on its own pad, but any process of the user can open
//! every pad node, so Steam Input on one seat claims another seat's controller. [`argv`] wraps
//! the nested command — never gamescope, whose capture and input plumbing stay outside — in a
//! `bwrap` that replaces `/dev` and binds this seat's directory back as `/dev/input`. The host
//! exposes a pad by writing one symlink there (`pf-inject`'s `seat_dev`).
//!
//! A visibility filter, not a trust boundary: the box's own Steam is not ours to start and still
//! sees every pad. Evidence: `design/steam-seats-warm-launch-implementation-plan.md` WP-S3.

use std::path::{Path, PathBuf};

/// `/dev/hidrawN` links the sandbox carries. The kernel hands a pad the lowest free number, and
/// 64 is what a box with every USB HID device plugged in can reach.
const HIDRAW_LINKS: u32 = 64;

/// Nodes a nested game opens that a fresh `--dev` does not carry. `nvidia*` is enumerated
/// beside them: how many a box has is the driver's business, not ours.
///
/// SEAM — `uinput` is in reach, and the node it makes is not. Steam Input reads the seat's pad
/// over `hidraw` and hands games a virtual X360 pad of its own, which lands in the real
/// `/dev/input` where no seat can see it, so a game that reads evdev rather than the Steam Input
/// API finds no controller. Linking it needs the seat that created it, which needs
/// `UI_GET_SYSNAME` on the creating fd (`pidfd_getfd` into that seat's process tree) — holding a
/// `uinput` fd is not enough to tell two seats apart. Untested: it needs a signed-in seat in a
/// game. Withholding `uinput` instead turns Steam Input off for the seat.
const AUX_NODES: &[&str] = &["dri", "uinput", "kfd", "fuse"];

/// Whether this session's nested command runs behind the filter, and what stopped it. One
/// decision for the spawn and for the host thread that writes the links, so the directory a pad
/// is exposed in is the directory the seat's Steam reads.
pub(crate) enum Plan {
    /// The operator has not asked for the filter.
    Off,
    /// No seat home, so this launch shares the box's Steam and there is nothing to separate.
    NoSeatHome,
    /// `bubblewrap` is not installed.
    NoBwrap,
    On {
        bwrap: PathBuf,
        dev: PathBuf,
    },
}

impl Plan {
    /// The seat's device directory, or `None` on every reason it is not filtered.
    pub(crate) fn dev(self) -> Option<PathBuf> {
        match self {
            Plan::On { dev, .. } => Some(dev),
            _ => None,
        }
    }
}

/// `has_seat_home` is the launch's, not the session's: a non-Steam command runs under the box's
/// home, and filtering its devices would take pads off a session that never asked for a seat.
pub(crate) fn plan(iso: Option<&crate::SessionIsolation>, has_seat_home: bool) -> Plan {
    if !pf_host_config::config().steam_seat_sandbox {
        return Plan::Off;
    }
    let Some(iso) = iso.filter(|_| has_seat_home) else {
        return Plan::NoSeatHome;
    };
    match bwrap_bin() {
        Some(bwrap) => Plan::On {
            bwrap,
            dev: pf_paths::gamescope_seat_dev_dir(&iso.id),
        },
        None => Plan::NoBwrap,
    }
}

/// `bwrap` as an absolute path. The nested shell runs under gamescope's `PATH`, which is not
/// ours, so the argv names the binary we resolved.
fn bwrap_bin() -> Option<PathBuf> {
    on_path("bwrap")
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Create the seat's directory and the three it holds: `hostdev/` (the sandbox mounts the real
/// `/dev` there), `input/` (bound in as `/dev/input`), `hidraw/` (what the links point through).
pub(crate) fn ensure_dirs(dev: &Path) -> std::io::Result<()> {
    pf_paths::create_private_dir(dev)?;
    for sub in ["hostdev", "input", "hidraw"] {
        pf_paths::create_private_dir(&dev.join(sub))?;
    }
    Ok(())
}

/// The `bwrap` argv, ending in the command to run.
///
/// Order is the contract: `--dev-bind /dev` resolves its source before `--dev /dev` replaces it,
/// and the `hidraw` links must be in place before the kernel announces a pad — Steam never looks
/// at a node again once it failed to resolve.
pub(crate) fn argv(bwrap: &Path, dev: &Path, aux: &[String]) -> Vec<String> {
    let at = |sub: &str| dev.join(sub).to_string_lossy().into_owned();
    let mut out = vec![
        bwrap.to_string_lossy().into_owned(),
        "--bind".into(),
        "/".into(),
        "/".into(),
        "--dev-bind".into(),
        "/dev".into(),
        at("hostdev"),
        "--dev".into(),
        "/dev".into(),
        "--bind".into(),
        at("input"),
        "/dev/input".into(),
    ];
    for n in 0..HIDRAW_LINKS {
        out.push("--symlink".into());
        out.push(at(&format!("hidraw/hidraw{n}")));
        out.push(format!("/dev/hidraw{n}"));
    }
    for node in aux {
        out.push("--dev-bind".into());
        out.push(node.clone());
        out.push(node.clone());
    }
    // gamescope is the session keepalive; a seat that outlived it would hold the display.
    out.push("--die-with-parent".into());
    out
}

/// The nodes under `dev` worth binding back, in a fixed order so two spawns of one seat build
/// the same argv. A node that is not there is a box without that GPU, not a failure.
pub(crate) fn aux_nodes(dev: &Path) -> Vec<String> {
    let spell = |name: &str| dev.join(name).to_string_lossy().into_owned();
    let mut nvidia: Vec<String> = std::fs::read_dir(dev)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("nvidia").then(|| spell(&name))
        })
        .collect();
    nvidia.sort();
    let mut out: Vec<String> = AUX_NODES.iter().map(|n| spell(n)).collect();
    out.extend(nvidia);
    out.retain(|p| Path::new(p).exists());
    out
}

/// Off-switch for [`with_flatpak_shim`].
const FLATPAK_SHIM_ENV: &str = "PUNKTFUNK_GAMESCOPE_FLATPAK_SHIM";

/// A local bwrap that has not answered by now never will.
const NEST_PROBE: std::time::Duration = std::time::Duration::from_secs(5);

/// `filter` plus a shim over the flatpak binary, for everything the nested Steam starts.
///
/// Steam runs a shortcut's `/usr/bin/flatpak` by absolute path with Steam's own environment,
/// which every game shares, so neither `PATH` nor `WAYLAND_DISPLAY` reaches the shortcut alone.
/// The shim gives `flatpak run` what [`super::shape_flatpak_command`] gives a direct launch.
/// `filter` is the seat sandbox's argv, or empty. It comes back unchanged when flatpak is not
/// installed or bwrap cannot nest here, as pressure-vessel and flatpak must inside it.
pub(crate) fn with_flatpak_shim(mut filter: Vec<String>) -> Vec<String> {
    if pf_host_config::env_on(FLATPAK_SHIM_ENV) == Some(false) {
        return filter;
    }
    let Some(flatpak) = on_path("flatpak").and_then(|p| std::fs::canonicalize(p).ok()) else {
        return filter;
    };
    let bwrap = filter.first().map(PathBuf::from).or_else(bwrap_bin);
    let Some(bwrap) = bwrap.filter(|b| nests(b)) else {
        tracing::warn!(
            "gamescope: bwrap cannot run here, so a Steam shortcut to a flatpak may open on the \
             desktop instead of the stream — install bubblewrap and allow user namespaces"
        );
        return filter;
    };
    let dir = Path::new(&crate::session::runtime_dir()).join("punktfunk-flatpak");
    let binds = match write_shim(&dir, &flatpak) {
        Ok(binds) => binds,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "gamescope: flatpak shim not written");
            return filter;
        }
    };
    if filter.is_empty() {
        let root = ["--dev-bind", "/", "/", "--die-with-parent"];
        filter.push(bwrap.to_string_lossy().into_owned());
        filter.extend(root.map(String::from));
    }
    filter.extend(binds);
    tracing::info!(
        flatpak = %flatpak.display(),
        "gamescope: the flatpaks this Steam starts get gamescope's Wayland socket and X11"
    );
    filter
}

/// Writes the shim under `dir` and returns the binds that put it over `real`. The real binary
/// stays reachable at `flatpak.real`, an empty file outside the namespace.
fn write_shim(dir: &Path, real: &Path) -> std::io::Result<Vec<String>> {
    pf_paths::create_private_dir(dir)?;
    let (shim, mount) = (dir.join("flatpak"), dir.join("flatpak.real"));
    std::fs::File::options()
        .create(true)
        .append(true)
        .open(&mount)?;
    // A rename, never a rewrite: a running session's namespace still has the old file bound.
    let tmp = dir.join(format!("flatpak.{}", std::process::id()));
    std::fs::write(&tmp, shim_script(&mount))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &shim)?;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    Ok(vec![
        "--ro-bind".into(),
        s(real),
        s(&mount),
        "--ro-bind".into(),
        s(&shim),
        s(real),
    ])
}

/// Adds `--socket=x11` after `run` and points `WAYLAND_DISPLAY` at gamescope; any other flatpak
/// command passes through.
fn shim_script(real: &Path) -> String {
    // `for a` walks the original arguments while `set --` rebuilds them; the first word that is
    // not an option is the flatpak command.
    let lines = [
        "#!/bin/sh".to_string(),
        "cmd=".into(),
        "for a do".into(),
        "shift".into(),
        "set -- \"$@\" \"$a\"".into(),
        "if [ -z \"$cmd\" ] && [ \"${a#-}\" = \"$a\" ]; then".into(),
        "cmd=$a".into(),
        "if [ \"$a\" = run ]; then set -- \"$@\" --socket=x11; fi".into(),
        "fi".into(),
        "done".into(),
        format!("if [ \"$cmd\" = run ]; then {}fi", super::FLATPAK_WAYLAND),
        format!("exec {} \"$@\"", super::shell_word(&real.to_string_lossy())),
    ];
    lines.join("\n") + "\n"
}

/// Whether bwrap runs nested here, as pressure-vessel and flatpak will inside the shim's
/// namespace. Once per host: user namespaces do not come and go.
fn nests(bwrap: &Path) -> bool {
    static NESTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NESTS.get_or_init(|| {
        let mut cmd = std::process::Command::new(bwrap);
        cmd.args(["--dev-bind", "/", "/", "--"])
            .arg(bwrap)
            .args(["--dev-bind", "/", "/", "--", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        matches!(crate::proc::status_within(&mut cmd, NEST_PROBE), Ok(s) if s.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim run against a stand-in flatpak that prints what it got.
    fn run_shim(args: &[&str]) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pf-flatpak-shim-{}-{}",
            std::process::id(),
            args.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(
            &real,
            "#!/bin/sh\nprintf '%s|' \"$@\"\nprintf 'WL=%s' \"${WAYLAND_DISPLAY-unset}\"\n",
        )
        .unwrap();
        let shim = dir.join("shim");
        std::fs::write(&shim, shim_script(&real)).unwrap();
        for p in [&real, &shim] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = std::process::Command::new(&shim)
            .args(args)
            .env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-3")
            .env("XDG_RUNTIME_DIR", &dir)
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_flatpak_shim_shapes_run_and_nothing_else() {
        assert_eq!(
            run_shim(&[
                "--user",
                "run",
                "--branch=stable",
                "org.x.Y",
                "--fullscreen"
            ]),
            "--user|run|--socket=x11|--branch=stable|org.x.Y|--fullscreen|WL=wayland-gamescope-3"
        );
        // An app argument named `run` is the app's.
        assert_eq!(
            run_shim(&["run", "org.x.Y", "run"]),
            "run|--socket=x11|org.x.Y|run|WL=wayland-gamescope-3"
        );
        assert_eq!(run_shim(&["info", "org.x.Y"]), "info|org.x.Y|WL=unset");
    }

    /// The whole mount namespace, as a normal user with `bwrap` and a stand-in `flatpak` first
    /// on `PATH`: `cargo test -p pf-vdisplay --lib -- --ignored live_flatpak_shim`. A nested
    /// `bwrap` must still run, as pressure-vessel and the real flatpak start one.
    #[test]
    #[ignore = "needs bwrap, user namespaces and a stand-in flatpak on PATH"]
    fn live_flatpak_shim_covers_an_absolute_flatpak() {
        let flatpak = std::fs::canonicalize(on_path("flatpak").unwrap()).unwrap();
        let mut argv = with_flatpak_shim(Vec::new());
        assert!(!argv.is_empty(), "the shim did not apply");
        let inner = format!(
            "{} --user run org.x.Y; {} --dev-bind / / true && echo nested-ok",
            flatpak.display(),
            bwrap_bin().unwrap().display()
        );
        argv.extend(["sh".into(), "-c".into(), inner]);
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-5")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("--user|run|--socket=x11|org.x.Y|WL=wayland-gamescope-5"),
            "{text} / {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("nested-ok"), "{text}");
    }

    /// The real binary is parked before the shim covers its path, and both are read-only.
    #[test]
    fn the_shim_binds_park_the_real_flatpak() {
        let dir = std::env::temp_dir().join(format!("pf-flatpak-binds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let binds = write_shim(&dir, Path::new("/usr/bin/flatpak")).unwrap();
        let spell = |n: &str| dir.join(n).to_string_lossy().into_owned();
        assert_eq!(
            binds,
            [
                "--ro-bind".to_string(),
                "/usr/bin/flatpak".into(),
                spell("flatpak.real"),
                "--ro-bind".into(),
                spell("flatpak"),
                "/usr/bin/flatpak".into(),
            ]
        );
        assert!(dir.join("flatpak.real").is_file(), "the mount point exists");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn joined(argv: &[String], flag: &str) -> Vec<Vec<String>> {
        argv.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == flag)
            .map(|(i, _)| argv[i + 1..(i + 3).min(argv.len())].to_vec())
            .collect()
    }

    /// The order is the whole mechanism: the real `/dev` is parked before `--dev` hides it, the
    /// seat's directory lands on `/dev/input`, and every `hidraw` number has a link.
    #[test]
    fn the_sandbox_parks_the_real_dev_before_it_replaces_it() {
        let argv = argv(
            Path::new("/usr/bin/bwrap"),
            Path::new("/run/user/1000/pf-dev"),
            &["/dev/dri".to_string()],
        );
        assert_eq!(argv[0], "/usr/bin/bwrap");
        let pos = |a: &str| argv.iter().position(|x| x == a).unwrap();
        assert!(
            pos("--dev-bind") < pos("--dev"),
            "--dev would hide the source of the bind that parks it"
        );
        assert_eq!(
            joined(&argv, "--dev-bind"),
            [
                ["/dev", "/run/user/1000/pf-dev/hostdev"],
                ["/dev/dri", "/dev/dri"]
            ],
            "the real /dev is parked, then the GPU nodes come back"
        );
        assert_eq!(
            joined(&argv, "--bind"),
            [["/", "/"], ["/run/user/1000/pf-dev/input", "/dev/input"]]
        );
        let links = joined(&argv, "--symlink");
        assert_eq!(links.len(), 64, "every hidraw number a pad can take");
        assert_eq!(
            links[0],
            ["/run/user/1000/pf-dev/hidraw/hidraw0", "/dev/hidraw0"]
        );
        assert_eq!(
            links[63],
            ["/run/user/1000/pf-dev/hidraw/hidraw63", "/dev/hidraw63"]
        );
        assert_eq!(argv.last().unwrap(), "--die-with-parent");
    }

    /// Only nodes the box has, in one order whatever `read_dir` returns.
    #[test]
    fn only_the_nodes_this_box_has_are_bound_back() {
        let dir = std::env::temp_dir().join(format!("pf-sandbox-nodes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dri")).unwrap();
        for name in ["nvidiactl", "nvidia0", "uinput", "kfd"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let spell = |n: &str| dir.join(n).to_string_lossy().into_owned();
        assert_eq!(
            aux_nodes(&dir),
            [
                spell("dri"),
                spell("uinput"),
                spell("kfd"),
                spell("nvidia0"),
                spell("nvidiactl")
            ],
            "/dev/fuse is absent on this box, so it is not bound"
        );
        assert!(
            aux_nodes(Path::new("/nonexistent-dev")).is_empty(),
            "a directory that is not there is no node at all"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three directories the argv points at, owner-only.
    #[test]
    fn a_seat_directory_holds_the_three_the_argv_names() {
        let dir = std::env::temp_dir().join(format!("pf-sandbox-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_dirs(&dir).unwrap();
        ensure_dirs(&dir).unwrap(); // a re-warm reuses the seat's directory
        for sub in ["hostdev", "input", "hidraw"] {
            assert!(dir.join(sub).is_dir(), "{sub} missing");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
