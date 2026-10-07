//! Streaming host: virtual display, capture, encode, then FEC + packetize + pace
//! + send via `punktfunk_core`. Input returns through the inject backends.
//!
//! `serve` is the secure default (native punktfunk + management API).
//! `--gamestream` is opt-in, trusted-LAN only. `punktfunk1-host` is the native
//! plane alone. `spike` writes encoded AUs to a file and loopbacks them through
//! `punktfunk_core`.
//!
//! Platform backends are `#[cfg]`; the crate still compiles on every workspace
//! OS. Pin: `design/`. Evidence: this crate's tests and `docs/adr/`.

// Dead methods/paths exist before their backends land.
#![allow(dead_code)]
// Keep `unsafe fn` only where a caller can violate a contract (raw pointer / borrowed HANDLE).
// Workspace lints already require `// SAFETY:` on every `unsafe` block.

// Shim: audio backends live in `pf-audio`; keep `crate::audio::*` for this crate's callers.
mod audio {
    pub(crate) use pf_audio::*;
}
mod bringup;
mod capture;
mod detect;
mod devtest;
/// Structured health verdicts — design/web-console-diagnostics.md.
#[forbid(unsafe_code)]
mod diagnostics;
// `#[path]` keeps `crate::*` names flat while files live under `src/linux/`.
#[cfg(target_os = "linux")]
#[path = "linux/drm_sync.rs"]
mod drm_sync;
// Everything Windows-only lives under `src/windows/`; the flat names below keep every
// `crate::install::*` path unchanged. Off Windows, `windows::entry` is the no-op twin.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows::{game_term, install, interactive, service, tray};
// What this host reads of the multi-seat contract; unset means the console host.
#[cfg(target_os = "windows")]
use pf_paths::seat;
#[cfg(not(target_os = "windows"))]
mod windows {
    pub(crate) mod entry {
        pub(crate) fn service_run_requested() -> bool {
            false
        }
        pub(crate) fn init_file_logging(_filter: tracing_subscriber::EnvFilter) {}
        pub(crate) fn install_crash_handler() {}
        pub(crate) fn preflight(_management_cli: bool) {}
        pub(crate) fn serve_startup_recover() {}
        pub(crate) fn subcommand(_cmd: &str, _args: &[String]) -> Option<anyhow::Result<()>> {
            None
        }
        pub(crate) fn print_usage() {}
    }
    /// No IDD-push display here: no slot to reserve, no topology watchdog, no driver encoder.
    pub(crate) mod idd {
        use anyhow::Result;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        pub(crate) fn setup_guard(
            _capture: crate::session_plan::CaptureBackend,
            _identity: Option<[u8; 32]>,
            _size: (u32, u32),
            _stop: &Arc<AtomicBool>,
        ) -> Result<Option<()>> {
            Ok(None)
        }
        pub(crate) fn topology_reassert_gen() -> u64 {
            0
        }
        pub(crate) fn hw_cursor_capable() -> bool {
            false
        }
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn open_driver_encoder(
            _plan: &crate::session_plan::SessionPlan,
            _capturer: &dyn crate::capture::Capturer,
            _size: (u32, u32),
            _fps: u32,
            _bitrate_bps: u64,
            _bit_depth: u8,
            _client_hdr: Option<pf_frame::HdrMeta>,
            _wire_seq_base: u32,
        ) -> Result<Box<dyn crate::encode::Encoder>> {
            anyhow::bail!("the in-driver encoder is Windows IDD-push only")
        }
    }
    /// Only Windows can be pre-planted: the Unix config dir is 0700 from birth.
    pub(crate) mod planted {
        pub(crate) fn quarantine_planted_secret(_path: &std::path::Path) -> bool {
            false
        }
    }
}
use windows::planted;
// Shim: encode backends live in `pf-encode`; keep `crate::encode::*` for this crate's callers.
mod encode {
    pub(crate) use pf_encode::*;

    /// Refresh rate a client may ask for, the companion to [`validate_dimensions`].
    ///
    /// The display is created at it times `vdisplay_hz_mult_for`, so bound the product at the
    /// largest multiple any compositor gets: an out-of-contract value otherwise reaches mode
    /// selection and kills the session after Welcome, and a huge one overflows the multiply.
    pub(crate) fn validate_refresh(refresh_hz: u32) -> anyhow::Result<()> {
        const MAX_HZ: u32 = 1000;
        let mult = pf_host_config::config().vdisplay_hz_mult_for(refresh_hz, true);
        let effective = refresh_hz.saturating_mul(mult);
        anyhow::ensure!(
            (1..=MAX_HZ).contains(&refresh_hz) && effective <= MAX_HZ,
            "refresh {refresh_hz} Hz is out of range (1..={} at hz_mult {mult})",
            MAX_HZ / mult
        );
        Ok(())
    }
}
mod encode_recovery;
mod events;
// Launch, lease and liveness of a session's game; the flat names keep `crate::gamelease::*`.
mod game;
use game::{
    gamelease, holds, launchreg, procscan, runstate, session_launch, session_settings,
    stream_marker,
};
mod gamestream;
#[cfg(target_os = "linux")]
#[path = "linux/gpuclocks.rs"]
mod gpuclocks;
mod hooks;
// What every plane shares: host facts, session state, `serve`.
mod host;
// The box itself: identity, adverts, wake, power, sleep; the flat names keep `crate::power::*`.
mod hostsys;
use hostsys::{discovery, identity, osinfo, power, sleep_inhibit, wol};
// Shim: inject backends live in `pf-inject`; keep `crate::inject::*` for this crate's callers.
mod inject {
    pub(crate) use pf_inject::*;
}
mod pen_sink;
// Unix wall clock every stored deadline and event stamp reads.
mod clock;
// Compositor + gamescope route for a connect, shared by the native and GameStream planes.
mod compositor_route;
mod library;
// Network-facing secure-default surface. `not(test)` because tests mutate process env
// (`set_var` is unsafe in 2024) and `native` has in-process C-ABI roundtrips.
#[cfg_attr(not(test), forbid(unsafe_code))]
mod mgmt;
#[forbid(unsafe_code)]
mod mgmt_token;
// Loopback client of the surfaces above; same `forbid` (holds the operator token and cert pin).
#[forbid(unsafe_code)]
mod ctl;
#[cfg_attr(not(test), forbid(unsafe_code))]
mod native;
#[forbid(unsafe_code)]
mod native_pairing;
// Live per-session pad tap the console's Controllers page streams.
mod emulators;
mod pad_feed;
// Plugin runner, access and store; the flat names keep `crate::plugins::*`.
mod plugin_host;
use plugin_host::{plugins, store};
mod send_pacing;
mod session_plan;
mod slug;
mod spike;
// Session status, stats and log capture; the flat names keep `crate::session_status::*`.
mod telemetry;
use telemetry::{
    client_logs, encoder_sessions, link_health, log_capture, net_health, session_status,
    stats_recorder,
};
#[cfg(test)]
mod test_support {
    /// A fresh directory that lives until the calling test's thread ends, for a helper that
    /// returns a path rather than a guard. The test harness runs each test on its own thread.
    pub(crate) fn scratch() -> std::path::PathBuf {
        thread_local!(static DIRS: std::cell::RefCell<Vec<tempfile::TempDir>> = const {
            std::cell::RefCell::new(Vec::new())
        });
        let dir = tempfile::tempdir().expect("create a scratch dir");
        let path = dir.path().to_path_buf();
        DIRS.with_borrow_mut(|dirs| dirs.push(dir));
        path
    }
}
mod update;
mod version;
// The browser plane (design/web-client-implementation-plan.md Phase 1). Runtime opt-in.
mod webtransport;
// Shim: virtual-display lives in `pf-vdisplay`; keep `crate::vdisplay::*` for this crate's callers.
mod vdisplay {
    pub(crate) use pf_vdisplay::*;
}
// Shim: GPU import lives in `pf-zerocopy`; keep `crate::zerocopy::*` for `session_plan`.
#[cfg(target_os = "linux")]
mod zerocopy {
    pub(crate) use pf_zerocopy::*;
}

use anyhow::{bail, Context, Result};
use encode::Codec;
use spike::{Options, Source};
use std::path::PathBuf;

/// Console filter when `RUST_LOG` is unset. `zbus::proxy` warns once per portal
/// Request/Session proxy whose server answers no `org.freedesktop.DBus.Properties`
/// — four per stream under xdg-desktop-portal-hyprland/-wlr, and nothing in the
/// portal flow reads a property off those two transient objects. It is the module's
/// only `warn!`, so `error` there costs nothing else. The ring keeps them regardless.
const DEFAULT_LOG_FILTER: &str = "info,zbus::proxy=error";

// POSIX `geteuid`: no arguments, no memory, cannot fail — so `safe` for every caller.
#[cfg(unix)]
unsafe extern "C" {
    safe fn geteuid() -> u32;
}

fn main() {
    // Before any `ureq` agent (cover-art, webhooks, catalog, updates).
    punktfunk_core::tls::install_default_provider();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into());
    // SCM `service run` has no console; log to a file, not stderr.
    if windows::entry::service_run_requested() {
        windows::entry::init_file_logging(filter);
    } else {
        // stderr so stdout stays machine-readable (`openapi > spec.json`). The ring tees DEBUG+
        // past this filter, so the Logs tab keeps every line stderr drops.
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        // Colour only on a terminal: the service child's stderr is `host.log`.
        let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
        let fmt = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            fmt
        } else {
            fmt.with_ansi(false)
        };
        log_capture::install_global(
            tracing_subscriber::registry()
                .with(
                    log_capture::RingLayer
                        .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
                )
                .with(fmt.with_filter(filter)),
        );
    }

    // Push panics into the ring before the default hook (stderr is gone when detached).
    // Do not emit via `tracing`: the registry uses TLS, so a destructor-path panic re-enters
    // the hook (`MustAbort::PanicInHook`) and the cause is erased. `LogRing` is OnceLock+Mutex;
    // `thread::current().name()` and `Backtrace::force_capture()` are TLS-teardown safe.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `payload_as_str` needs Rust 1.91; MSRV is 1.82.
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let location = info
            .location()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<unknown>".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let ts_ms = clock::unix_ms();
        log_capture::ring().push_remote(
            "ERROR",
            "punktfunk_host::panic",
            &format!("PANIC: {payload} (thread={thread}, at {location})\n{backtrace}"),
            ts_ms,
        );
        default_panic(info);
    }));
    windows::entry::install_crash_handler();

    if let Err(e) = real_main() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

/// Aim absolute input at the pinned capture monitor (env, else stored policy).
///
/// Called at startup and whenever the console writes the pin so a picker change
/// re-aims without a host restart. Lives here, not in the mirror backend:
/// `pf-vdisplay` must not depend on `pf-inject`, and the injector is host-lifetime
/// (shared by every session) — design/per-monitor-portal-capture.md.
#[cfg(target_os = "linux")]
pub(crate) fn refresh_capture_monitor_anchor(context: &str) {
    let Some(want) = pf_vdisplay::capture_monitor() else {
        // Drop a stale anchor so a later virtual-display session does not inherit it.
        // Log a clear only when an anchor was set; unpinned hosts must stay quiet at startup.
        if pf_inject::absolute_anchor().is_some() {
            tracing::info!(
                context,
                "capture monitor: cleared — sessions create a virtual display again and absolute \
                 input is no longer anchored"
            );
        }
        pf_inject::set_absolute_anchor(None);
        return;
    };
    match pf_vdisplay::detect().and_then(pf_vdisplay::monitors::list) {
        Ok(ms) => match pf_vdisplay::monitors::resolve(&ms, &want) {
            Ok(m) => {
                // Match libei by origin: two heads can share a size; a mirror is not client-sized.
                pf_inject::set_absolute_anchor(Some(pf_inject::AbsoluteAnchor {
                    origin: Some((m.x, m.y)),
                    mapping_id: None,
                }));
                tracing::info!(
                    context,
                    connector = %m.connector,
                    description = %m.description,
                    mode = %m.mode_label(),
                    at = %format!("+{}+{}", m.x, m.y),
                    "capture monitor: sessions will mirror this monitor (no virtual display) and \
                     absolute input is anchored to it"
                );
            }
            // Do not guess an anchor; `create` fails with the same unresolved pin.
            Err(e) => {
                pf_inject::set_absolute_anchor(None);
                tracing::warn!(
                    context,
                    error = %e,
                    "capture monitor: the pinned monitor is not on this host — sessions will fail \
                     to start until it is corrected or cleared"
                );
            }
        },
        Err(e) => {
            pf_inject::set_absolute_anchor(None);
            tracing::warn!(
                context,
                error = %format!("{e:#}"),
                monitor = %want,
                "capture monitor: a monitor is pinned but the monitors could not be enumerated"
            );
        }
    }
}

/// Take the credentials out of our own environment before anything can inherit them: hooks, games
/// and the plugin runner are children of this process, and none of them has business with the
/// admin API. `mgmt_token` keeps the values and persists a pinned one to its file.
fn take_env_credentials() {
    let read = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    mgmt_token::adopt_env_tokens(read("PUNKTFUNK_MGMT_TOKEN"), read("PUNKTFUNK_PLUGIN_TOKEN"));
    for key in mgmt_token::CREDENTIAL_ENV_VARS {
        // SAFETY: the first statement of `real_main`, so this process is still single-threaded and
        // nothing can read the environment concurrently.
        unsafe { std::env::remove_var(key) };
    }
}

// Package/service/driver CLI: skip the banner and the Windows GPU-pref hook (its DPI
// probe WARNs `access denied` on `plugins add`). `service run` is the SCM host, not CLI.
fn is_management_cli(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("plugins")
        | Some("driver")
        | Some("web")
        | Some("tray")
        // Loopback API client; `watch` is long-lived — do not take GPU clocks or the DXGI hook.
        | Some("ctl")
        | Some("settings")
        | Some("openapi")
        | Some("library")
        | Some("detect-conflicts")
        // The per-app audio pin, run as the console user by the capture thread.
        | Some("voice-route")
        // Prints the same list `refresh_capture_monitor_anchor` would log; skip host startup.
        | Some("list-monitors")
        | Some("-h")
        | Some("--help")
        | Some("help")
        | None => true,
        Some("service") => args.get(1).map(String::as_str) != Some("run"),
        _ => false,
    }
}

/// Once per process, before the subcommand: the banner, the capture anchor, the display
/// event sink, the voice-pin hooks, platform preflight and the GPU driver profile. A
/// management CLI skips the host parts.
fn startup(args: &[String]) {
    let management_cli = is_management_cli(args);

    if !management_cli {
        tracing::info!(
            "punktfunk-host {} (punktfunk_core ABI v{})",
            crate::version::get(),
            punktfunk_core::ABI_VERSION
        );
    }

    #[cfg(target_os = "linux")]
    if !management_cli {
        refresh_capture_monitor_anchor("startup");
    }

    // Once: pf-vdisplay emits a crate-neutral `DisplayEvent`; this crate owns the SSE bus type.
    let _ = pf_vdisplay::DISPLAY_EVENT_SINK.set(Box::new(|ev| match ev {
        pf_vdisplay::DisplayEvent::Created {
            backend,
            width,
            height,
            refresh_hz,
        } => events::emit(events::EventKind::DisplayCreated {
            backend,
            mode: events::mode_str(width, height, refresh_hz),
        }),
        pf_vdisplay::DisplayEvent::Released { count } => {
            events::emit(events::EventKind::DisplayReleased { count })
        }
    }));

    // Once: the voice-chat pin reaches processes and the console user through the host.
    #[cfg(target_os = "windows")]
    let _ = audio::voice_route::HOST_HOOKS.set(audio::voice_route::HostHooks {
        processes: procscan::processes,
        run_hidden_as_user: interactive::run_hidden_as_current_session_user,
        running_as_system: hooks::running_as_system,
    });

    windows::entry::preflight(management_cli);

    // P2-cap driver profile only. Clock pin is per live client (`gpuclocks::session_pin`), not
    // host-lifetime, so idle clocks stay down. No-op off NVIDIA.
    #[cfg(target_os = "linux")]
    if matches!(
        args.first().map(String::as_str),
        Some("serve") | Some("punktfunk1-host")
    ) {
        gpuclocks::on_host_start();
    }
}

fn real_main() -> Result<()> {
    take_env_credentials();
    #[cfg(target_os = "linux")]
    if let Some(value) = encode::anv_video_encode_flag() {
        // SAFETY: right after `take_env_credentials`, so still single-threaded, and no Vulkan
        // instance exists yet (Mesa reads the variable when its ICD loads).
        unsafe { std::env::set_var("ANV_DEBUG", &value) };
        tracing::debug!(value, "ANV_DEBUG opted into Mesa Vulkan Video encode");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();

    if matches!(
        args.first().map(String::as_str),
        Some("--version") | Some("-V") | Some("version")
    ) {
        println!("punktfunk-host {}", crate::version::get());
        return Ok(());
    }

    startup(&args);

    match args.first().map(String::as_str) {
        Some("serve") => {
            let (mgmt_opts, native, gamestream) = parse_serve(&args[1..])?;
            // A launcher rewrite can drop a granted folder's ACE; re-apply them each boot.
            plugins::converge_grants();
            // Restart-class settings changed after this point wait for a restart.
            pf_host_config::mark_started();
            // Must run before any new session touches the topology.
            windows::entry::serve_startup_recover();
            host::serve(mgmt_opts, native, gamestream)
        }
        Some("detect-conflicts") => {
            let found = detect::scan();
            if found.is_empty() {
                println!("No conflicting game-streaming host detected.");
                return Ok(());
            }
            print!("{}", detect::render_report(&found));
            // Exit 1 only for a host that runs or will auto-start. Dormant leftovers print, then 0
            // (installers gate on this; see `detect` docs).
            if detect::any_active(&found) {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("ctl") => ctl::main(&args[1..]),
        Some("settings") => settings_cli(&args[1..]),
        Some("plugins") => plugins::main(&args[1..]),
        Some("openapi") => {
            print!("{}", mgmt::openapi_json());
            Ok(())
        }
        // Covers the host fetched for a client. Dropping them costs one refetch each.
        Some("library") if args.get(1).is_some_and(|a| a == "art") => {
            if args.get(2).map(String::as_str) != Some("--clear") {
                anyhow::bail!("usage: punktfunk-host library art --clear");
            }
            let (files, bytes) = library::clear_art_store()?;
            println!("cleared {files} covers, {} MiB", bytes / 1024 / 1024);
            Ok(())
        }
        // Same JSON as `GET /api/v1/library`.
        Some("library") => {
            println!("{}", serde_json::to_string_pretty(&library::all_games())?);
            Ok(())
        }
        Some("input-test") => devtest::input_test(),
        #[cfg(target_os = "linux")]
        Some("pen-test") => devtest::pen_test(),
        #[cfg(target_os = "linux")]
        Some("zerocopy-probe") => zerocopy::probe(),
        // Hidden: capture spawns this from a pinned fd of its own image — design/zerocopy-worker-isolation.md.
        #[cfg(target_os = "linux")]
        Some("zerocopy-worker") => zerocopy::worker::run_from_args(&args[1..]),
        // Hidden: backgrounds beside a nested gamescope app so a fresh headless instance composites
        // from the first second. Needs that session's DISPLAY.
        #[cfg(target_os = "linux")]
        Some("gamescope-splash") => vdisplay::gamescope_splash_client(),
        #[cfg(target_os = "linux")]
        Some("nv12-selftest") => zerocopy::nv12_selftest(),
        #[cfg(target_os = "linux")]
        Some("hdr-probe") => devtest::hdr_probe(),
        // Exit 0 iff a virtual output can be created now — bringup scripts poll this instead of `sleep`.
        Some("probe-compositor") => {
            let compositor = vdisplay::detect()?;
            vdisplay::probe(compositor).with_context(|| format!("{compositor:?} not ready"))?;
            println!("{compositor:?} ready");
            Ok(())
        }
        // `voice-route set|clear …`: the per-app output pin. The capture thread spawns it as the
        // console user, because a SYSTEM caller writes SYSTEM's app preferences, not the user's.
        #[cfg(target_os = "windows")]
        Some("voice-route") => audio::voice_route::cli(&args[1..]),
        #[cfg(target_os = "linux")]
        Some("list-monitors") => devtest::list_monitors(),
        #[cfg(target_os = "linux")]
        Some("mirror-test") => devtest::mirror_test(&args),
        #[cfg(target_os = "linux")]
        Some("anchor-test") => devtest::anchor_test(&args),
        #[cfg(target_os = "linux")]
        Some("dualsense-test") => devtest::dualsense_test(&args),
        #[cfg(target_os = "linux")]
        Some("pad-sink-test") => devtest::pad_sink_test(&args),
        #[cfg(target_os = "linux")]
        Some("pad-usbip-test") => devtest::pad_usbip_test(&args),
        #[cfg(target_os = "linux")]
        Some("switchpro-test") => devtest::switchpro_test(&args),
        Some("spike") => spike::run(parse_spike(&args[1..])?),
        Some("punktfunk1-host") => native::run(parse_punktfunk1(&args[1..])?),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        // No implicit `serve`; bare invocation is `None` above and prints help.
        Some(other) => match windows::entry::subcommand(other, &args) {
            Some(result) => result,
            None => bail!("unknown command '{other}' (try --help)"),
        },
    }
}

/// `settings set <id> <value>` writes the console's settings store without a running host, so an
/// installer's choice stays the console's to change. The value is JSON (`true`, `30`; `null`
/// clears), else a bare string.
fn settings_cli(args: &[String]) -> Result<()> {
    let [verb, id, raw] = args else {
        bail!("usage: punktfunk-host settings set <id> <value>");
    };
    if verb != "set" {
        bail!("unknown settings verb '{verb}' (try: set)");
    }
    let value = serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::from(raw.as_str()));
    let patch = serde_json::Map::from_iter([(id.clone(), value.clone())]);
    pf_host_config::save(&patch).context("save host settings")?;
    println!("{id}={value} → {}", pf_host_config::store_path().display());
    Ok(())
}

/// Native plane + management API always run. `--gamestream` is trusted-LAN only.
/// Pairing is required unless `--open`. Returns `(mgmt, native, gamestream)`.
fn parse_serve(args: &[String]) -> Result<(mgmt::Options, native::NativeServe, bool)> {
    let mut opts = mgmt::Options::default();
    let mut native_port: u16 = 9777;

    let mut open = false;
    let mut gamestream = false;
    // The browser plane, off unless asked for — same stance as GameStream above.
    let mut webtransport = false;
    let mut webtransport_port: u16 = webtransport::DEFAULT_PORT;
    let mut webtransport_port_explicit = false;
    // Interface only; the port is its own flag so `--webtransport-bind` reads like an address.
    let mut webtransport_host = "::".to_string();
    let mut webtransport_bind_explicit = false;
    let mut no_mdns = false;
    // If unset, bind wide below so paired clients can browse. Admin stays loopback in `require_auth`.
    let mut mgmt_bind_explicit = false;
    // Explicit `--native-port` outranks `PUNKTFUNK_NATIVE_PORT` after the loop.
    let mut native_port_explicit = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--mgmt-bind" => {
                opts.bind = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --mgmt-bind (want IP:PORT)"))?;
                mgmt_bind_explicit = true;
            }
            // No-op: the native plane always runs.
            "--native" => {}
            "--native-port" => {
                native_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --native-port (want a port number)"))?;
                native_port_explicit = true;
            }
            // Video rides the native port. Accepted so an older service unit still starts.
            "--data-port" => {
                next()?;
                tracing::warn!("--data-port is ignored: video uses the native port");
            }
            "--gamestream" | "--moonlight" => gamestream = true,
            "--webtransport" => webtransport = true,
            "--webtransport-port" => {
                webtransport_port = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --webtransport-port (want a port number)"))?;
                webtransport_port_explicit = true;
            }
            "--webtransport-bind" => {
                webtransport_host = next()?;
                webtransport_bind_explicit = true;
            }
            "--open" => open = true,
            // Bridged Docker / CI netns: multicast never arrives.
            "--no-mdns" => no_mdns = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    // Env (persisted), else the `mgmt-token` file, else generate. HTTPS+token even on loopback.
    if opts.token.is_none() {
        opts.token = Some(crate::mgmt_token::load_or_generate()?);
    }
    // Installs before this build granted every local account read on the config tree.
    #[cfg(windows)]
    crate::plugins::converge_config_dir_acls();
    // The tray's bearer. A seat host has no tray and serves no summary. Not fatal: the tray
    // then shows the host as running without detail.
    #[cfg(target_os = "windows")]
    let seat_host = seat::is_seat_host();
    #[cfg(not(target_os = "windows"))]
    let seat_host = false;
    if !seat_host {
        match crate::mgmt_token::mint_tray_token() {
            Ok(t) => opts.tray_token = Some(t),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "tray token not written"),
        }
    }
    // Mint only if the runner is installed — otherwise a second admin-adjacent credential sits
    // on disk for a subsystem that is not running. Scope: `plugin_may_access`, not pairing/hooks.
    let runner = crate::plugins::runtime_status();
    if runner.installed {
        opts.plugin_token = Some(crate::mgmt_token::load_or_generate_plugin()?);
        // One token per installed plugin, so the API can tell them apart: a plugin may write its
        // own registration and its own provider, and no other's.
        opts.plugin_tokens = crate::mgmt_token::load_or_generate_per_plugin()?;
        // An upgrade or a hand-edited grants file may have changed what the runner must see.
        #[cfg(windows)]
        crate::plugins::publish_sandbox_override();
        crate::plugins::converge_runner_roots();
        crate::plugins::converge_runner_acls(&runner);
    }
    // Default all-interfaces so paired clients browse over mTLS. Admin stays loopback in
    // `require_auth`. Packaged units ship a fixed ExecStart — `host.env` is the upgrade-safe pin;
    // CLI wins as the more explicit of the two.
    if !mgmt_bind_explicit {
        opts.bind = match pf_host_config::config().mgmt_bind.as_deref() {
            Some(s) => s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_MGMT_BIND '{s}' (want IP:PORT)"))?,
            None => std::net::SocketAddr::from(([0, 0, 0, 0], mgmt::DEFAULT_PORT)),
        };
    }
    // A bad value is fatal — serving 9777 while host.env says otherwise reads as
    // "I moved the port and the client still cannot reach me".
    if !native_port_explicit {
        if let Some(s) = pf_host_config::config().native_port.as_deref() {
            native_port = s
                .parse()
                .map_err(|_| anyhow::anyhow!("bad PUNKTFUNK_NATIVE_PORT '{s}' (want a port)"))?;
        }
    }
    // Same function as the token persist so the console unit sees both. A race falls back to
    // 47990; `Restart=always` retries.
    mgmt::publish_endpoint(opts.bind);
    let native = native::NativeServe {
        port: native_port,
        require_pairing: !open,
        // Real bound port, not the default, so mDNS clients follow a moved mgmt port.
        mgmt_port: opts.bind.port(),
        mdns: !no_mdns && discovery::mdns_enabled(),
        // Resolved just below, once the env fallbacks have been applied.
        webtransport_bind: None,
    };
    if !webtransport_port_explicit {
        if let Some(s) = pf_host_config::config().webtransport_port.as_deref() {
            webtransport_port = s.parse().map_err(|_| {
                anyhow::anyhow!("bad PUNKTFUNK_WEBTRANSPORT_PORT '{s}' (want a port)")
            })?;
        }
    }
    if !webtransport_bind_explicit {
        if let Some(s) = pf_host_config::config().webtransport_bind.as_deref() {
            webtransport_host = s.to_string();
        }
    }
    // Bracketed IPv6 or a bare IPv4, joined to the port flag. A bad address is a startup error:
    // listening on every interface when the operator asked for one is the wrong way to fail.
    let webtransport_bind: std::net::SocketAddr = format!(
        "{}:{webtransport_port}",
        if webtransport_host.contains(':') && !webtransport_host.starts_with('[') {
            format!("[{webtransport_host}]")
        } else {
            webtransport_host.clone()
        }
    )
    .parse()
    .map_err(|_| anyhow::anyhow!("bad --webtransport-bind '{webtransport_host}' (want an IP)"))?;
    // A flag outranks env and the console's value; pinning it lets the console show why.
    if gamestream {
        pf_host_config::pin("gamestream", "--gamestream", serde_json::Value::Bool(true));
    }
    if webtransport {
        pf_host_config::pin(
            "webtransport",
            "--webtransport",
            serde_json::Value::Bool(true),
        );
    }
    let gamestream = pf_host_config::config().gamestream;
    let native = native::NativeServe {
        webtransport_bind: pf_host_config::config()
            .webtransport
            .then_some(webtransport_bind),
        ..native
    };
    // Refused here rather than at bind: the plane is spawned as a secondary tier whose errors
    // only log, and this combination must not be something an operator can miss.
    if native.webtransport_bind.is_some()
        && !webtransport::is_confined(
            native.require_pairing,
            &pf_host_config::config().webtransport_origins,
        )
    {
        anyhow::bail!(
            "--open leaves the browser plane unauthenticated, and with no origin list any page \
             the user visits can stream and inject input (WebTransport gets no same-origin rule). \
             Set PUNKTFUNK_WEBTRANSPORT_ORIGINS, or drop --open"
        );
    }
    Ok((opts, native, gamestream))
}

/// `punktfunk1-host` flags. A bad value is an error, as in [`parse_serve`]: a typo'd port
/// must not quietly serve 9777.
fn parse_punktfunk1(args: &[String]) -> Result<native::Punktfunk1Options> {
    fn value<T: std::str::FromStr>(flag: &str, v: String) -> Result<T> {
        v.trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("bad {flag} '{v}'"))
    }
    let mut opts = native::Punktfunk1Options {
        port: 9777,
        source: native::Punktfunk1Source::Synthetic,
        seconds: 30,
        frames: 300,
        max_sessions: 0,
        max_concurrent: native::DEFAULT_MAX_CONCURRENT,
        // Pairing required unless `--allow-tofu`. `--require-pairing`/`--allow-pairing` are no-ops.
        require_pairing: true,
        allow_pairing: true,
        pairing_pin: None,
        paired_store: None,
        // QUIC idle timeout; flag overrides env; absent = core default (8 s).
        idle_timeout: native::idle_timeout_from_env(),
        mdns: discovery::mdns_enabled(),
    };
    let mut source = "synthetic".to_string();
    // What `--source synthetic-abr` encodes; ignored by the other sources.
    let (mut content, mut fill, mut idr_pct) =
        ("steady".to_string(), 100u32, native::DEFAULT_IDR_PCT);
    let (mut recovery_ms, mut bringup_ms, mut serve_ramp) = (0u64, 2_500u64, true);
    let mut answer = "idr".to_string();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--port" => opts.port = value(arg, next()?)?,
            "--source" => source = next()?,
            "--seconds" => opts.seconds = value(arg, next()?)?,
            "--frames" => opts.frames = value(arg, next()?)?,
            "--max-sessions" => opts.max_sessions = value(arg, next()?)?,
            "--max-concurrent" => opts.max_concurrent = value(arg, next()?)?,
            "--allow-tofu" => opts.require_pairing = false,
            "--require-pairing" | "--allow-pairing" => {}
            // Empty would arm SPAKE2 with an empty password (same trap as `--mgmt-token`).
            "--pairing-pin" => match next()? {
                p if p.trim().is_empty() => bail!("--pairing-pin must not be empty"),
                p => opts.pairing_pin = Some(p),
            },
            "--idle-timeout-ms" => match value::<u64>(arg, next()?)? {
                0 => bail!("--idle-timeout-ms must be > 0"),
                ms => opts.idle_timeout = Some(std::time::Duration::from_millis(ms)),
            },
            "--no-mdns" => opts.mdns = false,
            "--content" => content = next()?,
            "--fill" => fill = value(arg, next()?)?,
            "--recovery-ms" => recovery_ms = value(arg, next()?)?,
            "--keyframe-answer" => answer = next()?,
            "--bringup-ms" => bringup_ms = value(arg, next()?)?,
            "--idr-pct" => idr_pct = value(arg, next()?)?,
            "--no-ramp" => serve_ramp = false,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }
    opts.source = match source.as_str() {
        "synthetic" => native::Punktfunk1Source::Synthetic,
        "virtual" => native::Punktfunk1Source::Virtual,
        "synthetic-abr" => {
            if !(1..=100).contains(&fill) {
                bail!("--fill takes 1-100");
            }
            if idr_pct == 0 {
                bail!("--idr-pct must be > 0");
            }
            let Some(answer) = native::KeyframeAnswer::parse(&answer) else {
                bail!("--keyframe-answer takes idr or wave:<n>");
            };
            let Some(content) = native::Content::parse(&content, fill) else {
                bail!("--content takes steady, idle-then-motion, frame-driven:<fps> or motion-then-still:<fps>");
            };
            native::Punktfunk1Source::SyntheticAbr(native::SynthAbrShape {
                content,
                recovery: std::time::Duration::from_millis(recovery_ms),
                answer,
                idr_pct,
                bringup: std::time::Duration::from_millis(bringup_ms),
                serve_ramp,
            })
        }
        other => bail!("unknown --source '{other}' (synthetic|synthetic-abr|virtual)"),
    };
    Ok(opts)
}

fn parse_spike(args: &[String]) -> Result<Options> {
    let mut source = Source::Portal;
    let mut width = 1920u32;
    let mut height = 1080u32;
    let mut fps = 60u32;
    let mut seconds = 5u32;
    let mut codec = Codec::H265;
    let mut hdr = false;
    let mut bitrate_mbps = 20u64;
    let mut out: Option<PathBuf> = None;
    let mut loopback = true;
    let mut wire_chunk: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg {
            "--source" => {
                source = match next()?.as_str() {
                    "synthetic" => Source::Synthetic,
                    "synthetic-nv12" => Source::SyntheticNv12,
                    "portal" => Source::Portal,
                    // `kwin-virtual` is what this was called when only KWin had one.
                    "virtual" | "kwin-virtual" => Source::Virtual,
                    other => {
                        bail!(
                            "unknown --source '{other}' \
                             (synthetic|synthetic-nv12|portal|virtual)"
                        )
                    }
                }
            }
            "--width" => {
                width = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --width"))?
            }
            "--height" => {
                height = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --height"))?
            }
            "--fps" => fps = next()?.parse().map_err(|_| anyhow::anyhow!("bad --fps"))?,
            "--seconds" => {
                seconds = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --seconds"))?
            }
            "--codec" => {
                codec = match next()?.as_str() {
                    "h264" => Codec::H264,
                    "h265" | "hevc" => Codec::H265,
                    "av1" => Codec::Av1,
                    // Needs `pyrowave` and `PUNKTFUNK_ENCODER=pyrowave` (raw-dmabuf passthrough).
                    "pyrowave" => Codec::PyroWave,
                    other => bail!("unknown --codec '{other}' (h264|h265|av1|pyrowave)"),
                }
            }
            "--hdr" => hdr = true,
            "--bitrate" => {
                bitrate_mbps = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --bitrate (Mbps)"))?
            }
            "--out" => out = Some(PathBuf::from(next()?)),
            "--no-loopback" => loopback = false,
            "--wire-chunk" => {
                let v: usize = next()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --wire-chunk (bytes)"))?;
                wire_chunk = (v > 0).then_some(v);
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
        i += 1;
    }

    if fps == 0 || width == 0 || height == 0 || seconds == 0 {
        bail!("--fps/--width/--height/--seconds must be > 0");
    }

    let out = out.unwrap_or_else(|| {
        let ext = match codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "obu",
            // Concatenated packets; not an FFmpeg-playable stream.
            Codec::PyroWave => "pyrowave",
        };
        std::env::temp_dir().join(format!("punktfunk-spike.{ext}"))
    });

    Ok(Options {
        source,
        width,
        height,
        fps,
        seconds,
        codec,
        hdr,
        bitrate_bps: bitrate_mbps.saturating_mul(1_000_000),
        out,
        loopback,
        wire_chunk,
    })
}

fn print_usage() {
    eprintln!(
        "punktfunk-host — Linux streaming host

USAGE:
    punktfunk-host serve [OPTIONS]            native punktfunk host + management REST API
                                              (secure default; add --gamestream for Moonlight compat)
    punktfunk-host ctl <VERB>                 operator control over the local management API —
                                              pairing, devices, sessions, `watch` (line-JSON for a
                                              shell widget); `ctl --help` for the verb list
    punktfunk-host settings set <ID> <VALUE>  write one console setting to host-settings.json;
                                              restart the host to apply it
    punktfunk-host plugins <CMD>              install/run host plugins (add, remove, list, enable,
                                              disable, status) — `plugins --help` for details
    punktfunk-host tray <CMD>                 status-tray lifecycle (start, stop, status) — Windows;
                                              `start` is how you get the icon back without a re-logon
    punktfunk-host openapi                    print the management API's OpenAPI document (codegen)
    punktfunk-host library [art --clear]      print the game catalog as JSON; `art --clear` drops
                                              every cover the host fetched and cached on disk
    punktfunk-host punktfunk1-host [OPTIONS]  native punktfunk host alone (tests and benches)
    punktfunk-host probe-compositor           exit 0 iff the compositor is up + ready (bringup gate)
    punktfunk-host list-monitors              list the host's physical monitors (Linux) — the
                                              connector names PUNKTFUNK_CAPTURE_MONITOR takes
    punktfunk-host spike [OPTIONS]            capture→encode→file pipeline spike (dev tool)

SERVE OPTIONS:
    --mgmt-bind <IP:PORT>        management API address (or PUNKTFUNK_MGMT_BIND in host.env, which
                                 this flag overrides). Default: 0.0.0.0:47990 — paired clients
                                 reach the read-only surface, incl. the game library, over mTLS;
                                 the bearer admin API stays loopback-only. Pin 127.0.0.1:47990 to
                                 bind loopback only. Move the PORT (e.g. 0.0.0.0:47991) to share a
                                 machine with Sunshine/Apollo/Vibeshine, whose web UI owns 47990 —
                                 clients follow via mDNS and the console via mgmt-endpoint
    --gamestream  (--moonlight)  ALSO run the GameStream/Moonlight-compat planes (nvhttp pairing,
                                 RTSP, ENet control, _nvstream mDNS). OFF by default — they carry
                                 inherent on-path weaknesses (plain-HTTP pairing + legacy GCM nonce
                                 reuse, security-review #5/#9); enable only on a TRUSTED LAN.
                                 The flag locks the console's GameStream setting; an install sets
                                 that setting instead (`settings set gamestream true`)
    --native                     no-op (the native plane always runs in `serve`)
    --native-port <PORT>         native QUIC port (or PUNKTFUNK_NATIVE_PORT in host.env, which
                                 this flag overrides). Default 9777. Clients follow via mDNS, and
                                 a manually-added host keeps whatever port it was added with
    --open                       disable mandatory native pairing (default: pairing REQUIRED —
                                 an open host any LAN device can stream from is insecure)
    --no-mdns                    skip the mDNS adverts (native + GameStream) — for multicast-dead
                                 environments (bridged Docker, CI); clients connect via a manually
                                 added host. Also PUNKTFUNK_MDNS=0

PUNKTFUNK1-HOST OPTIONS:
    --port <N>                   QUIC listen port (default: 9777)
    --source <synthetic|synthetic-abr|virtual>
                                 test frames, frames sized from the live Automatic rate, or a
                                 virtual display + NVENC (default: synthetic). synthetic-abr
                                 needs no display and no GPU
    --content <SCRIPT>           what synthetic-abr encodes: steady, idle-then-motion,
                                 frame-driven:<fps> for a source slower than the session, or
                                 motion-then-still:<fps> for a minute of motion, then <fps>
                                 new frames a second among repeats (default: steady)
    --fill <PCT>                 share of each frame's bit allowance synthetic-abr fills,
                                 1-100 (default: 100)
    --recovery-ms <MS>           how long synthetic-abr takes to answer a keyframe request.
                                 0 (the default) answers on the next frame; a GPU host that
                                 rebuilds its pipeline takes about a second
    --keyframe-answer <KIND>     what synthetic-abr answers a keyframe request with: idr
                                 (the default), or wave:<n> to answer only every n-th ask
                                 with one, as a host that prefers an intra-refresh wave does.
                                 Either way the answer waits out the same IDR cooldown a real
                                 host applies, so a burst of asks costs one keyframe
    --idr-pct <PCT>              a keyframe's size as a percent of an ordinary frame
                                 (default: 1000). A hardware encoder runs VBV at one frame,
                                 so a measured host is far nearer 100 than 1000
    --bringup-ms <MS>            how long synthetic-abr holds its first frame back, the way
                                 a display session's pipeline build does (default: 2500).
                                 The client measures the link over this window
    --no-ramp                    do not offer to measure the link before the first frame.
                                 The client falls back to the in-session test burst, which
                                 is what it does against a host that predates the ramp
    --seconds <N>                per-session stream duration, virtual and synthetic-abr
                                 sources (default: 30)
    --frames <N>                 per-session frame count, synthetic source (default: 300)
    --max-sessions <N>           exit after N sessions; 0 = serve forever (default: 0)
    --max-concurrent <N>         stream at most N sessions at once (NVENC bound); overflow waits
                                 in the accept queue; 0 = unlimited (default: 4)
    --allow-tofu                 also accept UNPAIRED clients (trust-on-first-use) and advertise
                                 pair=optional. Default: pairing REQUIRED — the host rejects
                                 unpaired clients and logs a 4-digit pairing PIN at startup;
                                 TOFU without pairing is insecure on a LAN
    --pairing-pin <PIN>          fixed pairing PIN instead of the random per-ceremony one — for
                                 test harnesses/CI (deterministic `probe --pair`); do not use on
                                 a real LAN (a guessable PIN defeats the ceremony's rate limit)
    --no-mdns                    skip the _punktfunk._udp mDNS advert (multicast-dead environments;
                                 clients use --connect HOST:PORT). Also PUNKTFUNK_MDNS=0

SPIKE OPTIONS:
    --source <synthetic|synthetic-nv12|portal|virtual>
                                 frame source (default: portal). 'virtual' creates a platform
                                 virtual display at --width x --height; 'kwin-virtual' is an alias
    --seconds <N>                capture duration in seconds (default: 5)
    --fps <N>                    target frame rate (default: 60)
    --codec <h264|h265|av1|pyrowave>
                                 encode codec (default: h265). 'pyrowave' also wants
                                 PUNKTFUNK_ENCODER=pyrowave so capture takes the passthrough
    --hdr                        request HDR capture and a 10-bit encode; on Windows this is
                                 the seat readiness gate and fails rather than falls back
    --bitrate <MBPS>             target bitrate in Mbps (default: 20)
    --width <W> --height <H>     synthetic source size (default: 1920x1080)
    --out <PATH>                 raw output (default: the system temp dir)
    --no-loopback                skip the punktfunk_core round-trip verification
    --wire-chunk <BYTES>         PyroWave datagram-aligned packetization at this shard payload
                                 (a real session passes its negotiated shard_payload, e.g. 1408).
                                 With PUNKTFUNK_PYROWAVE_STREAMED_AU=1 also armed, the AU is
                                 drained through poll_chunk and sealed as a STREAMED wire frame
                                 (VIDEO_CAP_STREAMED_AU), then byte-verified by the loopback
    -h, --help                   this help

NOTES:
    'portal' needs headless Sway + xdg-desktop-portal-wlr running in this session
    (see design/linux-setup.md). 'synthetic' needs no capture session and always runs.
    Encoded AUs are written to a playable file AND (unless --no-loopback) fed through a
    punktfunk_core host→client loopback that reassembles and byte-verifies each one.
    Both 'serve' and 'punktfunk1-host' advertise the native service over mDNS
    (_punktfunk._udp) for client auto-discovery — 'punktfunk-probe --discover' lists them."
    );
    windows::entry::print_usage();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// A bad value is refused rather than replaced by the default.
    #[test]
    fn punktfunk1_host_refuses_a_bad_value() {
        let o = parse_punktfunk1(&args(&["--port", "9800", "--allow-tofu", "--no-mdns"])).unwrap();
        assert_eq!((o.port, o.require_pairing, o.mdns), (9800, false, false));
        assert!(matches!(o.source, native::Punktfunk1Source::Synthetic));
        for bad in [
            &["--port", "abc"][..],
            &["--seconds", "-1"],
            &["--source", "virtaul"],
            &["--source", "synthetic-abr", "--fill", "0"],
            &["--idle-timeout-ms", "0"],
            &["--pairing-pin", " "],
            &["--port"],
            &["--frobnicate"],
        ] {
            assert!(
                parse_punktfunk1(&args(bad)).is_err(),
                "{bad:?} was accepted"
            );
        }
        let abr = [
            "--source",
            "synthetic-abr",
            "--content",
            "idle-then-motion",
            "--fill",
            "50",
        ];
        let o = parse_punktfunk1(&args(&abr)).unwrap();
        let native::Punktfunk1Source::SyntheticAbr(shape) = o.source else {
            panic!("synthetic-abr source");
        };
        assert_eq!(
            (shape.idr_pct, shape.serve_ramp),
            (native::DEFAULT_IDR_PCT, true)
        );
    }
}
