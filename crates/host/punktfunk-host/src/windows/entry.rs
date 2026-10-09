//! What `main` does differently on Windows, as one seam: the SCM `service run` logging,
//! the SEH crash hook, the pre-DXGI GPU preference, the `serve` startup recovery of a prior
//! host's display state, and the Windows-only subcommands. `main.rs` carries a no-op twin
//! for every other OS.

use anyhow::Result;
use pf_win_display::monitor_devnode;
use pf_win_display::win_display::isolate_journal;

/// `service run` is the SCM host: no console, so logging goes to a file.
pub(crate) fn service_run_requested() -> bool {
    let a: Vec<String> = std::env::args().skip(1).take(2).collect();
    a.first().map(String::as_str) == Some("service") && a.get(1).map(String::as_str) == Some("run")
}

pub(crate) fn init_file_logging(filter: tracing_subscriber::EnvFilter) {
    crate::service::init_file_logging(filter);
}

/// SEH last-resort: a GPU-runtime AV otherwise kills the process with no ring entry.
pub(crate) fn install_crash_handler() {
    punktfunk_core::crash::install();
}

/// Before DXGI: virtual-display setup creates a factory. Hybrid-GPU boxes otherwise reparent
/// the virtual output off the capture GPU (ACCESS_LOST). Idempotent Once.
pub(crate) fn preflight(management_cli: bool) {
    if !management_cli {
        crate::capture::dxgi::install_gpu_pref_hook();
    }
}

/// Undo what a prior host left on the display stack, before any session touches it.
pub(crate) fn serve_startup_recover() {
    // First-comer-wins: claim before any client, or an idle service loses the driver to a stray host.
    crate::vdisplay::manager::claim_instance_eagerly();
    // Re-enable PnP monitors a prior Exclusive session disabled and never restored.
    monitor_devnode::startup_recover();
    // Unpin AMD connector emulation a prior host locked. The pin outlives the process
    // (and can outlive a reboot).
    pf_win_display::adl_emul::startup_recover();
    // Re-light Exclusive CCD-isolate panels. After the devnode leg so re-enabled
    // monitors exist for the EXTEND preset (the snapshot was process memory).
    isolate_journal::startup_recover();
    // Turn NVIDIA Instant Replay back on if a prior host paused it and died.
    super::instant_replay::startup_recover();
    // The display actor (cached CCD snapshot) — up before the first session or management
    // read, so nothing else has to touch the display-config lock for inventory.
    pf_win_display::display_events::spawn_once();
}

/// The Windows-only subcommands. `args` starts at the subcommand. `None` = not one of ours.
pub(crate) fn subcommand(cmd: &str, args: &[String]) -> Option<Result<()>> {
    Some(match cmd {
        "hdr-p010-selftest" => hdr_p010_selftest(args),
        "deck-windows-spike" => super::devtest::deck_windows_spike(args),
        "vmouse-spike" => super::devtest::vmouse_spike(args),
        "channel-proof-probe" => super::devtest::channel_proof_probe(args),
        "dualsense-windows-test" => super::devtest::dualsense_windows_test(args),
        "pad-endpoint" => super::devtest::pad_endpoint(args),
        "audio-probe" => super::devtest::audio_probe(args),
        "service" => crate::service::main(&args[1..]),
        // Installer work in-process: locale-parsed PowerShell files break on ANSI codepages.
        "driver" => crate::install::driver_main(&args[1..]),
        "web" => crate::install::web_main(&args[1..]),
        // HKLM Run fires only at sign-in; this is how an upgrade/crash gets the icon back.
        "tray" => crate::tray::main(&args[1..]),
        _ => return None,
    })
}

/// `WxH` (default 64×64) and vendor. 1080 is not 16-aligned — a different driver path.
/// Dual-GPU boxes otherwise test the default adapter, not the encoder.
fn hdr_p010_selftest(args: &[String]) -> Result<()> {
    let mut size = (64u32, 64u32);
    let mut vendor = None;
    // `args` starts at the subcommand, so optionals begin at index 1.
    for a in args.iter().skip(1) {
        match a.as_str() {
            "intel" => vendor = Some(0x8086),
            "nvidia" => vendor = Some(0x10de),
            "amd" => vendor = Some(0x1002),
            s => {
                let parsed = s
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                match parsed {
                    Some(wh) => size = wh,
                    None => anyhow::bail!(
                        "hdr-p010-selftest: unrecognized arg {s:?} (want WxH or intel|nvidia|amd)"
                    ),
                }
            }
        }
    }
    crate::capture::dxgi::hdr_p010_selftest_at(size.0, size.1, vendor)
}

pub(crate) fn print_usage() {
    eprintln!(
        "\nWINDOWS SERVICE (end-user deployment — replaces a manual launch):\n\
        \x20   punktfunk-host service install    register an auto-start SYSTEM service + firewall rules\n\
        \x20   punktfunk-host service uninstall  remove the service + firewall rules\n\
        \x20   punktfunk-host service start|stop|restart|status\n\
        \x20   config: %ProgramData%\\punktfunk\\host.env\n\
        \nWINDOWS DIAGNOSTICS:\n\
        \x20   punktfunk-host hdr-p010-selftest  GPU colour check for the PUNKTFUNK_HDR_SHADER_P010 path\n\
        \x20                                     (scRGB FP16 -> P010 BT.2020 PQ shader vs an f64 reference)"
    );
}
