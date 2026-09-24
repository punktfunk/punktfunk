//! Install-time `driver install|uninstall` and `web setup`: the installer's plan spawns this
//! EXE rather than a `.ps1` file.
//!
//! PowerShell 5.1 reads a `.ps1` *file* in the machine ANSI codepage; a non-ASCII byte on a
//! non-English locale aborts as "unterminated string". A compiled subcommand has no such
//! surface: `certutil`/`pnputil`/`nefconc`/`schtasks`/`netsh`/`icacls` are string literals.
//! Same pattern as `service install` in `service.rs`.
//!
//! Best-effort: a hiccup warns but returns `Ok`, since a non-zero exit aborts the installer
//! mid-plan. `driver check`, the plan's last step, is what fails an install whose pf-vdisplay
//! did not load.

use crate::vdisplay::DriverHealth;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn flag_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
fn flag_present(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
/// `%SystemRoot%\System32\<rel>` — the one place the System32 rule lives.
///
/// `CreateProcess` searches the calling process's directory and the working directory before
/// `%PATH%`, and everything routed through here runs elevated or as SYSTEM: a `certutil.exe`
/// planted beside the installer would otherwise win. `SystemRoot`, then `WINDIR`, then the
/// literal default — never a bare name, which is the PATH search this exists to avoid.
///
/// `rel` may carry a subdirectory, as PowerShell does.
pub(crate) fn sys32(rel: &str) -> String {
    let root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| r"C:\Windows".to_string());
    format!(r"{root}\System32\{rel}")
}

/// [`sys32`] for a bare tool name. A name that already carries a separator (the staged
/// `nefconc.exe`) is its own path and passes through.
pub(crate) fn resolve_tool(cmd: &str) -> String {
    if cmd.contains('\\') || cmd.contains('/') {
        return cmd.to_string();
    }
    sys32(&format!("{cmd}.exe"))
}
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    run_code(cmd, args) == Some(0)
}
/// Exit code, output discarded. `None` when the tool did not launch.
fn run_code(cmd: &str, args: &[&str]) -> Option<i32> {
    Command::new(resolve_tool(cmd))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
}
fn run_capture(cmd: &str, args: &[&str]) -> String {
    Command::new(resolve_tool(cmd))
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

pub fn driver_main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => driver_install(&args[1..]),
        Some("uninstall") => driver_uninstall(&args[1..]),
        Some("check") => driver_check(),
        _ => bail!(
            "usage: punktfunk-host driver install --dir <stage> [--gamepad]\n\
             \x20      punktfunk-host driver uninstall [--gamepad|--audio]\n\
             \x20      punktfunk-host driver check"
        ),
    }
}

fn driver_install(args: &[String]) -> Result<()> {
    let dir =
        PathBuf::from(flag_val(args, "--dir").context("driver install: --dir <stage> required")?);
    // `--dir` is code and trust (Root cert, nefconc, INF). A stage a non-admin can write is LPE.
    ensure_admin_only_source(&dir).with_context(|| {
        format!(
            "refusing to install drivers from {} — the staging directory must be writable only by \
             SYSTEM/Administrators",
            dir.display()
        )
    })?;
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", install_gamepad(&dir))
    } else {
        ("pf-vdisplay", install_pf_vdisplay(&dir))
    };
    if let Err(e) = res {
        eprintln!("warning: {what} driver install: {e:#} (the host degrades without it)");
    }
    Ok(())
}
/// The owner and DACL checks, shared with the update path and the setup engine.
pub(crate) use pf_paths_win::ensure_admin_only_source;
use pf_paths_win::quarantine_planted_secret;

/// Subject CN both signing certs carry. `certutil` matches CertId on subject, not a localized name.
const DRIVER_CERT_CN: &str = "punktfunk-driver";

/// Delete every `CN=punktfunk-driver` cert from machine `Root` and `TrustedPublisher`.
///
/// Match on subject, not thumbprint, so one run collects every historical cert under this CN.
/// Deleting the root does not unload an installed driver: PnP validates the signature when the
/// package is staged, not on every load. Safe to run before re-adding the current cert.
///
/// `certutil -delstore` deletes one match per call and fails when none remain. Bound the loop
/// so a pathological store cannot hang uninstall.
fn purge_driver_certs() {
    for store in ["Root", "TrustedPublisher"] {
        let mut removed = 0;
        while removed < 64 && run_quiet("certutil", &["-delstore", store, DRIVER_CERT_CN]) {
            removed += 1;
        }
        if removed > 0 {
            println!("removed {removed} stale '{DRIVER_CERT_CN}' cert(s) from {store}");
        }
    }
}

/// Machine `Root` (chain validates) and `TrustedPublisher` (PnP installs without a prompt).
fn trust_cert(dir: &Path) {
    match first_with_ext(dir, "cer") {
        Some(cer) => {
            let cer = cer.to_string_lossy().into_owned();
            for store in ["Root", "TrustedPublisher"] {
                if !run_quiet("certutil", &["-addstore", "-f", store, &cer]) {
                    eprintln!("warning: certutil -addstore {store} failed for {cer}");
                }
            }
            println!("trusted driver cert {cer} (Root + TrustedPublisher)");
        }
        None => eprintln!(
            "warning: no .cer in {} - driver may not install silently",
            dir.display()
        ),
    }
}

fn install_pf_vdisplay(dir: &Path) -> Result<()> {
    let inf = dir.join("pf_vdisplay.inf");
    if !inf.exists() {
        bail!("no pf_vdisplay.inf in {}", dir.display());
    }
    // Purge here only, not in `install_gamepad`. The installer runs vdisplay then gamepad; a
    // second purge would delete the cert the vdisplay leg just added when the two bundles differ.
    purge_driver_certs();
    trust_cert(dir);
    ensure_pf_vdisplay_node(dir, &inf);
    add_pf_vdisplay(&inf);
    // The wait also lets a departing monitor release the adapter before the restart.
    if wait_for_driver(DRIVER_SETTLE).is_ok() {
        return Ok(());
    }
    restart_pf_vdisplay();
    if wait_for_driver(DRIVER_SETTLE).is_ok() {
        return Ok(());
    }
    // What a manual reinstall does: unbind every console package, then bind ours. The needle
    // spares the seats package, which ships `pf_vdisplay_seats.cat`.
    delete_store_drivers(&["catalogfile=pf_vdisplay.cat"]);
    ensure_pf_vdisplay_node(dir, &inf);
    add_pf_vdisplay(&inf);
    match wait_for_driver(DRIVER_SETTLE) {
        Ok(_) => Ok(()),
        Err(h) => bail!("pf-vdisplay still not loaded after a clean reinstall ({h:?})"),
    }
}

/// Create the ROOT node only if absent: a re-create is a phantom duplicate, and the host binds
/// index 0. nefconc (ROOT\DISPLAY), never devgen (SWD\DEVGEN nodes survive reboot + registry delete).
fn ensure_pf_vdisplay_node(dir: &Path, inf: &Path) {
    if pf_vdisplay_present() {
        println!("pf-vdisplay device node already present - leaving it.");
    } else if let Some(nef) = first_named(dir, "nefconc.exe") {
        let (class, guid) = inf_class(inf);
        let ok = run_quiet(
            &nef.to_string_lossy(),
            &[
                "--create-device-node",
                "--hardware-id",
                "root\\pf_vdisplay",
                "--class-name",
                &class,
                "--class-guid",
                &guid,
            ],
        );
        if ok {
            println!("created root\\pf_vdisplay device node (nefconc)");
        } else {
            eprintln!("warning: nefconc --create-device-node failed");
        }
    } else {
        eprintln!(
            "warning: nefconc.exe not found in {} - cannot create the device node",
            dir.display()
        );
    }
}

/// Stage pf_vdisplay.inf and bind it to matching devices. Exit 3010 means the device kept its
/// loaded driver; the caller's [`wait_for_driver`] catches that.
fn add_pf_vdisplay(inf: &Path) {
    match run_code(
        "pnputil",
        &["/add-driver", &inf.to_string_lossy(), "/install"],
    ) {
        Some(0) => println!("pnputil /add-driver pf_vdisplay.inf /install ok"),
        Some(code) => {
            eprintln!("warning: pnputil /add-driver pf_vdisplay.inf /install exited {code}")
        }
        None => eprintln!("warning: pnputil did not launch"),
    }
}

/// Restart the console node so a staged driver loads. `ROOT\` only: a seat adapter is a live
/// RDP session's display.
fn restart_pf_vdisplay() {
    for id in pf_vdisplay_instance_ids() {
        if !id.to_ascii_uppercase().starts_with("ROOT\\") {
            continue;
        }
        if run_quiet("pnputil", &["/restart-device", &id]) {
            println!("restarted {id}");
        } else {
            eprintln!("warning: pnputil /restart-device {id} failed");
        }
    }
}

/// One rung's budget: a fresh node registers its interface within a few seconds.
const DRIVER_SETTLE: Duration = Duration::from_secs(15);

/// Poll until pf-vdisplay answers at this host's protocol (`Ok(protocol)`), or until `budget`
/// passes (`Err(last reading)`).
fn wait_for_driver(budget: Duration) -> Result<u32, DriverHealth> {
    let deadline = Instant::now() + budget;
    loop {
        match crate::vdisplay::driver_health() {
            DriverHealth::Ok { protocol } => return Ok(protocol),
            h if Instant::now() >= deadline => return Err(h),
            _ => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// The installer's last step. Exits 1 unless pf-vdisplay answers at this host's protocol; the
/// last stderr line is the failure setup shows.
fn driver_check() -> Result<()> {
    match wait_for_driver(DRIVER_SETTLE) {
        Ok(protocol) => {
            println!("pf-vdisplay answers protocol {protocol}");
            Ok(())
        }
        Err(h) => {
            eprintln!("pf-vdisplay driver check: {h:?}");
            eprintln!(
                "The virtual display driver didn't update, so streams can't start. Restart \
                 Windows, then run the installer again."
            );
            std::process::exit(1)
        }
    }
}

/// Stage the pad drivers, then verify the next pad can only bind them: the staged build in the
/// store, no other build of the same package, no pad devnode left to revive an old one. One
/// retry, then an error naming what is still wrong.
fn install_gamepad(dir: &Path) -> Result<()> {
    let infs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")))
        .collect();
    if infs.is_empty() {
        bail!("no driver .inf in {}", dir.display());
    }
    let staged: Vec<(String, String)> = infs
        .iter()
        .filter_map(|inf| inf_identity(&read_inf_text(inf)))
        .collect();
    trust_cert(dir);
    // Hardware IDs did not move when `pf-dualsense` became `pf-gamepad`. Match the old INF on
    // `pf_dualsense.dll` — matching those IDs would also delete the package we are about to add.
    delete_store_drivers(&["pf_dualsense.dll"]);
    let mut problems = Vec::new();
    for attempt in 0..2 {
        if attempt > 0 {
            eprintln!(
                "warning: gamepad drivers not settled ({}); retrying",
                problems.join("; ")
            );
        }
        // No `/install`, no device node: the host SwDeviceCreate's the per-session devnode when
        // a client forwards a pad, so PnP binds the store driver on demand.
        for inf in &infs {
            if run_quiet("pnputil", &["/add-driver", &inf.to_string_lossy()]) {
                println!("pnputil /add-driver {} ok", file_name(inf));
            } else {
                eprintln!("warning: pnputil /add-driver {} failed", inf.display());
            }
        }
        // An older build left in the store outranks nothing, but a newer-dated one would win.
        for (name, catalog, ver) in store_packages() {
            if staged.iter().any(|(c, v)| *c == catalog && *v != ver) {
                delete_store_driver(&name);
            }
        }
        // Phantoms too: SwDeviceCreate with a known instance id revives the bound driver and
        // never re-ranks the store. Per-session objects; the next pad binds the fresh package.
        remove_pad_devnodes();
        let store: Vec<(String, String)> = store_packages()
            .into_iter()
            .map(|(_, c, v)| (c, v))
            .collect();
        problems = pad_install_problems(&staged, &store, &pad_instance_ids());
        if problems.is_empty() {
            return Ok(());
        }
    }
    bail!("gamepad drivers did not settle: {}", problems.join("; "))
}

/// `(catalog, DriverVer)` of an INF, lowercased and space-free: which package it is and which
/// build. `None` for text without both lines.
fn inf_identity(text: &str) -> Option<(String, String)> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let line = line.split(';').next()?;
            let (k, v) = line.split_once('=')?;
            k.trim().eq_ignore_ascii_case(key).then(|| {
                v.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
                    .to_ascii_lowercase()
            })
        })
    };
    Some((value("CatalogFile")?, value("DriverVer")?))
}

/// What stops the next pad from binding the staged drivers: a staged build missing from the
/// store, another build of the same package still there, or a pad devnode that would revive the
/// driver it last bound. `store` is every `(catalog, DriverVer)` in the driver store.
fn pad_install_problems(
    staged: &[(String, String)],
    store: &[(String, String)],
    devnodes: &[String],
) -> Vec<String> {
    let mut problems = Vec::new();
    for (catalog, ver) in staged {
        if !store.iter().any(|s| s.0 == *catalog && s.1 == *ver) {
            problems.push(format!("{catalog} {ver} is not in the driver store"));
        }
        for (_, old) in store.iter().filter(|s| s.0 == *catalog && s.1 != *ver) {
            problems.push(format!("{catalog} {old} is still in the driver store"));
        }
    }
    problems.extend(
        devnodes
            .iter()
            .map(|id| format!("pad devnode {id} is still present")),
    );
    problems
}

fn remove_pad_devnodes() {
    for id in pad_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed stale pad devnode {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
}

fn driver_uninstall(args: &[String]) -> Result<()> {
    // Audio removes host-minted devnodes on Valve's drivers; return before the cert purge so a
    // `--audio` uninstall does not run that purge a third time.
    if flag_present(args, "--audio") {
        return uninstall_audio_devices();
    }
    let gamepad = flag_present(args, "--gamepad");
    let (what, res) = if gamepad {
        ("gamepad", uninstall_gamepad())
    } else {
        ("pf-vdisplay", uninstall_pf_vdisplay())
    };
    if let Err(e) = res {
        eprintln!("warning: {what} driver uninstall: {e:#}");
    }
    // Once per invocation, here rather than in each uninstall body: the uninstaller calls both
    // legs back to back, and a leftover trusted root CA must not survive.
    purge_driver_certs();
    Ok(())
}

/// Host-minted "Punktfunk Speakers"/"Punktfunk Microphone" and per-pad DualSense speaker
/// endpoints. Run after `service uninstall`: a live host re-mints them on the next wiring pass.
///
/// Does not remove Steam's streaming-audio drivers. Marker-matched in `audio::devnode_cleanup`.
fn uninstall_audio_devices() -> Result<()> {
    match crate::audio::devnode_cleanup::purge() {
        Ok(r) if r.devnodes == 0 && r.devnode_failures == 0 => {
            println!("no punktfunk audio devices to remove")
        }
        Ok(r) => {
            println!(
                "removed {} punktfunk audio device(s), {} endpoint record(s)",
                r.devnodes, r.endpoint_records
            );
            if r.devnode_failures > 0 {
                eprintln!(
                    "warning: {} punktfunk audio device(s) could not be removed — they can be \
                     deleted from Device Manager (View ▸ Show hidden devices)",
                    r.devnode_failures
                );
            }
        }
        Err(e) => eprintln!("warning: audio device cleanup: {e:#}"),
    }
    Ok(())
}

fn uninstall_pf_vdisplay() -> Result<()> {
    // ROOT nodes first; leaving them is a ghost "punktfunk virtual display" in Device Manager.
    for id in pf_vdisplay_instance_ids() {
        if run_quiet("pnputil", &["/remove-device", &id]) {
            println!("removed device node {id}");
        } else {
            eprintln!("warning: pnputil /remove-device {id} failed");
        }
    }
    delete_store_drivers(&["pf_vdisplay"]);
    Ok(())
}

fn uninstall_gamepad() -> Result<()> {
    // Devnodes (incl. phantoms) before store packages.
    remove_pad_devnodes();
    delete_store_drivers(&[
        "pf_gamepad",
        "pf_dualsense",
        "pf_dualshock4",
        "pf_xusb",
        "pf_mouse",
    ]);
    Ok(())
}

/// Instance IDs of enumerated pf-vdisplay devices. Blank-line blocks from `pnputil /enum-devices`;
/// ours if the block mentions the hardware id / description. The instance ID is the first line's
/// VALUE (never the localized "Instance ID:" label — pnputil prints that first in every block).
fn pf_vdisplay_instance_ids() -> Vec<String> {
    let out = run_capture("pnputil", &["/enum-devices", "/class", "Display"]);
    let mut ids = Vec::new();
    for block in out.split("\r\n\r\n").flat_map(|b| b.split("\n\n")) {
        let lo = block.to_ascii_lowercase();
        if !lo.contains("pf_vdisplay") && !lo.contains("punktfunk virtual display") {
            continue;
        }
        let Some(first) = block.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some((_, value)) = first.split_once(':') else {
            continue;
        };
        let id = value.trim();
        // Instance IDs are backslashed paths with no spaces (`ROOT\DISPLAY\0000`).
        if !id.is_empty() && id.contains('\\') && !id.contains(' ') {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Pad instance IDs (`SWD\PUNKTFUNK\…`), including phantoms (`/enum-devices` lists disconnected
/// nodes). Same VALUE-side parse as [`pf_vdisplay_instance_ids`]. No `/class`: HIDClass + System.
fn pad_instance_ids() -> Vec<String> {
    let out = run_capture("pnputil", &["/enum-devices"]);
    let mut ids = Vec::new();
    for block in out.split("\r\n\r\n").flat_map(|b| b.split("\n\n")) {
        let Some(first) = block.lines().find(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some((_, value)) = first.split_once(':') else {
            continue;
        };
        let id = value.trim();
        // Pads enumerate under their USB interface id (`SWD\VID_…&MI_03\PF_…`), the rest
        // under `punktfunk`.
        let up = id.to_ascii_uppercase();
        let ours = up.starts_with("SWD\\PUNKTFUNK\\")
            || (up.starts_with("SWD\\VID_") && up.contains("\\PF_"));
        if ours && !id.contains(' ') {
            ids.push(id.to_string());
        }
    }
    ids
}

/// `(file name, text)` of every `%WINDIR%\INF\oem*.inf` — the driver store's packages, read
/// as content rather than through `pnputil /enum-drivers` (localized).
fn store_infs() -> Vec<(String, String)> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let inf_dir = Path::new(&windir).join("INF");
    let Ok(entries) = std::fs::read_dir(&inf_dir) else {
        eprintln!("warning: {} is unreadable", inf_dir.display());
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|path| {
            let name = file_name(&path).to_ascii_lowercase();
            (name.starts_with("oem") && name.ends_with(".inf"))
                .then(|| (name, read_inf_text(&path)))
        })
        .collect()
}

/// `(file name, catalog, DriverVer)` of each store package ([`inf_identity`]).
fn store_packages() -> Vec<(String, String, String)> {
    store_infs()
        .into_iter()
        .filter_map(|(name, text)| inf_identity(&text).map(|(c, v)| (name, c, v)))
        .collect()
}

/// Delete each store package whose text mentions a needle. `/uninstall /force` also unbinds
/// remaining devnodes.
fn delete_store_drivers(needles: &[&str]) {
    for (name, text) in store_infs() {
        let text = text.to_ascii_lowercase();
        if needles.iter().any(|n| text.contains(n)) {
            delete_store_driver(&name);
        }
    }
}

fn delete_store_driver(name: &str) {
    if run_quiet("pnputil", &["/delete-driver", name, "/uninstall", "/force"]) {
        println!("deleted driver package {name}");
    } else {
        eprintln!("warning: pnputil /delete-driver {name} /uninstall /force failed");
    }
}

/// `%WINDIR%\INF` is ANSI or UTF-16LE(+BOM); decode either so the needle match works.
fn read_inf_text(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Enumerated AND connected. Without `/connected` a phantom from an earlier uninstall satisfies
/// this, install skips the live ROOT node, and the host (present devices only) reports no driver.
/// Match device ID / description, not a localized label.
fn pf_vdisplay_present() -> bool {
    let lo = run_capture(
        "pnputil",
        &["/enum-devices", "/connected", "/class", "Display"],
    )
    .to_ascii_lowercase();
    lo.contains("pf_vdisplay") || lo.contains("punktfunk virtual display")
}

/// INF `Class` + `ClassGuid` so the node matches the shipped driver; fallback is Display.
fn inf_class(inf: &Path) -> (String, String) {
    let text = std::fs::read_to_string(inf).unwrap_or_default();
    let (mut class, mut guid) = (None, None);
    for line in text.lines() {
        let t = line.trim();
        if let Some(eq) = t.find('=') {
            let key = t[..eq].trim().to_ascii_lowercase();
            let val = t[eq + 1..]
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            match key.as_str() {
                "class" => class = Some(val),
                "classguid" => guid = Some(val),
                _ => {}
            }
        }
    }
    (
        class
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "Display".into()),
        guid.filter(|g| !g.is_empty())
            .unwrap_or_else(|| "{4d36e968-e325-11ce-bfc1-08002be10318}".into()),
    )
}

// Install-time provisioning only: login password, firewall, and delete of the legacy
// `PunktfunkWeb` scheduled task (a live one races the service's console child for :47992).
// The service supervises bun; see `service.rs` and design/windows-web-console-lifecycle.md.

/// Retired scheduled-task name; referenced only to delete it from older installs.
const WEB_TASK: &str = "PunktfunkWeb";

pub fn web_main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("setup") => web_setup(&args[1..]),
        Some("password") => web_password(),
        _ => bail!(
            "usage: punktfunk-host web setup --app-dir <app> [--password-file <file>]\n       punktfunk-host web password"
        ),
    }
}

/// Print the console login password, the one affordance a silent install leaves.
///
/// Readable until the first sign-in only: the console then replaces the clear-text line with a
/// salted hash, and the way back in is to write a new password into the file, not to read one out.
/// The file is ACL'd to Administrators + SYSTEM, so a non-elevated read fails on permission rather
/// than absence, and the two need different next moves.
fn web_password() -> Result<()> {
    let path = pf_paths::config_dir().join("web-password");
    let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!(
            "Couldn't read the console password, which only Administrators may see. Run this from an elevated PowerShell."
        ),
        std::io::ErrorKind::NotFound => anyhow::anyhow!(
            "Couldn't find the console password. Run this from an elevated PowerShell — a normal one can't see the file even when it is there."
        ),
        _ => anyhow::anyhow!("Couldn't read the console password — {e}"),
    })?;
    let mut hashed = false;
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match (key.trim(), value.trim()) {
            ("PUNKTFUNK_UI_PASSWORD", pw) if !pw.is_empty() => {
                println!("{pw}");
                return Ok(());
            }
            ("PUNKTFUNK_UI_PASSWORD_HASH", h) if !h.is_empty() => hashed = true,
            _ => {}
        }
    }
    bail!(
        "{} Set a new one: put a PUNKTFUNK_UI_PASSWORD=<your-password> line in {}, then run `punktfunk-host service restart`.",
        if hashed {
            "The console password is stored as a salted hash, so it can't be read back."
        } else {
            "The console password file carries no password, so the console admits nobody."
        },
        path.display()
    )
}

fn web_setup(args: &[String]) -> Result<()> {
    let app_dir =
        PathBuf::from(flag_val(args, "--app-dir").context("web setup: --app-dir <app> required")?);
    let pw_file = flag_val(args, "--password-file");
    let data_dir = pf_paths::config_dir();
    let pw_path = data_dir.join("web-password");
    // Before the hardening, not after: `create_private_dir` re-owns the contents on its first
    // pass, which would make a planted password look administrator-owned and keep it.
    quarantine_planted_secret(&pw_path);
    // `create_private_dir`, not `create_dir_all`: the next line writes the console password, and
    // `create_dir_all` would inherit `%ProgramData%` (BUILTIN\Users can create files). The error
    // is the junction refusal, and `write_secret_file` only rejects the FILE being a link — a
    // junctioned parent still redirects the write, so refuse rather than continue.
    pf_paths::create_private_dir(&data_dir).with_context(|| {
        format!(
            "lock down {} before writing the console password",
            data_dir.display()
        )
    })?;

    set_web_password(&pw_path, pw_file.as_deref());
    // End + delete the legacy task (idempotent if absent). The installer disables it before the
    // file copy so it cannot respawn between service start and this delete.
    run_quiet("schtasks", &["/end", "/tn", WEB_TASK]);
    run_quiet("schtasks", &["/delete", "/tn", WEB_TASK, "/f"]);
    // Informational: the supervisor waits on its own; install time is when a human is watching.
    let server = app_dir
        .join("web")
        .join(".output")
        .join("server")
        .join("index.mjs");
    if !server.exists() {
        eprintln!(
            "warning: web console payload missing at {} - the service will not serve a console",
            server.display()
        );
    }
    // TCP 47992 (console) and 47993 (plugin UIs — separate origin so a plugin cannot act as the
    // operator). Delete any prior rule first so an upgrade re-scopes instead of stacking. Bind
    // to `<app>/bun/bun.exe`; a port-only allow admits whoever binds first. No UDP: browsers
    // will not QUIC a self-signed/no-SAN cert. Missing bun.exe falls back to port-only.
    let fw_profile =
        crate::service::firewall_profile_arg(crate::service::allow_public_network(args)?);
    let bun = app_dir.join("bun").join("bun.exe");
    let program = bun.exists().then_some(bun.as_path());
    if program.is_none() {
        eprintln!(
            "warning: {} not found — the console firewall rules stay open to any program on those \
             ports instead of only the console",
            bun.display()
        );
    }
    for (name, port) in [
        ("Punktfunk web console (TCP 47992)", "47992"),
        ("Punktfunk plugin UIs (TCP 47993)", "47993"),
    ] {
        run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
        if !crate::service::run_netsh(&crate::service::fw_add_rule_args(
            name,
            "TCP",
            Some(port),
            program,
            fw_profile,
        )) {
            eprintln!("warning: firewall rule for TCP {port} not added");
        }
    }
    println!(
        "web console set up (https://<host-ip>:47992; supervised by the PunktfunkHost service)"
    );
    Ok(())
}

/// Non-empty `--password-file` (fresh) > keep existing (upgrade) > random. Writes
/// `PUNKTFUNK_UI_PASSWORD=<pw>\n` (LF, no BOM) and ACLs it to Administrators + SYSTEM only.
/// The console replaces that line with a salted hash the first time the password signs in, so
/// an upgrade keeping the existing file keeps the hash.
fn set_web_password(pw_path: &Path, pw_file: Option<&str>) {
    // Non-admin owner means planted under `%ProgramData%` CREATOR OWNER before this install.
    // `FileExists` would treat it as an upgrade and keep the attacker's password. Rename aside;
    // `!planted` still rotates if the rename failed. An Administrators-owned file is kept.
    let planted = pf_paths_win::planted_by_non_admin(pw_path);
    if planted {
        let _ = pf_paths_win::rename_aside(pw_path);
        println!(
            "web console password file was owned by a non-admin (planted before install) — rotating to a fresh password"
        );
    }
    let password = pw_file
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if pw_path.exists() && !planted {
                println!("keeping existing web console password");
                None
            } else {
                Some(random_password())
            }
        });
    if let Some(pw) = password {
        // The shared writer, not a local copy of it: it refuses a reparse point (a junction
        // planted here would redirect this SYSTEM write), and on a failed DACL lock it removes
        // the file and reports the error instead of leaving the secret on the inherited,
        // Users-readable `%ProgramData%` ACL.
        if let Err(e) =
            pf_paths::write_secret_file(pw_path, format!("PUNKTFUNK_UI_PASSWORD={pw}\n").as_bytes())
        {
            eprintln!("warning: {} not written: {e}", pw_path.display());
        }
    }
}

/// 20 chars, URL/shell-safe (no `/ + =`).
fn random_password() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
    base64::engine::general_purpose::STANDARD
        .encode(b)
        .chars()
        .filter(|c| !matches!(c, '/' | '+' | '='))
        .take(20)
        .collect()
}

fn first_with_ext(dir: &Path, ext: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)))
}
fn first_named(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(name);
    p.exists().then_some(p)
}
fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The staged INF as stampinf writes it, and the same package after pnputil stores it.
    #[test]
    fn inf_identity_reads_the_stamped_package() {
        let inf = "[Version]\r\nSignature=\"$WINDOWS NT$\"\r\nClass=HIDClass\r\n\
                   CatalogFile=pf_gamepad.cat ; signed\r\n\
                   DriverVer = 09/24/2026,9.9.0924.1200\r\n";
        assert_eq!(
            inf_identity(inf),
            Some(("pf_gamepad.cat".into(), "09/24/2026,9.9.0924.1200".into()))
        );
        assert_eq!(
            inf_identity("CatalogFile=pf_gamepad.cat\n"),
            None,
            "unstamped"
        );
        assert_eq!(
            inf_identity(";DriverVer=1\nCatalogFile=x.cat\n"),
            None,
            "commented out"
        );
    }

    /// An older build of the package left in the store, a missing staged build, and a pad devnode
    /// that survived removal are each named; a clean store is not a problem.
    #[test]
    fn pad_install_problems_names_what_keeps_the_old_driver() {
        let gp = |v: &str| ("pf_gamepad.cat".to_string(), v.to_string());
        let staged = [gp("new")];
        assert!(pad_install_problems(&staged, &[gp("new")], &[]).is_empty());
        let stale = pad_install_problems(&staged, &[gp("new"), gp("old")], &[]);
        assert_eq!(stale, ["pf_gamepad.cat old is still in the driver store"]);
        let missing = pad_install_problems(&staged, &[gp("old")], &[]);
        assert_eq!(missing.len(), 2, "{missing:?}");
        let other = [("pf_xusb.cat".to_string(), "old".to_string()), gp("new")];
        assert!(
            pad_install_problems(&staged, &other, &[]).is_empty(),
            "other packages"
        );
        let live =
            pad_install_problems(&staged, &[gp("new")], &["SWD\\PUNKTFUNK\\PF_PAD_0".into()]);
        assert_eq!(
            live,
            ["pad devnode SWD\\PUNKTFUNK\\PF_PAD_0 is still present"]
        );
    }
}
