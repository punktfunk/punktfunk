//! Check catalog: owning-crate verdicts mapped to [`HostCheck`]s.
//!
//! English fallback, impact, and remedy strings live here. Probes stay in `pf-vdisplay` /
//! `pf-inject` / [`crate::detect`] so those crates never depend on host wire types.
//!
//! Two traps the mapping must not collapse:
//!
//! * User-database membership (`id -nG <user>`) and this process's groups (`id -nG`) are
//!   different questions. `usermod -aG` updates the first immediately and the second only
//!   after a restart (a lingering `systemd --user` outlives a log-out), so they need
//!   different remedies.
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

/// Tests pass an isolated registry; the process-wide one is [`super::registry`]. The door opens
/// no device and runs no plugin, and sits outside the device groups on purpose: it probes only
/// the box's own checks, since any other would say nothing about it and its remedy would undo
/// the separation.
pub(crate) fn register_all(reg: &Diagnostics, door: bool) {
    let local = |id: &'static str, probe: fn() -> HostCheck, why: (&'static str, &'static str)| {
        if door {
            reg.register(move || HostCheck::inapplicable(id, why.1).worded(why.0, None, None));
        } else {
            reg.register(probe);
        }
    };
    let no_device = ("diag_door_no_device_summary", "The door opens no device.");
    let device = |id, probe| local(id, probe, no_device);
    device(ids::TAKEOVER_PRIVILEGE, takeover_privilege);
    device(ids::VIRTUAL_DECK_VHCI, virtual_deck_vhci);
    device(ids::UINPUT_ACCESS, uinput_access);
    reg.register(server_conflict);
    device(ids::HYPRLAND_PERMISSIONS, hyprland_permissions);
    device(ids::NIRI_VIRTUAL_OUTPUTS, niri_virtual_outputs);
    reg.register(omarchy_updates);
    device(ids::VDISPLAY_DRIVER, vdisplay_driver);
    device(ids::PAD_AUDIO, pad_audio);
    device(ids::PAD_DRIVER, pad_driver);
    device(ids::ENCODER_SHARING, encoder_sharing);
    let no_plugins = (
        "diag_plugin_sandbox_door_summary",
        "The door runs no plugins. The owner's host runs them.",
    );
    local(ids::PLUGIN_SANDBOX, plugin_sandbox, no_plugins);
    reg.register(restart_pending);
}

/// Host settings changed that only a restart applies. Pushed again after every settings write.
pub(crate) fn restart_pending() -> HostCheck {
    let id = ids::RESTART_PENDING;
    let pending = pf_host_config::restart_pending();
    let names: Vec<&str> = pending
        .iter()
        .map(|&id| pf_host_config::registry::find(id).map_or(id, |s| s.title))
        .collect();
    if names.is_empty() {
        return HostCheck::ok(id, "Every host setting is in effect.").worded(
            "diag_restart_pending_ok_summary",
            None,
            None,
        );
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
    // `settings` holds English titles; an app words `setting_<id>` for each of these.
    .with_param("setting_ids", pending.join(","))
    .worded(
        "diag_restart_pending_summary",
        Some("diag_restart_pending_impact"),
        Some("diag_restart_pending_remedy"),
    )
}

/// The runner's own refusal, unless it runs again since. Its probe runs inside its unit, which
/// can refuse what this process may do, so the file it leaves is the only way to hear of it.
#[cfg(any(target_os = "linux", windows))]
fn runner_refusal(status: &crate::plugins::RuntimeStatus) -> Option<String> {
    if status.running {
        return None;
    }
    crate::plugins::sandbox_refusal()
}

/// The runner stopped rather than run a plugin unsandboxed. `why` is its reason, `start` the
/// command that starts it again on this platform.
#[cfg(any(target_os = "linux", windows))]
fn runner_stopped(id: &str, why: &str, start: &str) -> HostCheck {
    let (cause, impact_id) = if why.is_empty() {
        (String::new(), "diag_plugin_sandbox_stopped_impact")
    } else {
        (
            format!(": {why}"),
            "diag_plugin_sandbox_stopped_error_impact",
        )
    };
    HostCheck::problem(
        id,
        CheckStatus::Fail,
        Severity::Critical,
        "The plugin runner has stopped",
        format!(
            "It couldn't build a plugin sandbox when it started, so no plugin runs and the game \
             library stays empty{cause}"
        ),
    )
    .with_remedy(Remedy {
        text: "Repair what it names, then start the plugin runner again.".into(),
        command: Some(start.into()),
        relogin_required: false,
    })
    .with_param("error", why)
    .worded(
        "diag_plugin_sandbox_stopped_summary",
        Some(impact_id),
        Some("diag_plugin_sandbox_stopped_remedy"),
    )
}

/// Plugins run in their own sandbox, or the runner stops: a box that cannot build one says so
/// here rather than quietly running third-party code with the operator's whole account.
#[cfg(target_os = "linux")]
fn plugin_sandbox() -> HostCheck {
    let id = ids::PLUGIN_SANDBOX;
    let status = crate::plugins::runtime_status();
    if !status.installed {
        return HostCheck::inapplicable(id, "The plugin runner is not installed on this host.")
            .worded("diag_plugin_sandbox_na_not_installed", None, None);
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
        })
        .worded(
            "diag_plugin_sandbox_off_summary",
            Some("diag_plugin_sandbox_off_impact"),
            Some("diag_plugin_sandbox_off_remedy"),
        );
    }
    let start = "systemctl --user restart punktfunk-scripting";
    let (reason, fix, keys, error) = match bwrap_probe() {
        Ok(()) => {
            return match runner_refusal(&status) {
                Some(why) => runner_stopped(id, &why, start),
                None => HostCheck::ok(id, "Each plugin runs in its own sandbox.").worded(
                    "diag_plugin_sandbox_ok_summary",
                    None,
                    None,
                ),
            }
        }
        Err(None) => (
            "bubblewrap (bwrap) is not installed".to_string(),
            "Install bubblewrap (bwrap) with your package manager, then start the plugin runner \
             again.",
            (
                "diag_plugin_sandbox_no_bwrap_impact",
                "diag_plugin_sandbox_no_bwrap_remedy",
            ),
            None,
        ),
        Err(Some(why)) => (
            format!("bubblewrap refused the sandbox — {why}"),
            "Allow unprivileged user namespaces and use bubblewrap 0.8 or newer, then start the \
             plugin runner again.",
            (
                "diag_plugin_sandbox_bwrap_refused_impact",
                "diag_plugin_sandbox_bwrap_refused_remedy",
            ),
            Some(why),
        ),
    };
    let check = HostCheck::problem(
        id,
        CheckStatus::Fail,
        Severity::Critical,
        "Plugins can't be sandboxed here",
        format!(
            "The plugin runner stops instead of running a plugin unsandboxed, so the game \
             library stays empty: {reason}"
        ),
    )
    .with_remedy(Remedy {
        text: fix.into(),
        command: Some(start.into()),
        relogin_required: false,
    });
    let check = match error {
        Some(why) => check.with_param("error", why),
        None => check,
    };
    check.worded(
        "diag_plugin_sandbox_unavailable_summary",
        Some(keys.0),
        Some(keys.1),
    )
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

/// Each Windows plugin runs in its own AppContainer, unless `host.env` turned that off. A runner
/// that can't build one stops, and its task ends cleanly: only the file it leaves says so.
#[cfg(windows)]
fn plugin_sandbox() -> HostCheck {
    let id = ids::PLUGIN_SANDBOX;
    let status = crate::plugins::runtime_status();
    if !status.installed {
        return HostCheck::inapplicable(id, "The plugin runner is not installed on this host.")
            .worded("diag_plugin_sandbox_na_not_installed", None, None);
    }
    if crate::plugins::runner_sandbox_off() {
        return HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Critical,
            "Plugin sandboxing is turned off",
            "Every installed plugin runs with the plugin runner's whole access: each other's \
             files, the launchers' folders, and the loopback management port."
                .to_string(),
        )
        .with_remedy(Remedy {
            text: "Remove PUNKTFUNK_PLUGIN_SANDBOX from host.env, then restart the host.".into(),
            command: None,
            relogin_required: false,
        })
        .worded(
            "diag_plugin_sandbox_off_summary",
            Some("diag_plugin_sandbox_off_windows_impact"),
            Some("diag_plugin_sandbox_off_windows_remedy"),
        );
    }
    match runner_refusal(&status) {
        Some(why) => runner_stopped(id, &why, "Start-ScheduledTask -TaskName PunktfunkScripting"),
        None => HostCheck::ok(id, "Each plugin runs in its own AppContainer.").worded(
            "diag_plugin_sandbox_ok_windows_summary",
            None,
            None,
        ),
    }
}

/// macOS has no plugin runner.
#[cfg(not(any(target_os = "linux", windows)))]
fn plugin_sandbox() -> HostCheck {
    HostCheck::inapplicable(
        ids::PLUGIN_SANDBOX,
        "There is no plugin runner on this host.",
    )
    .worded("diag_plugin_sandbox_na_no_runner", None, None)
}

/// The controller speaker endpoint takes the host's 4-channel open, or DualSense titles fall
/// back to rumble and the pad stays silent.
#[cfg(windows)]
fn pad_audio() -> HostCheck {
    use crate::audio::pad_endpoint::PadAudioHealth;
    let id = ids::PAD_AUDIO;
    match crate::audio::pad_endpoint::health() {
        PadAudioHealth::Off => HostCheck::inapplicable(
            id,
            "Controller audio is turned off on this host.",
        )
        .worded("diag_pad_audio_na_off", None, None),
        PadAudioHealth::NotProvisioned => HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Warning,
            "The controller speaker endpoint is not set up",
            "Games see no DualSense speaker, so haptics fall back to rumble. The host retries \
             at the next connect.",
        )
        .worded(
            "diag_pad_audio_missing_summary",
            Some("diag_pad_audio_missing_impact"),
            None,
        ),
        PadAudioHealth::Ok { slots } => HostCheck::ok(
            id,
            if slots == 1 {
                "The controller speaker endpoint takes the host's open (1 pad slot).".to_string()
            } else {
                format!(
                    "The controller speaker endpoint takes the host's open ({slots} pad slots)."
                )
            },
        )
        .with_param("count", slots.to_string())
        .worded("diag_pad_audio_ok_summary", None, None),
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
                   the Punktfunk host. If it stays, run `punktfunk-host pad-endpoint repair` \
                   as administrator."
                .to_string(),
            command: None,
            relogin_required: false,
        })
        .with_param("pad", pad.to_string())
        .worded(
            "diag_pad_audio_refused_summary",
            Some("diag_pad_audio_refused_impact"),
            Some("diag_pad_audio_refused_remedy"),
        ),
    }
}

#[cfg(not(windows))]
fn pad_audio() -> HostCheck {
    HostCheck::inapplicable(
        ids::PAD_AUDIO,
        "The controller speaker endpoint is a Windows component.",
    )
    .worded("diag_pad_audio_na_windows_only", None, None)
}

/// Another app on the NVENC engine right now. Only NVIDIA reports its sessions.
fn encoder_sharing() -> HostCheck {
    let id = ids::ENCODER_SHARING;
    if !crate::encoder_sessions::available() {
        return HostCheck::inapplicable(id, "Only NVIDIA GPUs report who uses the video encoder.")
            .worded("diag_encoder_sharing_na_not_nvidia", None, None);
    }
    let others = crate::encoder_sessions::foreign();
    if others.is_empty() {
        return HostCheck::ok(id, "No other app is using the video encoder right now.").worded(
            "diag_encoder_sharing_ok_summary",
            None,
            None,
        );
    }
    let who = crate::encoder_sessions::describe(&others);
    HostCheck::problem(
        id,
        CheckStatus::Warn,
        Severity::Warning,
        "Another app is using the video encoder",
        format!(
            "{who} shares the encoder with the stream, so stream frames wait behind its frames \
             and the picture stutters."
        ),
    )
    .with_remedy(Remedy {
        text: "Close or pause the other recorder (NVIDIA Instant Replay, OBS, Discord) while you \
               stream. On Windows, Pause Instant Replay in the host settings handles Instant \
               Replay by itself."
            .to_string(),
        command: None,
        relogin_required: false,
    })
    .with_param("sessions", who)
    .worded(
        "diag_encoder_sharing_summary",
        Some("diag_encoder_sharing_impact"),
        Some("diag_encoder_sharing_remedy"),
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
        V::Unseen => HostCheck::ok(id, "No controller has connected since the host started.")
            .worded("diag_pad_driver_ok_unseen_summary", None, None),
        V::Current => HostCheck::ok(id, "The virtual controller driver matches this host.")
            .worded("diag_pad_driver_ok_summary", None, None),
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
        .with_param("host_rev", host_rev.to_string())
        .worded(
            "diag_pad_driver_stale_summary",
            Some("diag_pad_driver_stale_impact"),
            Some("diag_pad_driver_remedy"),
        ),
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
        .with_param("host_protocol", host_proto.to_string())
        .worded(
            "diag_pad_driver_protocol_summary",
            Some("diag_pad_driver_protocol_impact"),
            Some("diag_pad_driver_remedy"),
        ),
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
            .worded(
                "diag_pad_driver_identity_summary",
                Some("diag_pad_driver_identity_impact"),
                Some("diag_pad_driver_remedy"),
            )
        }
        V::NotAttached => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Warning,
            "No driver picked up the last virtual controller",
            "Games get no input from that controller.",
        )
        .with_remedy(reinstall())
        .worded(
            "diag_pad_driver_unattached_summary",
            Some("diag_pad_driver_unattached_impact"),
            Some("diag_pad_driver_remedy"),
        ),
    }
}

#[cfg(not(windows))]
fn pad_driver() -> HostCheck {
    HostCheck::inapplicable(
        ids::PAD_DRIVER,
        "The virtual controller driver is a Windows component.",
    )
    .worded("diag_pad_driver_na_windows_only", None, None)
}

/// The Windows virtual-display driver answers, or the host has no video at all. A driver whose
/// host process hangs still handshakes and streams audio, so nothing else reports it.
fn vdisplay_driver() -> HostCheck {
    vdisplay_driver_check(crate::vdisplay::driver_health())
}

fn vdisplay_driver_check(health: DriverHealth) -> HostCheck {
    let id = ids::VDISPLAY_DRIVER;
    let reinstall = || Remedy {
        text: "Reinstall the host — the installer bundles the matching driver.".to_string(),
        command: None,
        relogin_required: false,
    };
    match health {
        DriverHealth::Inapplicable => {
            HostCheck::inapplicable(id, "The virtual display driver is a Windows component.")
                .worded("diag_vdisplay_driver_na_windows_only", None, None)
        }
        DriverHealth::Ok { protocol } => HostCheck::ok(
            id,
            format!("The virtual display driver answers (protocol {protocol})."),
        )
        .with_param("protocol", protocol.to_string())
        .with_param("driver_protocol", protocol.to_string())
        .worded("diag_vdisplay_driver_ok_summary", None, None),
        DriverHealth::Installed => HostCheck::ok(
            id,
            "The virtual display driver is installed. It starts with the next stream.",
        )
        .worded("diag_vdisplay_driver_installed_summary", None, None),
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
            text: "Restart the Punktfunk host. If it happens again, disable and re-enable the \
                   punktfunk Virtual Display adapter in Device Manager, or reboot. RivaTuner \
                   Statistics Server (MSI Afterburner) is a known cause — close it before the \
                   next connect."
                .to_string(),
            command: None,
            relogin_required: false,
        })
        .worded(
            "diag_vdisplay_driver_wedged_summary",
            Some("diag_vdisplay_driver_wedged_impact"),
            Some("diag_vdisplay_driver_wedged_remedy"),
        ),
        DriverHealth::Absent => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            "The virtual display driver is not installed",
            "No virtual monitor can be created, so every stream is a black screen.",
        )
        .with_remedy(reinstall())
        .worded(
            "diag_vdisplay_driver_absent_summary",
            Some("diag_vdisplay_driver_absent_impact"),
            Some("diag_vdisplay_driver_remedy"),
        ),
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
        .with_param("host_protocol", host.to_string())
        .worded(
            "diag_vdisplay_driver_outdated_summary",
            Some("diag_vdisplay_driver_outdated_impact"),
            Some("diag_vdisplay_driver_remedy"),
        ),
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
        .with_param("error", detail)
        .worded(
            "diag_vdisplay_driver_not_ready_summary",
            Some("diag_vdisplay_driver_not_ready_impact"),
            Some("diag_vdisplay_driver_not_ready_remedy"),
        ),
    }
}

/// Hyprland 0.49+ `ecosystem.enforce_permissions`. Off by default; when on and ungranted,
/// screencopy and virtual input fail with no error from host or compositor.
fn hyprland_permissions() -> HostCheck {
    let id = ids::HYPRLAND_PERMISSIONS;
    if !cfg!(target_os = "linux") {
        return HostCheck::inapplicable(id, "Hyprland's permission system is a Linux feature.")
            .worded("diag_hyprland_permissions_na_not_linux", None, None);
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
        )
        .worded("diag_hyprland_permissions_na_no_session", None, None);
    };
    let enforced = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|j| j.get("int").and_then(|v| v.as_i64()))
        .is_some_and(|v| v != 0);
    if !enforced {
        return HostCheck::ok(
            id,
            "Hyprland is not enforcing per-application permissions, so nothing needs granting.",
        )
        .worded("diag_hyprland_permissions_ok_summary", None, None);
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
            "hl.permission(\"/usr/bin/punktfunk-host\", \"screencopy\", \"allow\")\n\
             hl.permission(\"/usr/(lib|libexec|lib64)/xdg-desktop-portal-hyprland\", \
             \"screencopy\", \"allow\")"
                .to_string(),
        ),
        relogin_required: false,
    })
    .worded(
        "diag_hyprland_permissions_summary",
        Some("diag_hyprland_permissions_impact"),
        Some("diag_hyprland_permissions_remedy"),
    )
}

/// Where to get biri, the niri fork that creates virtual outputs.
const BIRI_DOCS: &str = "https://docs.punktfunk.unom.io/docs/niri#get-biri";

/// Only a niri that creates virtual outputs (biri) can give each stream its own display.
fn niri_virtual_outputs() -> HostCheck {
    use crate::vdisplay::Compositor;
    if !matches!(crate::vdisplay::detect(), Ok(Compositor::Niri)) {
        return HostCheck::inapplicable(
            ids::NIRI_VIRTUAL_OUTPUTS,
            "This machine is not running a niri session, so niri's virtual outputs do not apply.",
        )
        .worded("diag_niri_virtual_outputs_na_no_session", None, None);
    }
    niri_verdict(
        crate::vdisplay::probe(Compositor::Niri),
        crate::vdisplay::capture_monitor().is_some(),
    )
}

/// `probe` is the niri capability probe's answer. A pinned monitor still streams on stock niri,
/// so the pin softens the verdict from a failure to a warning.
fn niri_verdict(probe: anyhow::Result<()>, monitor_pinned: bool) -> HostCheck {
    let id = ids::NIRI_VIRTUAL_OUTPUTS;
    let Err(e) = probe else {
        return HostCheck::ok(id, "niri can create a virtual display for each stream.").worded(
            "diag_niri_virtual_outputs_ok_summary",
            None,
            None,
        );
    };
    #[cfg(target_os = "linux")]
    let stock = e
        .downcast_ref::<crate::vdisplay::NoVirtualOutputs>()
        .is_some();
    #[cfg(not(target_os = "linux"))]
    let stock = false;
    if !stock {
        return HostCheck::problem(
            id,
            CheckStatus::Warn,
            Severity::Warning,
            "niri didn't answer the virtual display check",
            format!("Until niri answers, a stream may get no display of its own. ({e:#})"),
        )
        .with_param("error", format!("{e:#}"))
        .worded(
            "diag_niri_virtual_outputs_silent_summary",
            Some("diag_niri_virtual_outputs_silent_impact"),
            None,
        );
    }
    let (status, severity, impact, impact_id) = if monitor_pinned {
        (
            CheckStatus::Warn,
            Severity::Warning,
            "The monitor pinned in the console still streams. Without the pin, every connect \
             fails: this niri can't create a display for the stream.",
            "diag_niri_virtual_outputs_pinned_impact",
        )
    } else {
        (
            CheckStatus::Fail,
            Severity::Critical,
            "Every connect fails: this niri can't create a display for the stream. Pinning a \
             physical monitor in the console streams that monitor instead.",
            "diag_niri_virtual_outputs_impact",
        )
    };
    HostCheck::problem(
        id,
        status,
        severity,
        "This niri can't create virtual displays",
        impact,
    )
    .with_remedy(Remedy {
        text: format!(
            "Install biri, the niri fork that adds virtual outputs: the AUR package `biri-git` \
             replaces `niri` on Arch. Log in again, then restart the host. See {BIRI_DOCS}"
        ),
        command: None,
        relogin_required: true,
    })
    .with_param("url", BIRI_DOCS)
    .worded(
        "diag_niri_virtual_outputs_summary",
        Some(impact_id),
        Some("diag_niri_virtual_outputs_remedy"),
    )
}

/// Omarchy has no console apply button; updates are `omarchy update`. This row exists so the
/// diagnostics page says so instead of looking like the button is missing.
fn omarchy_updates() -> HostCheck {
    let id = ids::OMARCHY_UPDATES;
    if !crate::osinfo::is_omarchy() {
        return HostCheck::inapplicable(id, "This machine is not running Omarchy.").worded(
            "diag_omarchy_updates_na",
            None,
            None,
        );
    }
    let version = command_output("omarchy-version", &[]).unwrap_or_default();
    let pretty = &crate::osinfo::detect().pretty;
    let version = if version.is_empty() {
        pretty.clone()
    } else {
        version
    };
    HostCheck::ok(id, format!("{version}: update with `omarchy update`"))
        .with_param("update_command", "omarchy update")
        .with_param("version", version)
        .worded("diag_omarchy_updates_ok_summary", None, None)
}

/// Hyprland and Omarchy have no owning-crate probe, so these two rows shell out. Other catalog
/// checks must not grow a third.
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = pf_host_config::session::export_to(Command::new(program).args(args))
        .output()
        .ok()?;
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
            let (key, why) = takeover_inapplicable_reason(why);
            HostCheck::inapplicable(id, why).worded(key, None, None)
        }
        TakeoverVerdict::Ok { user, group } => {
            HostCheck::ok(id, format!("User “{user}” is in the “{group}” group."))
                .with_param("user", user)
                .with_param("group", group)
                .worded("diag_takeover_privilege_ok_summary", None, None)
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
                "Streams that need the managed takeover can't stop {dm}, so every one of them \
                 degrades to mirroring this machine's own session instead. With the panel off that \
                 looks like a black screen on every connect, and nothing else reports it."
            ),
        )
        .with_remedy(Remedy {
            text: format!(
                "Add the user to the “{group}” group, then restart the computer. The same group \
                 gates the USB/IP pads' nodes, which can present arbitrary emulated USB devices \
                 — join it only on a machine you trust."
            ),
            command: Some(format!("sudo usermod -aG {group} {user}")),
            // Helper reads the user database and is satisfied at once; this process keeps the
            // group set it started with. One restart covers both.
            relogin_required: true,
        })
        .with_param("user", user)
        .with_param("group", group)
        .with_param("dm", dm)
        .with_param("helper", helper)
        .worded(
            "diag_takeover_privilege_summary",
            Some("diag_takeover_privilege_impact"),
            Some("diag_takeover_privilege_remedy"),
        ),
    }
}

/// The message id and English of why the takeover does not apply.
fn takeover_inapplicable_reason(why: TakeoverInapplicable) -> (&'static str, &'static str) {
    match why {
        TakeoverInapplicable::Root => (
            "diag_takeover_privilege_na_root",
            "The host runs as root, so it stops the display manager directly and never needs the \
             privilege helper.",
        ),
        TakeoverInapplicable::NoDisplayManager => (
            "diag_takeover_privilege_na_no_dm",
            "No display manager drives this machine's logins, so a takeover has nothing to stop.",
        ),
        TakeoverInapplicable::NoManagedSession => (
            "diag_takeover_privilege_na_no_session",
            "This machine has no gamescope session infrastructure, so the managed takeover never \
             runs here.",
        ),
        TakeoverInapplicable::NoPackagedHelper => (
            "diag_takeover_privilege_na_unpackaged",
            "This is an unpackaged install: it has no privilege helper and no group, and uses the \
             polkit rule from the documentation instead.",
        ),
        TakeoverInapplicable::UnknownUser => (
            "diag_takeover_privilege_na_unknown_user",
            "The host's user name couldn't be resolved, so no group membership could be checked.",
        ),
        TakeoverInapplicable::NotLinux => (
            "diag_takeover_privilege_na_not_linux",
            "The managed gamescope takeover is a Linux feature.",
        ),
    }
}

/// What a player loses while USB/IP pads can't attach.
const USBIP_PAD_IMPACT: &str = "USB/IP pads can't attach. Steam Input then never sees a virtual \
     Steam Deck or Steam Controller, so nothing in Game Mode can be navigated with a pad, and \
     Switch 2 pads fall back to the Switch Pro.";

/// The same with the Steam and DualSense gates off: only a Switch 2 pad still asks for USB/IP.
const SWITCH2_ONLY_IMPACT: &str = "Switch 2 pads fall back to the Switch Pro. USB/IP is off for \
     Steam and DualSense pads, so nothing else changes.";

/// Whether a gate still asks for a USB/IP pad: the Steam gate (on by default) or the DualSense
/// row. A client can always ask for a Switch 2 pad, which degrades instead.
#[cfg(target_os = "linux")]
fn usbip_gate_open() -> bool {
    pf_inject::steam_usbip::usbip_preferred() || pf_inject::dualsense_usbip::usbip_preferred()
}

#[cfg(not(target_os = "linux"))]
fn usbip_gate_open() -> bool {
    false
}

fn virtual_deck_vhci() -> HostCheck {
    let id = ids::VIRTUAL_DECK_VHCI;
    let group = PUNKTFUNK_GROUP;
    match crate::inject::vhci_probe() {
        // pf-inject's only inapplicable verdict is a host that isn't Linux.
        VhciVerdict::Inapplicable { why } => HostCheck::inapplicable(id, why).worded(
            "diag_virtual_deck_vhci_na_not_linux",
            None,
            None,
        ),
        VhciVerdict::Ok => HostCheck::ok(id, "USB/IP pads can attach.").worded(
            "diag_virtual_deck_vhci_ok_summary",
            None,
            None,
        ),
        VhciVerdict::ModuleMissing => {
            // Both gates off leaves only the Switch 2 fallback: a note, not a warning.
            let (severity, impact, impact_id) = if usbip_gate_open() {
                (
                    Severity::Warning,
                    USBIP_PAD_IMPACT,
                    "diag_virtual_deck_vhci_pads_impact",
                )
            } else {
                (
                    Severity::Info,
                    SWITCH2_ONLY_IMPACT,
                    "diag_virtual_deck_vhci_switch2_impact",
                )
            };
            HostCheck::problem(
                id,
                CheckStatus::Fail,
                severity,
                "The vhci_hcd kernel module is not loaded",
                impact,
            )
            .with_remedy(Remedy {
                text: "Load the vhci_hcd module (the packages install a modules-load rule that \
                       does this at boot; on an unpackaged install, load it by hand)."
                    .to_string(),
                command: Some("sudo modprobe vhci-hcd".to_string()),
                relogin_required: false,
            })
            .worded(
                "diag_virtual_deck_vhci_no_module_summary",
                Some(impact_id),
                Some("diag_virtual_deck_vhci_no_module_remedy"),
            )
        }
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
    let pad_impact = USBIP_PAD_IMPACT;

    match (in_userdb, in_process) {
        (Some(true), Some(false)) => session_predates(
            id,
            group,
            &path,
            Severity::Warning,
            ("diag_virtual_deck_vhci_pads_impact", pad_impact),
        ),

        (Some(false), _) => {
            let user = user.unwrap_or_default();
            base(
                &format!("User “{user}” is not in the “{group}” group"),
                pad_impact,
            )
            .with_remedy(Remedy {
                text: format!(
                    "Add the user to the “{group}” group, then restart the computer. This group \
                     can present arbitrary emulated USB devices — join it only on a machine you \
                     trust."
                ),
                command: Some(format!("sudo usermod -aG {group} {user}")),
                relogin_required: true,
            })
            .with_param("user", user)
            .worded(
                "diag_virtual_deck_vhci_not_member_summary",
                Some("diag_virtual_deck_vhci_pads_impact"),
                Some("diag_virtual_deck_vhci_not_member_remedy"),
            )
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
        })
        .worded(
            "diag_virtual_deck_vhci_node_group_summary",
            Some("diag_virtual_deck_vhci_pads_impact"),
            Some("diag_virtual_deck_vhci_node_group_remedy"),
        ),

        // User database unreachable. Do not guess a cause; a wrong remedy costs more than a
        // vague one.
        _ => base("The USB/IP pads' attach node is not writable", pad_impact)
            .with_remedy(Remedy {
                text: format!(
                "Check that this machine's user is in the “{group}” group and that the udev rule \
                 granting it access to the vhci nodes is installed, then restart the computer."
            ),
                command: None,
                relogin_required: true,
            })
            .worded(
                "diag_virtual_deck_vhci_unwritable_summary",
                Some("diag_virtual_deck_vhci_pads_impact"),
                Some("diag_virtual_deck_vhci_unwritable_remedy"),
            ),
    }
}

/// Granted in the user database, not in this process: `systemd --user` keeps the group set it
/// started with, and under linger it outlives a log-out. Only a restart applies the grant.
fn session_predates(
    id: &str,
    group: &str,
    path: &str,
    severity: Severity,
    (impact_id, impact): (&str, &str),
) -> HostCheck {
    HostCheck::problem(
        id,
        CheckStatus::Fail,
        severity,
        format!("The group “{group}” was granted but this session predates it"),
        impact,
    )
    .with_remedy(Remedy {
        text: "Restart the computer. The membership is already recorded, but the host's services \
               started before it and keep their old groups until a restart — logging out is not \
               enough."
            .to_string(),
        command: None,
        relogin_required: true,
    })
    .with_param("group", group)
    .with_param("path", path)
    .with_param("user", current_user().unwrap_or_default())
    .worded(
        "diag_group_predates_summary",
        Some(impact_id),
        Some("diag_group_predates_remedy"),
    )
}

fn uinput_access() -> HostCheck {
    let id = ids::UINPUT_ACCESS;
    match crate::inject::uinput_probe() {
        UinputVerdict::Inapplicable => HostCheck::inapplicable(
            id,
            "Virtual controllers are created through this platform's own driver stack rather than \
             uinput.",
        )
        .worded("diag_uinput_access_na_platform", None, None),
        UinputVerdict::Ok => HostCheck::ok(id, "The input device nodes are reachable.").worded(
            "diag_uinput_access_ok_summary",
            None,
            None,
        ),

        // Node exists, open denied. Already granted → restart; else the remedy depends on
        // whether this OS persists `usermod`.
        UinputVerdict::PermissionDenied { path } => {
            let impact = "Every virtual controller fails to be created, so games see no gamepad \
                          at all — and the pen and tablet input paths are dead with it.";
            let in_userdb = current_user()
                .as_deref()
                .and_then(|u| user_in_group_userdb(u, INPUT_GROUP));
            if in_userdb == Some(true) && process_in_group(INPUT_GROUP) == Some(false) {
                let impact = ("diag_uinput_access_denied_impact", impact);
                return session_predates(id, INPUT_GROUP, path, Severity::Critical, impact);
            }
            let (remedy_id, remedy) = input_group_remedy();
            HostCheck::problem(
                id,
                CheckStatus::Fail,
                Severity::Critical,
                format!("No permission to open {path}"),
                impact,
            )
            .with_remedy(remedy)
            .with_param("path", path)
            .with_param("group", INPUT_GROUP)
            .worded(
                "diag_uinput_access_denied_summary",
                Some("diag_uinput_access_denied_impact"),
                Some(remedy_id),
            )
        }

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
        .with_param("path", path)
        .worded(
            "diag_uinput_access_missing_summary",
            Some("diag_uinput_access_missing_impact"),
            Some("diag_uinput_access_missing_remedy"),
        ),

        UinputVerdict::Error { path, message } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("{path} couldn't be opened: {message}"),
            "Virtual controllers may fail to be created, so games can see no gamepad.".to_string(),
        )
        .with_remedy(Remedy {
            text: format!("Check the state of {path} on this machine."),
            command: None,
            relogin_required: false,
        })
        .with_param("path", path)
        .with_param("error", message)
        .worded(
            "diag_uinput_access_error_summary",
            Some("diag_uinput_access_error_impact"),
            Some("diag_uinput_access_error_remedy"),
        ),
    }
}

/// On Universal Blue, `usermod -aG input` looks successful and is reverted: `/etc/group` is not
/// writable state. `packaging/README.md`. Returns the remedy's message id beside it.
fn input_group_remedy() -> (&'static str, Remedy) {
    let user = current_user().unwrap_or_else(|| "$USER".to_string());
    if is_universal_blue() {
        let remedy = Remedy {
            text:
                "Add the user to the “input” group with ujust, then restart the computer. On this \
                   OS a plain `usermod` does not persist."
                    .to_string(),
            command: Some("ujust add-user-to-input-group".to_string()),
            relogin_required: true,
        };
        ("diag_uinput_access_ublue_remedy", remedy)
    } else {
        let remedy = Remedy {
            text: "Add the user to the “input” group, then restart the computer. If the group \
                   already lists the user, the udev rule granting it access may be missing \
                   (scripts/60-punktfunk.rules)."
                .to_string(),
            command: Some(format!("sudo usermod -aG {INPUT_GROUP} {user}")),
            relogin_required: true,
        };
        ("diag_uinput_access_remedy", remedy)
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
        )
        .worded("diag_server_conflict_ok_summary", None, None);
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
    .worded(
        "diag_server_conflict_summary",
        Some("diag_server_conflict_impact"),
        Some("diag_server_conflict_remedy"),
    )
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
/// start; a fresh `usermod` is invisible until that manager restarts.
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

    /// A door probes only the box's own checks; every other row says why it doesn't apply.
    #[test]
    fn the_door_probes_only_the_box_s_checks() {
        let reg = Diagnostics::new();
        register_all(&reg, true);
        reg.run_all(super::super::CheckSource::Startup);
        let probed = [
            ids::SERVER_CONFLICT,
            ids::OMARCHY_UPDATES,
            ids::RESTART_PENDING,
        ];
        for check in reg.report().checks {
            if probed.contains(&check.id.as_str()) {
                continue;
            }
            assert_eq!(check.status, CheckStatus::Inapplicable, "{}", check.id);
            assert!(check.remedy.is_none(), "{}", check.id);
        }
    }

    /// The doctor must probe the sandbox the runner actually builds.
    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_probe_matches_the_runner() {
        let ts = include_str!("../../../../../sdk/src/sandbox.ts");
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

    /// Stock niri fails loudly, unless a pinned monitor keeps the host streaming. Either way the
    /// remedy names the package and the page.
    #[cfg(target_os = "linux")]
    #[test]
    fn stock_niri_fails_unless_a_monitor_is_pinned() {
        for (pinned, status, severity) in [
            (false, CheckStatus::Fail, Severity::Critical),
            (true, CheckStatus::Warn, Severity::Warning),
        ] {
            let stock = anyhow::Error::new(crate::vdisplay::NoVirtualOutputs);
            let check = niri_verdict(Err(stock), pinned);
            assert_eq!((check.status, check.severity), (status, severity));
            let remedy = check.remedy.expect("a stock niri row carries a remedy");
            assert!(remedy.text.contains("biri-git") && remedy.text.contains(BIRI_DOCS));
            assert!(!check.impact.is_empty());
        }
    }

    #[test]
    fn niri_that_answers_is_ok_and_one_that_does_not_only_warns() {
        let ok = niri_verdict(Ok(()), false);
        assert_eq!(ok.status, CheckStatus::Ok);

        let silent = niri_verdict(Err(anyhow::anyhow!("no socket")), false);
        assert_eq!(silent.status, CheckStatus::Warn);
        assert!(!silent.impact.is_empty());
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

    /// Already granted: re-running `usermod` changes nothing, so the remedy is a restart only.
    #[test]
    fn session_predates_asks_for_a_restart_not_usermod() {
        let check = session_predates(
            ids::UINPUT_ACCESS,
            INPUT_GROUP,
            "/dev/uhid",
            Severity::Critical,
            ("diag_uinput_access_denied_impact", "impact"),
        );
        let remedy = check.remedy.expect("a predates row carries a remedy");
        assert!(remedy.command.is_none());
        assert!(remedy.relogin_required);
        assert!(remedy.text.starts_with("Restart the computer"));
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
                !takeover_inapplicable_reason(why).1.trim().is_empty(),
                "{why:?} needs a reason — an inapplicable row exists to answer \"why not here?\""
            );
        }
    }

    /// Atomic-OS branch: `usermod` looks successful on Bazzite and is gone after reboot.
    #[test]
    fn input_remedy_matches_this_box_flavour() {
        let (_, remedy) = input_group_remedy();
        let command = remedy
            .command
            .expect("the input remedy is always pasteable");
        assert!(remedy.relogin_required, "a group change needs a restart");
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

    /// Installed but idle until the first connect is healthy; no adapter at all is not.
    #[test]
    fn vdisplay_driver_installed_is_ok_absent_fails() {
        let check = vdisplay_driver_check(DriverHealth::Installed);
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.remedy.is_none());
        let check = vdisplay_driver_check(DriverHealth::Absent);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.remedy.is_some());
    }

    #[test]
    fn server_conflict_is_ok_when_nothing_was_detected() {
        // Test binaries never ran a startup scan; empty matches a clean box.
        let check = server_conflict();
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.remedy.is_none());
    }
}
