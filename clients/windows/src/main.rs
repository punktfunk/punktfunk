//! `punktfunk-client` — the native Windows punktfunk/1 client.
//!
//! Pure Rust: `NativeClient` linked as a crate (no C ABI, like the GTK Linux client) · SDL3
//! gamepads · a **WinUI 3** shell (windows-reactor). Streaming (decode + present + audio) runs in
//! the spawned `punktfunk-session` Vulkan binary; the shell owns host selection, trust and
//! pairing. The trust surface mirrors the
//! other native clients: persistent identity, trust-on-first-use, SPAKE2 PIN pairing — all in-app
//! (host list, settings, pairing). Streaming runs in the spawned `punktfunk-session` binary;
//! `--headless --speed-test` keeps a decode-less CLI measurement path.
//!
//! Usage:
//!   punktfunk-client                          (open the WinUI 3 window: host list, settings, pairing)
//!   punktfunk-client --discover               (list punktfunk hosts on the LAN)
//!   punktfunk-client --headless --speed-test --connect host[:port]
//!                    (measure the path: probe burst → goodput / loss / recommended bitrate)

// Unsafe-proof program: every `unsafe {}` in this client carries a `// SAFETY:` proof.
// Link as a GUI (windows) subsystem binary so the default windowed launch (MSIX / double-click)
// does NOT pop a console window. The CLI paths (--headless/--discover) reattach to the launching
// terminal's console at startup (see main), so their output is still visible when run from a shell.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
// The `--console` couch hand-off, shared verbatim with `punktfunk-console.exe`.
#[cfg(windows)]
mod couch;
// `punktfunk://` activation: single instance, hand-off, and the positional URL parse
// (design/client-deep-links.md §4.2).
#[cfg(windows)]
mod deeplink;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod logfile;
#[cfg(windows)]
#[cfg(windows)]
mod shell_window;
#[cfg(windows)]
mod spawn;
#[cfg(windows)]
mod trust;

#[cfg(windows)]
use pf_client_core::wol;

#[cfg(windows)]
fn main() {
    // With #![windows_subsystem = "windows"] the process starts with no console, so the GUI/MSIX
    // launch is window-free. AttachConsole only binds to an ALREADY-EXISTING parent console (it
    // never creates one), so when launched from a terminal — `--headless`/`--discover` — stdout and
    // the tracing writer below land in that terminal; from Explorer/MSIX it's a harmless no-op.
    // SAFETY: `AttachConsole` takes a plain process-id constant and binds to an already-existing
    // parent console; it allocates nothing and dereferences no caller memory, and failure (no parent
    // console) is ignored by design.
    unsafe {
        use windows::Win32::consoleapi::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    set_app_user_model_id();
    punktfunk_core::tls::install_default_provider();

    // Logs go to stderr, the client log file (a GUI launch has no console; ANSI off, users send
    // it) and the DEBUG+ ring "Send logs to host" uploads; the env filter scopes stderr and file
    // only. The session's stderr joins file and ring through `logring::forward_child_stderr`.
    logfile::init();
    pf_client_core::logring::init_tracing(logfile::tee, false);
    if let Some(p) = logfile::path() {
        tracing::info!(path = %p.display(), "client log file (rotated at 10 MB, one .old kept)");
    }
    // SEH last-resort: a native crash in the shell otherwise ends the log mid-sentence.
    punktfunk_core::crash::install();

    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);

    // A `punktfunk://` link — from protocol activation, `start`, or a shortcut. If a shell is
    // already running, hand it over and get out of the way: one window, and the link opens
    // where the user's hosts already are. A hand-off that finds nobody falls through and this
    // process becomes the shell that opens it, so the link is never simply lost.
    let link = deeplink::positional_url(&args);
    if let Some(url) = &link
        && !deeplink::claim_primary()
        && deeplink::forward_to_primary(url)
    {
        return;
    }

    if flag("--discover") {
        discover_and_print();
        return;
    }

    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            std::process::exit(1);
        }
    };

    if flag("--headless") {
        run_headless_cli(&args, identity);
        return;
    }

    // `--console`: go straight to the gamepad/couch UI, skipping the WinUI shell entirely —
    // the HTPC entry point (a Start-menu tile, a Steam shortcut, a startup item). The session
    // binary's bare `--browse` IS a complete standalone client: host list, discovery, PIN
    // pairing, settings and Wake-on-LAN, all controller-driven. We just exec it and mirror
    // its exit code, so anything supervising this process sees the real result.
    if flag("--console") {
        couch::run_browse();
    }

    // Windowed (default): the WinUI 3 app owns host selection, settings, and pairing.
    // Framework-dependent deployment: initialize the Windows App SDK runtime before any WinUI
    // call (build.rs stages the bootstrap DLL via windows-reactor-setup).
    if let Err(e) = windows_reactor::bootstrap() {
        tracing::error!(error = %e, "Windows App SDK bootstrap failed");
        std::process::exit(1);
    }
    // The shared SDL gamepad service (pf-client-core). The shell only enumerates pads (Settings
    // list) and persists the pin; the spawned punktfunk-session runs the SAME service and does the
    // actual forwarding — so, unlike the old shell fork, we never `attach()` here. Idle it stays
    // hands-off the hardware (id-getter metadata, no device open, the Deck HIDAPI driver off).
    let gamepad = pf_client_core::gamepad::GamepadService::start();
    // From here this process IS the shell: queue the link it was launched with (the router
    // picks it up once the window is live) and start listening for links from later instances.
    if let Some(url) = link {
        deeplink::queue(url);
    } else {
        // A plain launch holds the mutex too, or the next link finds no primary and opens
        // a second shell. A launched link already tried to claim it above.
        deeplink::claim_primary();
    }
    deeplink::install_receiver();
    let outcome = app::run(identity, gamepad);
    // Hand the single-instance mutex back before exiting, so the next launch claims it as
    // primary immediately rather than waiting for the kernel to notice this process is gone.
    deeplink::release_primary();
    if let Err(e) = outcome {
        tracing::error!(error = %e, "WinUI app failed");
        std::process::exit(1);
    }
}

/// Tag unpackaged (dev) runs with the explicit AppUserModelID that `pf-presenter`'s
/// session window also adopts, so the shell and the stream window group as ONE taskbar
/// app across the shell⇄session visibility handoff. MSIX runs already carry the package
/// identity — overriding it would detach the window from the Start-menu pin, so packaged
/// processes are left alone. Must run before any window exists.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::Win32::shobjidl_core::SetCurrentProcessExplicitAppUserModelID;
    if deeplink::has_package_identity() {
        return; // packaged (or indeterminate) — leave the identity alone
    }
    // SAFETY: `SetCurrentProcessExplicitAppUserModelID` takes a static wide literal.
    unsafe {
        // Must stay in sync with pf-presenter's win32.rs, or the windows stop grouping.
        let _ = SetCurrentProcessExplicitAppUserModelID(windows::core::w!("unom.punktfunk.client"));
    }
}

/// `--headless --speed-test --connect host[:port]`: measure the path over the real data plane and
/// print the outcome — the Windows analogue of `punktfunk-probe`. The former in-process
/// frame-count connect path went with the legacy builtin stream; real streaming is windowed-only.
#[cfg(windows)]
fn run_headless_cli(args: &[String], identity: (String, String)) {
    let arg = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    let Some(target) = arg("--connect") else {
        eprintln!("--headless requires --connect host[:port]");
        std::process::exit(2);
    };
    // Shared parser: this value is also handed to `find_by_addr`, so a mis-split IPv6
    // both probes the wrong address and misses its own saved record.
    let (host, port) = pf_client_core::deeplink::parse_addr_port(&target)
        .unwrap_or_else(|| (target.clone(), pf_client_core::deeplink::DEFAULT_PORT));

    // Speed test: measure the path over the real data plane, print the outcome, exit. The saved
    // fingerprint for this address (if any) pins the connect, like the GUI's per-host test.
    if flag("--speed-test") {
        let fp = trust::KnownHosts::load()
            .find_by_addr(&host, port)
            .map(|k| k.fp_hex.clone());
        match pf_client_core::speed::run_speed_probe(&host, port, fp.as_deref(), identity) {
            Ok(r) => {
                let mbps = f64::from(r.ceiling_kbps) / 1000.0;
                let recommended =
                    f64::from(pf_client_core::speed::recommended_kbps(r.ceiling_kbps)) / 1000.0;
                let clean = match r.clean {
                    Some(c) => format!(
                        " · at {:.0} Mbit/s: {:.1} % loss, {:.1} ms jitter",
                        f64::from(c.rate_kbps) / 1000.0,
                        c.loss_pct,
                        f64::from(c.jitter_us) / 1000.0
                    ),
                    None => String::new(),
                };
                println!(
                    "{mbps:.0} Mbit/s measured{clean} · recommended bitrate {recommended:.0} Mbit/s (--bitrate {recommended:.0})"
                );
            }
            Err(e) => {
                eprintln!("speed test failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // Only --speed-test remains headless: real streaming runs in the windowed app's spawned
    // punktfunk-session binary, which the deleted in-process frame-count path was replaced by.
    eprintln!("--headless supports only --speed-test now \u{2014} run the windowed app to stream");
    std::process::exit(2);
}

/// `--discover`: browse the LAN for punktfunk hosts (mDNS) and print them, then exit.
#[cfg(windows)]
fn discover_and_print() {
    use std::time::{Duration, Instant};
    println!("Browsing the LAN for Punktfunk hosts (~5 s)…");
    let (rx, _rescan) = pf_client_core::discovery::browse();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = std::collections::HashSet::new();
    while Instant::now() < deadline {
        while let Ok(event) = rx.try_recv() {
            let pf_client_core::discovery::DiscoveryEvent::Resolved(h) = event else {
                continue;
            };
            if seen.insert(h.key.clone()) {
                println!(
                    "  {}  {}:{}  pair={}  fp={}",
                    h.name,
                    h.addr,
                    h.port,
                    if h.pair.is_empty() {
                        "optional"
                    } else {
                        &h.pair
                    },
                    if h.fp_hex.is_empty() { "-" } else { &h.fp_hex },
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if seen.is_empty() {
        println!("  (none found — is a host running with --native / punktfunk1-host?)");
    }
}

/// WinUI 3 / Direct3D11 / WASAPI / SDL3 are Windows turf; this stub keeps `cargo build
/// --workspace` green on Linux/macOS (the other native clients live in
/// clients/linux and clients/apple).
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "punktfunk-client-windows is Windows-only — the Linux client lives in \
         clients/linux, the macOS client in clients/apple"
    );
    std::process::exit(2);
}
