//! Check catalog: owning-crate verdicts mapped to [`HostCheck`]s.
//!
//! English fallback, impact, and remedy strings live here. Probes stay in `pf-vdisplay` /
//! `pf-inject` / [`crate::detect`] so those crates never depend on host wire types.
//!
//! Two traps the mapping must not collapse:
//!
//! * User-database membership (`id -nG <user>`) and this process's groups (`id -nG`) are
//!   different questions. `usermod -aG` updates the first immediately and the second only
//!   after the next login, so they need different remedies.
//! * `usermod` does not persist on an atomic OS. Universal Blue images want
//!   `ujust add-user-to-input-group`; everywhere else, `usermod -aG input`.

use super::{ids, CheckStatus, Diagnostics, HostCheck, Remedy, Severity};
use crate::inject::{UinputVerdict, VhciVerdict};
use crate::vdisplay::{DriverHealth, TakeoverInapplicable, TakeoverVerdict};
use std::process::Command;

/// Privilege-helper allowlist and vhci attach-node owner — one group, two gates.
const PUNKTFUNK_GROUP: &str = "punktfunk";
/// udev grants uinput/uhid here; not the same group as the vhci nodes.
const INPUT_GROUP: &str = "input";

/// Tests pass an isolated registry; the process-wide one is [`super::registry`].
pub(crate) fn register_all(reg: &Diagnostics) {
    reg.register(takeover_privilege);
    reg.register(virtual_deck_vhci);
    reg.register(uinput_access);
    reg.register(server_conflict);
    reg.register(hyprland_permissions);
    reg.register(omarchy_updates);
    reg.register(vdisplay_driver);
    reg.register(pad_audio);
    reg.register(pad_driver);
    reg.register(plugin_sandbox);
    reg.register(restart_pending);
}

/// Host settings changed that only a restart applies. Pushed again after every settings write.
pub(crate) fn restart_pending() -> HostCheck {
    let id = ids::RESTART_PENDING;
    let names: Vec<&str> = pf_host_config::restart_pending()
        .into_iter()
        .map(|id| pf_host_config::registry::find(id).map_or(id, |s| s.title))
        .collect();
    if names.is_empty() {
        return HostCheck::ok(id, "Every host setting is in effect.");
    }
    HostCheck::problem(
        id,
        CheckStatus::Warn,
        Severity::Warning,
        format!("Restart the host to apply: {}", names.join(", ")),
        "Until then the host runs with the values it started with.",
    )
    .with_remedy(Remedy {
        text: "Restart it from Host → Settings in the web console.".into(),
        command: None,
        relogin_required: false,
    })
    .with_param("settings", names.join(", "))
}

/// Plugins run in their own sandbox, or they do not run: a box that cannot build one says so
/// here rather than quietly running third-party code with the operator's whole account.
#[cfg(target_os = "linux")]
fn plugin_sandbox() -> HostCheck {
    let id = ids::PLUGIN_SANDBOX;
    if !crate::plugins::runtime_status().installed {
        return HostCheck::inapplicable(id, "The plugin runner is not installed on this host.");
    }
    if crate::plugins::runner_sandbox_off() {
        return HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Critical,
            "Plugin sandboxing is turned off",
            "Every installed plugin runs with your account's access: your files, your session, \
             and the host's own credentials."
                .to_string(),
        )
        .with_remedy(Remedy {
            text: "Remove PUNKTFUNK_PLUGIN_SANDBOX from the plugin runner's override, then restart it."
                .into(),
            command: Some("systemctl --user edit punktfunk-scripting".into()),
            relogin_required: false,
        });
    }
    let (reason, fix) = match bwrap_probe() {
        Ok(()) => return HostCheck::ok(id, "Each plugin runs in its own sandbox."),
        Err(None) => (
            "bubblewrap (bwrap) is not installed".to_string(),
            "Install bubblewrap (bwrap) with your package manager, then restart the plugin runner.",
        ),
        Err(Some(why)) => (
            format!("bubblewrap refused the sandbox — {why}"),
            "Allow unprivileged user namespaces and use bubblewrap 0.8 or newer, then restart \
             the plugin runner.",
        ),
    };
    HostCheck::problem(
        id,
        CheckStatus::Fail,
        Severity::Critical,
        "Plugins can't be sandboxed here",
        format!("No plugin will start, so the game library stays empty: {reason}"),
    )
    .with_remedy(Remedy {
        text: fix.into(),
        command: Some("systemctl --user restart punktfunk-scripting".into()),
        relogin_required: false,
    })
}

/// The namespaces and minimal root of every plugin sandbox: `BASE_ARGV` in `sdk/src/sandbox.ts`,
/// held equal by a test. Without the loader symlinks every exec fails, which reads as a refused
/// namespace.
#[cfg(target_os = "linux")]
const SANDBOX_BASE_ARGV: &[&str] = &[
    "--unshare-all",
    "--unshare-user",
    "--disable-userns",
    "--die-with-parent",
    "--new-session",
    "--clearenv",
    "--proc",
    "/proc",
    "--ro-bind",
    "/proc/sys",
    "/proc/sys",
    "--dev",
    "/dev",
    "--tmpfs",
    "/tmp",
    "--ro-bind",
    "/usr",
    "/usr",
    "--ro-bind-try",
    "/etc/ssl",
    "/etc/ssl",
    "--ro-bind-try",
    "/etc/resolv.conf",
    "/etc/resolv.conf",
    "--symlink",
    "usr/lib",
    "/lib",
    "--symlink",
    "usr/lib64",
    "/lib64",
    "--symlink",
    "usr/bin",
    "/bin",
    "--symlink",
    "usr/sbin",
    "/sbin",
];

/// Probe-only binds: a PATH `true` on NixOS is a store path whose loader lives under /nix;
/// /run/current-system is the profile symlink farm. Not in SANDBOX_BASE_ARGV — plugins do
/// not get the whole store read-only for a diagnostic.
#[cfg(target_os = "linux")]
const PROBE_BINDS: &[&str] = &[
    "--ro-bind-try",
    "/nix",
    "/nix",
    "--ro-bind-try",
    "/run/current-system",
    "/run/current-system",
];

/// Host `true` as an absolute path. `/bin/true` is an FHS path NixOS does not have.
#[cfg(target_os = "linux")]
fn which_true() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("true");
            // A file with an exec bit — a directory or unexecutable `true` on PATH makes a
            // healthy sandbox read as an exec failure. Do not canonicalize: Nix `true` is a
            // symlink onto the coreutils multicall binary.
            if candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            {
                return candidate;
            }
        }
    }
    "/bin/true".into()
}

/// Can bwrap build the sandbox a plugin gets? `Err(None)` when bwrap is missing, else bwrap's
/// own first stderr line.
#[cfg(target_os = "linux")]
fn bwrap_probe() -> Result<(), Option<String>> {
    let true_bin = which_true();
    let mut cmd = Command::new("bwrap");
    cmd.args(SANDBOX_BASE_ARGV).args(PROBE_BINDS);
    // A `true` under a PATH dir the sandbox does not bind (a ~/bin on FHS) still execs:
    // its parent comes along.
    if let Some(dir) = true_bin
        .parent()
        .filter(|d| *d != std::path::Path::new("/"))
    {
        cmd.arg("--ro-bind-try").arg(dir).arg(dir);
    }
    let out = cmd.arg(&true_bin).output().map_err(|_| None)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.lines().next().unwrap_or_default().trim();
    Err(Some(if line.is_empty() {
        format!("bwrap exited with {}", out.status)
    } else {
        line.to_string()
    }))
}

/// Windows de-privileges the runner with its own account instead.
#[cfg(not(target_os = "linux"))]
fn plugin_sandbox() -> HostCheck {
    HostCheck::inapplicable(
        ids::PLUGIN_SANDBOX,
        "The plugin runner here runs as its own low-privilege account rather than in a sandbox.",
    )
}

/// The controller speaker endpoint takes the host's 4-channel open, or DualSense titles fall
/// back to rumble and the pad stays silent.
#[cfg(windows)]
fn pad_audio() -> HostCheck {
    use crate::audio::pad_endpoint::PadAudioHealth;
    let id = ids::PAD_AUDIO;
    match crate::audio::pad_endpoint::health() {
        PadAudioHealth::Off => {
            HostCheck::inapplicable(id, "Controller audio is turned off on this host.")
        }
        PadAudioHealth::NotProvisioned => HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Warning,
            "The controller speaker endpoint is not set up",
            "Games see no DualSense speaker, so haptics fall back to rumble. The host retries \
             at the next connect.",
        ),
        PadAudioHealth::Ok { slots } => HostCheck::ok(
            id,
            format!("The controller speaker endpoint takes the host's open ({slots} pad slot(s))."),
        ),
        PadAudioHealth::Refused { pad } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Warning,
            "The controller speaker endpoint refuses the 4-channel format",
            "Games can't pair the DualSense with its speaker, so haptics fall back to rumble \
             and the pad stays silent.",
        )
        .with_remedy(Remedy {
            text: "Update Steam — its streaming audio driver backs the endpoint — and restart \
                   the punktfunk host. If it stays, run `punktfunk-host pad-endpoint repair` \
                   as administrator."
                .to_string(),
            command: None,
            relogin_required: false,
        })
        .with_param("pad", pad.to_string()),
    }
}

#[cfg(not(windows))]
fn pad_audio() -> HostCheck {
    HostCheck::inapplicable(
        ids::PAD_AUDIO,
        "The controller speaker endpoint is a Windows component.",
    )
}

/// What the last virtual pad saw of the Windows gamepad driver. A stale package still attaches,
/// so nothing but this row and a log line says the pads run old behaviour.
#[cfg(windows)]
fn pad_driver() -> HostCheck {
    use crate::inject::PadDriverVerdict as V;
    let id = ids::PAD_DRIVER;
    let reinstall = || Remedy {
        text: "Reinstall the host (the installer bundles the matching controller drivers), then \
               reconnect the controller."
            .to_string(),
        command: None,
        relogin_required: false,
    };
    match crate::inject::pad_driver_probe() {
        V::Unseen => HostCheck::ok(id, "No controller has connected since the host started."),
        V::Current => HostCheck::ok(id, "The virtual controller driver matches this host."),
        V::Stale {
            driver_rev,
            host_rev,
        } => HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Warning,
            format!("The virtual controller driver is revision {driver_rev}; this host needs {host_rev}"),
            "Controllers work, but with the old driver's bugs.",
        )
        .with_remedy(reinstall())
        .with_param("driver_rev", driver_rev.to_string())
        .with_param("host_rev", host_rev.to_string()),
        V::ProtocolMismatch {
            driver_proto,
            host_proto,
        } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!(
                "The virtual controller driver speaks protocol {driver_proto}; this host needs \
                 {host_proto}"
            ),
            "Games see the controller, but it never moves: the driver refuses the host.",
        )
        .with_remedy(reinstall())
        .with_param("driver_protocol", driver_proto.to_string())
        .with_param("host_protocol", host_proto.to_string()),
        V::WrongIdentity { want, got } => {
            let hex = |(v, p): (u16, u16)| format!("{v:04X}:{p:04X}");
            HostCheck::problem(
                id,
                CheckStatus::Fail,
                Severity::Warning,
                format!("A virtual controller showed up as {}, not {}", hex(got), hex(want)),
                "Games see a different controller than the player picked, so buttons and \
                 prompts can be wrong.",
            )
            .with_remedy(reinstall())
            .with_param("want", hex(want))
            .with_param("got", hex(got))
        }
        V::NotAttached => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Warning,
            "No driver picked up the last virtual controller",
            "Games get no input from that controller.",
        )
        .with_remedy(reinstall()),
    }
}

#[cfg(not(windows))]
fn pad_driver() -> HostCheck {
    HostCheck::inapplicable(
        ids::PAD_DRIVER,
        "The virtual controller driver is a Windows component.",
    )
}

/// The Windows virtual-display driver answers, or the host has no video at all. A driver whose
/// host process hangs still handshakes and streams audio, so nothing else reports it.
fn vdisplay_driver() -> HostCheck {
    let id = ids::VDISPLAY_DRIVER;
    let reinstall = || Remedy {
        text: "Reinstall the host — the installer bundles the matching driver.".to_string(),
        command: None,
        relogin_required: false,
    };
    match crate::vdisplay::driver_health() {
        DriverHealth::Inapplicable => {
            HostCheck::inapplicable(id, "The virtual display driver is a Windows component.")
        }
        DriverHealth::Ok { protocol } => HostCheck::ok(
            id,
            format!("The virtual display driver answers (protocol {protocol})."),
        )
        .with_param("protocol", protocol.to_string()),
        DriverHealth::Wedged => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            "The virtual display driver is not answering",
            "Every stream connects with audio and a black screen: the host can't create its \
             virtual monitor, so no video is ever captured, and the stuck sessions keep the host \
             reporting busy.",
        )
        .with_remedy(Remedy {
            text: "Restart the punktfunk host. If it happens again, disable and re-enable the \
                   punktfunk Virtual Display adapter in Device Manager, or reboot. RivaTuner \
                   Statistics Server (MSI Afterburner) is a known cause — close it before the \
                   next connect."
                .to_string(),
            command: None,
            relogin_required: false,
        }),
        DriverHealth::Absent => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            "The virtual display driver is not installed",
            "No virtual monitor can be created, so every stream is a black screen.",
        )
        .with_remedy(reinstall()),
        DriverHealth::Outdated { driver, host } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("The virtual display driver speaks protocol {driver}; this host needs {host}"),
            "The host refuses a mismatched driver, so no virtual monitor can be created."
                .to_string(),
        )
        .with_remedy(reinstall())
        .with_param("driver_protocol", driver.to_string())
        .with_param("host_protocol", host.to_string()),
        DriverHealth::NotReady { detail } => HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Warning,
            "The virtual display driver did not open",
            format!(
                "The host retries on every connect, so this may be a driver still starting up. \
                 If it stays, streams have no video. ({detail})"
            ),
        )
        .with_remedy(Remedy {
            text: "Wait a moment and refresh. If it stays, check the punktfunk Virtual Display \
                   adapter in Device Manager, or reboot."
                .to_string(),
            command: None,
            relogin_required: false,
        })
        .with_param("error", detail),
    }
}

/// Hyprland 0.49+ `ecosystem.enforce_permissions`. Off by default; when on and ungranted,
/// screencopy and virtual input fail with no error from host or compositor.
fn hyprland_permissions() -> HostCheck {
    let id = ids::HYPRLAND_PERMISSIONS;
    if !cfg!(target_os = "linux") {
        return HostCheck::inapplicable(id, "Hyprland's permission system is a Linux feature.");
    }
    // Same Hyprland-session probe as the backend. Missing binary or a different compositor is
    // inapplicable, not a failure.
    let Some(out) = command_output(
        "hyprctl",
        &["-j", "getoption", "ecosystem:enforce_permissions"],
    ) else {
        return HostCheck::inapplicable(
            id,
            "This machine is not running a Hyprland session, so Hyprland's permission system \
             does not apply.",
        );
    };
    let enforced = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|j| j.get("int").and_then(|v| v.as_i64()))
        .is_some_and(|v| v != 0);
    if !enforced {
        return HostCheck::ok(
            id,
            "Hyprland is not enforcing per-application permissions, so nothing needs granting.",
        );
    }
    HostCheck::problem(
        id,
        CheckStatus::Warn,
        // Warn, not Critical: this probe sees enforcement, not grant. A granted host streams
        // fine; from outside the compositor the two are indistinguishable.
        Severity::Warning,
        "Hyprland is enforcing permissions and this host may not be granted".to_string(),
        "Hyprland denies screencopy and virtual input SILENTLY — the client sees black frames and \
         input that does nothing, and neither the host nor the compositor logs an error. If \
         streaming already works, the host is already granted and there is nothing to do."
            .to_string(),
    )
    .with_remedy(Remedy {
        text: "Grant this host screencopy and virtual input in your Hyprland config, then reload \
               it. On a Lua-era config (Hyprland 4.x / Omarchy) the lines go in hyprland.lua or a \
               module it includes; on hyprlang they are `permission = …` lines."
            .to_string(),
        command: Some(
            "o.permission(\"/usr/bin/punktfunk-host\", \"screencopy\", \"allow\")\n\
             o.permission(\"/usr/bin/punktfunk-host\", \"plugin\", \"allow\")"
                .to_string(),
        ),
        relogin_required: false,
    })
}

/// Omarchy has no console apply button; updates are `omarchy update`. This row exists so the
/// diagnostics page says so instead of looking like the button is missing.
fn omarchy_updates() -> HostCheck {
    let id = ids::OMARCHY_UPDATES;
    if !crate::osinfo::is_omarchy() {
        return HostCheck::inapplicable(id, "This machine is not running Omarchy.");
    }
    let version = command_output("omarchy-version", &[]).unwrap_or_default();
    let pretty = &crate::osinfo::detect().pretty;
    let summary = if version.is_empty() {
        format!("{pretty}: update with `omarchy update`")
    } else {
        format!("{version}: update with `omarchy update`")
    };
    HostCheck::ok(id, summary)
        .with_param("update_command", "omarchy update")
        .with_param("version", version)
}

/// Hyprland and Omarchy have no owning-crate probe, so these two rows shell out. Other catalog
/// checks must not grow a third.
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn takeover_privilege() -> HostCheck {
    let id = ids::TAKEOVER_PRIVILEGE;
    match crate::vdisplay::takeover_privilege_verdict() {
        TakeoverVerdict::Inapplicable { why } => {
            HostCheck::inapplicable(id, takeover_inapplicable_reason(why))
        }
        TakeoverVerdict::Ok { user, group } => {
            HostCheck::ok(id, format!("User “{user}” is in the “{group}” group."))
                .with_param("user", user)
                .with_param("group", group)
        }
        TakeoverVerdict::MissingMembership {
            user,
            dm,
            helper,
            group,
        } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("User “{user}” is not in the “{group}” group"),
            format!(
                "Streams that need the managed takeover cannot stop {dm}, so every one of them \
                 degrades to mirroring this machine's own session instead. With the panel off that \
                 looks like a black screen on every connect, and nothing else reports it."
            ),
        )
        .with_remedy(Remedy {
            text: format!(
                "Add the user to the “{group}” group, then log out and back in. The same group \
                 gates the virtual Steam Deck pad's usbip nodes, which can present arbitrary \
                 emulated USB devices — join it only on a machine you trust."
            ),
            command: Some(format!("sudo usermod -aG {group} {user}")),
            // Helper reads the user database and is satisfied at once; this process keeps the
            // group set it started with. One re-login covers both.
            relogin_required: true,
        })
        .with_param("user", user)
        .with_param("group", group)
        .with_param("dm", dm)
        .with_param("helper", helper),
    }
}

fn takeover_inapplicable_reason(why: TakeoverInapplicable) -> &'static str {
    match why {
        TakeoverInapplicable::Root => {
            "The host runs as root, so it stops the display manager directly and never needs the \
             privilege helper."
        }
        TakeoverInapplicable::NoDisplayManager => {
            "No display manager drives this machine's logins, so a takeover has nothing to stop."
        }
        TakeoverInapplicable::NoManagedSession => {
            "This machine has no gamescope session infrastructure, so the managed takeover never \
             runs here."
        }
        TakeoverInapplicable::NoPackagedHelper => {
            "This is an unpackaged install: it has no privilege helper and no group, and uses the \
             polkit rule from the documentation instead."
        }
        TakeoverInapplicable::UnknownUser => {
            "The host's user name could not be resolved, so no group membership could be checked."
        }
        TakeoverInapplicable::NotLinux => "The managed gamescope takeover is a Linux feature.",
    }
}

fn virtual_deck_vhci() -> HostCheck {
    let id = ids::VIRTUAL_DECK_VHCI;
    let group = PUNKTFUNK_GROUP;
    match crate::inject::vhci_probe() {
        VhciVerdict::Inapplicable { why } => HostCheck::inapplicable(id, why),
        VhciVerdict::Ok => HostCheck::ok(id, "The virtual Steam Deck controller can attach."),
        VhciVerdict::ModuleMissing => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Warning,
            "The vhci_hcd kernel module is not loaded",
            "The virtual Steam Deck controller cannot attach, so Steam Input never sees it — in \
             Game Mode that means nothing can be navigated with a pad.",
        )
        .with_remedy(Remedy {
            text: "Load the vhci_hcd module (the packages install a modules-load rule that does \
                   this at boot; on an unpackaged install, load it by hand)."
                .to_string(),
            command: Some("sudo modprobe vhci-hcd".to_string()),
            relogin_required: false,
        }),
        // Node exists, not writable. The three causes need different remedies; only userdb vs
        // process groups can tell them apart.
        VhciVerdict::NotWritable { path } => not_writable_check(id, group, path),
    }
}

fn not_writable_check(id: &str, group: &str, path: String) -> HostCheck {
    let user = current_user();
    let in_userdb = user.as_deref().and_then(|u| user_in_group_userdb(u, group));
    let in_process = process_in_group(group);

    let base = |summary: &str, impact: &str| {
        HostCheck::problem(id, CheckStatus::Fail, Severity::Warning, summary, impact)
            .with_param("group", group)
            .with_param("path", path.clone())
    };
    let pad_impact = "The virtual Steam Deck controller cannot attach, so Steam Input never sees \
                      it — in Game Mode that means nothing can be navigated with a pad.";

    match (in_userdb, in_process) {
        // Granted in the user database, not in this process. `systemd --user` keeps the group
        // set it started with; only a re-login updates it.
        (Some(true), Some(false)) => base(
            &format!("The group “{group}” was granted but this session predates it"),
            pad_impact,
        )
        .with_remedy(Remedy {
            text: "Log out and back in. The membership is already recorded — this session just \
                   started before it was granted, and a session keeps the group set it began with."
                .to_string(),
            command: None,
            relogin_required: true,
        })
        .with_param("user", user.unwrap_or_default()),

        (Some(false), _) => {
            let user = user.unwrap_or_default();
            base(
                &format!("User “{user}” is not in the “{group}” group"),
                pad_impact,
            )
            .with_remedy(Remedy {
                text: format!(
                    "Add the user to the “{group}” group, then log out and back in. This group \
                     can present arbitrary emulated USB devices — join it only on a machine you \
                     trust."
                ),
                command: Some(format!("sudo usermod -aG {group} {user}")),
                relogin_required: true,
            })
            .with_param("user", user)
        }

        // Member in userdb and in this process: the udev rule never chgrp'd the node. Do not
        // send the operator back through `usermod`.
        (Some(true), Some(true)) => base(
            "The vhci attach node is not owned by the expected group",
            pad_impact,
        )
        .with_remedy(Remedy {
            text: format!(
                "Install the udev rule that grants the “{group}” group access to the vhci nodes \
                 (scripts/60-punktfunk.rules — the packages install it), then reload the rules or \
                 reboot."
            ),
            command: Some("sudo udevadm control --reload && sudo udevadm trigger".to_string()),
            relogin_required: false,
        }),

        // User database unreachable. Do not guess a cause; a wrong remedy costs more than a
        // vague one.
        _ => base(
            "The virtual Steam Deck controller's attach node is not writable",
            pad_impact,
        )
        .with_remedy(Remedy {
            text: format!(
                "Check that this machine's user is in the “{group}” group and that the udev rule \
                 granting it access to the vhci nodes is installed, then log out and back in."
            ),
            command: None,
            relogin_required: true,
        }),
    }
}

fn uinput_access() -> HostCheck {
    let id = ids::UINPUT_ACCESS;
    match crate::inject::uinput_probe() {
        UinputVerdict::Inapplicable => HostCheck::inapplicable(
            id,
            "Virtual controllers are created through this platform's own driver stack rather than \
             uinput.",
        ),
        UinputVerdict::Ok => HostCheck::ok(id, "The input device nodes are reachable."),

        // Node exists, open denied. Remedy depends on whether this OS persists `usermod`.
        UinputVerdict::PermissionDenied { path } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("No permission to open {path}"),
            "Every virtual controller fails to be created, so games see no gamepad at all — and \
             the pen and tablet input paths are dead with it."
                .to_string(),
        )
        .with_remedy(input_group_remedy())
        .with_param("path", path)
        .with_param("group", INPUT_GROUP),

        // Node missing: a group-membership remedy is the wrong turn.
        UinputVerdict::Missing { path } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("{path} does not exist"),
            "Every virtual controller fails to be created, so games see no gamepad at all."
                .to_string(),
        )
        .with_remedy(Remedy {
            text: format!(
                "Load the kernel module that provides {path} and install the udev rule that grants \
                 the “{INPUT_GROUP}” group access to it (scripts/60-punktfunk.rules — the packages \
                 install both)."
            ),
            command: None,
            relogin_required: false,
        })
        .with_param("path", path),

        UinputVerdict::Error { path, message } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("{path} could not be opened: {message}"),
            "Virtual controllers may fail to be created, so games can see no gamepad.".to_string(),
        )
        .with_remedy(Remedy {
            text: format!("Check the state of {path} on this machine."),
            command: None,
            relogin_required: false,
        })
        .with_param("path", path)
        .with_param("error", message),
    }
}

/// On Universal Blue, `usermod -aG input` looks successful and is reverted: `/etc/group` is not
/// writable state. `packaging/README.md`.
fn input_group_remedy() -> Remedy {
    let user = current_user().unwrap_or_else(|| "$USER".to_string());
    if is_universal_blue() {
        Remedy {
            text:
                "Add the user to the “input” group with ujust, then log out and back in. On this \
                   OS a plain `usermod` does not persist."
                    .to_string(),
            command: Some("ujust add-user-to-input-group".to_string()),
            relogin_required: true,
        }
    } else {
        Remedy {
            text: "Add the user to the “input” group, then log out and back in. If the group \
                   already lists the user, the udev rule granting it access may be missing \
                   (scripts/60-punktfunk.rules)."
                .to_string(),
            command: Some(format!("sudo usermod -aG {INPUT_GROUP} {user}")),
            relogin_required: true,
        }
    }
}

/// Match the chain's leaf (`ID`), never the `fedora` family. Workstation is mutable and wants
/// `usermod`. Bazzite is `linux/fedora/bazzite`.
fn is_universal_blue() -> bool {
    matches!(
        crate::osinfo::detect().chain.rsplit('/').next(),
        Some("bazzite" | "bluefin" | "aurora")
    )
}

fn server_conflict() -> HostCheck {
    let id = ids::SERVER_CONFLICT;
    // Same cached scan as the tray and Host card. Empty also means "never scanned" (no
    // GameStream planes); treat that as healthy, matching `LocalSummary.conflicts`.
    let labels = crate::detect::summary_labels(crate::detect::snapshot());
    if labels.is_empty() {
        return HostCheck::ok(
            id,
            "No other game-streaming server is active on this machine.",
        );
    }
    let servers = labels.join(", ");
    HostCheck::problem(
        id,
        CheckStatus::Fail,
        Severity::Critical,
        format!("Another game-streaming server is active: {servers}"),
        "Both servers bind the same ports, so whichever won the bind answers — pairing and \
         connections can land on the other server while this host looks installed and healthy."
            .to_string(),
    )
    .with_remedy(Remedy {
        text: "Stop the other server (and disable it if it starts on its own), then restart this \
               host."
            .to_string(),
        command: None,
        relogin_required: false,
    })
    .with_param("servers", servers)
}

/// This process's login name, resolved the way `pkexec` will (`id -un`).
fn current_user() -> Option<String> {
    capture(Command::new("id").arg("-un"))
}

/// User-database membership (`id -nG <user>`): what a root helper sees and what `usermod -aG`
/// updates immediately. `None` if NSS fails — a false miss sends the operator down the wrong path.
fn user_in_group_userdb(user: &str, group: &str) -> Option<bool> {
    let groups = capture(Command::new("id").args(["-nG", user]))?;
    Some(groups.split_whitespace().any(|g| g == group))
}

/// This process's supplementary groups (`id -nG`, no operand). Frozen at `systemd --user`
/// start; a fresh `usermod` is invisible until the next login.
fn process_in_group(group: &str) -> Option<bool> {
    let groups = capture(Command::new("id").arg("-nG"))?;
    Some(groups.split_whitespace().any(|g| g == group))
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doctor must probe the sandbox the runner actually builds.
    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_probe_matches_the_runner() {
        let ts = include_str!("../../../../sdk/src/sandbox.ts");
        let body = ts
            .split_once("const BASE_ARGV: readonly string[] = [")
            .and_then(|(_, rest)| rest.split_once("];"))
            .expect("BASE_ARGV in sdk/src/sandbox.ts")
            .0;
        let runner: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('"'))
            .map(|l| l.trim_end_matches(',').trim_matches('"'))
            .collect();
        assert_eq!(runner, SANDBOX_BASE_ARGV);
    }

    /// This machine may not reach every arm; a `NotWritable` row is still Fail+Warning and
    /// always carries a remedy.
    #[test]
    fn vhci_not_writable_shapes_produce_distinct_remedies() {
        let path = "/sys/devices/platform/vhci_hcd.0/attach".to_string();
        let check = not_writable_check(ids::VIRTUAL_DECK_VHCI, PUNKTFUNK_GROUP, path.clone());
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.severity, Severity::Warning);
        assert!(check.remedy.is_some());
        assert!(!check.impact.is_empty());
        assert_eq!(
            check.params.get("group").map(String::as_str),
            Some(PUNKTFUNK_GROUP)
        );
    }

    #[test]
    fn takeover_inapplicable_reasons_are_all_populated() {
        for why in [
            TakeoverInapplicable::Root,
            TakeoverInapplicable::NoDisplayManager,
            TakeoverInapplicable::NoManagedSession,
            TakeoverInapplicable::NoPackagedHelper,
            TakeoverInapplicable::UnknownUser,
            TakeoverInapplicable::NotLinux,
        ] {
            assert!(
                !takeover_inapplicable_reason(why).trim().is_empty(),
                "{why:?} needs a reason — an inapplicable row exists to answer \"why not here?\""
            );
        }
    }

    /// Atomic-OS branch: `usermod` looks successful on Bazzite and is gone after reboot.
    #[test]
    fn input_remedy_matches_this_box_flavour() {
        let remedy = input_group_remedy();
        let command = remedy
            .command
            .expect("the input remedy is always pasteable");
        assert!(remedy.relogin_required, "a group change needs a re-login");
        if is_universal_blue() {
            assert_eq!(command, "ujust add-user-to-input-group");
        } else {
            assert!(
                command.starts_with("sudo usermod -aG input "),
                "unexpected remedy: {command}"
            );
        }
    }

    /// Match the leaf. Matching the `fedora` family would send Workstation users to a `ujust`
    /// they do not have.
    #[test]
    fn universal_blue_is_matched_on_the_leaf_not_the_family() {
        fn leaf_is_ublue(chain: &str) -> bool {
            matches!(
                chain.rsplit('/').next(),
                Some("bazzite" | "bluefin" | "aurora")
            )
        }
        assert!(leaf_is_ublue("linux/fedora/bazzite"));
        assert!(leaf_is_ublue("linux/fedora/bluefin"));
        assert!(!leaf_is_ublue("linux/fedora"));
        assert!(!leaf_is_ublue("linux/fedora/fedora"));
        assert!(!leaf_is_ublue("linux/arch/steamos"));
    }

    #[test]
    fn server_conflict_is_ok_when_nothing_was_detected() {
        // Test binaries never ran a startup scan; empty matches a clean box.
        let check = server_conflict();
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.remedy.is_none());
    }
}
