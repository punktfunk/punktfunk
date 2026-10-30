//! `--browse [host[:port]]` — the console shell. Bare `--browse` opens the host list
//! (discovery, pairing, settings, wake — the whole couch flow); with a target it opens
//! straight into that host's library (the Decky per-host launch), B backing out to the
//! list in one press. A launches in the SAME window (no gamescope window handoff — the
//! whole point of one process), the session's end returns to the console, B at the root
//! quits to Gaming Mode.
//!
//! This file is the console's SERVICE side: the shell (pf-console-ui) renders and
//! raises [`ConsoleCmd`]s; worker threads here run everything that blocks — mDNS
//! discovery, reachability probes, the SPAKE2 pairing ceremony, wake-on-LAN loops,
//! library fetches, known-hosts persistence — and write results into the shared
//! models. `PUNKTFUNK_FAKE_LIBRARY=<file.json>` feeds canned entries with no host
//! (portrait paths starting with `/` load from disk), the GPU-only dev path.

// Windows counts `Instant` from boot, so `now - 60 s` panics in a console started at log-in.
#![warn(clippy::unchecked_time_subtraction)]

use crate::session_main::{
    arg_flag, arg_value, fullscreen_mode, parse_host_port, session_params, stats_tier, window_pos,
};
use pf_client_core::gamepad::is_steam_deck;
use pf_client_core::orchestrate::{
    self, emit, exit, SessionLine, WakeOutcome, WAKE_RESEND_SECS, WAKE_TIMEOUT_SECS,
};
use pf_client_core::{discovery, library, profiles, start, trust, wol};
use pf_console_ui::{
    ConsoleCmd, ConsoleEntry, ConsoleHandles, ConsoleOptions, ConsoleShared, HostRow, LibraryGame,
    LibraryPhase, LibraryShared, PairPhase, PairStatus, ProfilesAnswer, RequestKey, SkiaOverlay,
    SpeedPhase, WakeStatus,
};
use pf_presenter::overlay::OverlayAction;
use pf_presenter::ActionOutcome;
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A request-access connect awaiting the operator's approval on the host: stamped by the
/// launch handler and consumed by `on_connected`, which persists the host as paired.
struct PendingApproval {
    name: String,
    addr: String,
    port: u16,
    fp_hex: String,
}

/// Where the console opens, and what it seeds.
struct Entry {
    entry: ConsoleEntry,
    /// The window title's host, when a target named one.
    label: Option<String>,
    /// A target the store does not know, listed until the store or an advert covers it.
    seed: Option<HostRow>,
}

/// An explicit target wins: paired, it opens straight into its library; unpaired or unknown,
/// it lands on Home seeded into the list (one A from pairing). A bare launch asks the
/// start-screen policy, reading `settings` only then. The fake-library hook fabricates a
/// paired host with no network, and beats the policy too.
fn console_entry(
    target: Option<&str>,
    known: &trust::KnownHosts,
    fake: bool,
    settings: impl FnOnce() -> trust::Settings,
) -> Entry {
    match target {
        Some(target) => {
            let (addr, port) = parse_host_port(target);
            // `find_by_addr`, not a scan: a pinned record beats a placeholder saved at the
            // same address, and an address can carry more than one identity.
            let k = known.find_by_addr(&addr, port);
            let mut row = seed_row(k, &addr, port);
            row.paired |= fake;
            let label = Some(row.name.clone());
            let seed = k.is_none().then(|| row.clone());
            let entry = if row.paired {
                ConsoleEntry::Library(Box::new(row))
            } else {
                ConsoleEntry::Home
            };
            Entry { entry, label, seed }
        }
        None if fake => Entry {
            entry: ConsoleEntry::Library(Box::new(fake_host_row())),
            label: None,
            seed: None,
        },
        None => {
            let settings = settings();
            let (default, source) = start::default_host_with_source(&settings, known);
            tracing::info!(
                start_in = start::StartIn::parse(&settings.start_in).as_str(),
                default = default.map_or("none", |i| known.hosts[i].name.as_str()),
                source = source.as_str(),
                "console start"
            );
            let row = |i: usize| {
                let k = &known.hosts[i];
                Box::new(seed_row(Some(k), &k.addr, k.port))
            };
            let entry = match start::start_screen(&settings, known) {
                start::Start::Hosts => ConsoleEntry::Home,
                start::Start::Library(i) => ConsoleEntry::Library(row(i)),
                start::Start::Stream(i) => ConsoleEntry::Stream(row(i)),
            };
            Entry {
                entry,
                label: None,
                seed: None,
            }
        }
    }
}

/// The session let us in. A request-access connect that just succeeded means the operator
/// approved us: the host is saved as paired, keyed to the fingerprint we pinned. The host's
/// library port from its Welcome is recorded after that persist, so a host saved by this very
/// connect gets it too (`0` = none advertised, a no-op). A normal launch touches last-used only.
fn note_connected(pending: &Mutex<Option<PendingApproval>>, fingerprint: [u8; 32], mgmt_port: u16) {
    let fp_hex = trust::hex(&fingerprint);
    trust::touch_last_used(&fp_hex);
    if let Some(p) = pending.lock().unwrap().take() {
        if p.fp_hex == fp_hex {
            if let Err(e) = trust::persist_host(&p.name, &p.addr, p.port, &fp_hex, true, &[]) {
                tracing::warn!(error = %format!("{e:#}"), "saving the approved host");
            }
        }
    }
    trust::learn_mgmt_port_by_fp(&fp_hex, mgmt_port);
}

/// What the launch handler holds across actions: the console's identity for the session,
/// `--all-screens`, and the request-access hand-off slot [`note_connected`] reads.
struct Launcher {
    identity: (String, String),
    all_screens: bool,
    pending_approval: Arc<Mutex<Option<PendingApproval>>>,
}

impl Launcher {
    /// One overlay action. A launch becomes the session's params, re-resolved from the
    /// settings and the host's preset binding each time; the run-loop-side and Android-only
    /// actions are handled there, listed so adding an action to the enum keeps failing
    /// loudly here instead of falling into a wildcard that silently drops it.
    fn act(
        &self,
        action: OverlayAction,
        gamepad: &pf_client_core::gamepad::GamepadService,
        native: punktfunk_core::config::Mode,
        hdr: Option<punktfunk_core::quic::HdrMeta>,
        force_software: Arc<AtomicBool>,
        vulkan: Option<pf_client_core::video::VulkanDecodeDevice>,
    ) -> ActionOutcome {
        match action {
            OverlayAction::Launch {
                addr,
                port,
                fp_hex,
                launch,
                title,
                request_access,
                preset,
                profile,
            } => {
                let Some(pin) = trust::parse_hex32(&fp_hex) else {
                    // Connect (and request-access) pin the host's advertised fingerprint;
                    // a pinless launch is a logic slip, never a silent TOFU.
                    tracing::warn!(%addr, "launch without a stored pin — refusing");
                    return ActionOutcome::Handled;
                };
                tracing::info!(%addr, %title, request_access,
                    launch = launch.as_deref().unwrap_or("desktop"),
                    "launching from the console");
                // Re-resolved per launch, not latched: the settings screen may have moved
                // the defaults since the last stream, and the host may carry a preset
                // binding. A pinned card's one-off preset id wins over that binding, and a
                // dangling id falls back to the defaults instead of blocking the connect.
                let (settings, preset) = trust::effective_settings(
                    Some(&fp_hex),
                    &addr,
                    port,
                    preset.as_deref(),
                    launch.as_deref(),
                );
                let preset_id = preset.as_ref().map(|p| p.id.clone());
                let mut params = session_params(
                    &settings,
                    preset.map(|p| p.name),
                    preset_id,
                    // In-process launch: no spawner resolved a clipboard decision for us.
                    None,
                    addr.clone(),
                    port,
                    pin,
                    self.identity.clone(),
                    launch,
                    gamepad,
                    native,
                    hdr,
                    force_software,
                    vulkan,
                );
                params.profile = profile;
                params.shows_screens |= self.all_screens;
                if request_access {
                    // The host PARKS the connect until the operator approves — outlast its
                    // approval window (host `PENDING_APPROVAL_WAIT`), matching the desktop
                    // shells' 185 s. On success `note_connected` persists the host as paired.
                    params.connect_timeout = Duration::from_secs(185);
                    *self.pending_approval.lock().unwrap() = Some(PendingApproval {
                        name: title.clone(),
                        addr,
                        port,
                        fp_hex: fp_hex.clone(),
                    });
                }
                ActionOutcome::Start(Box::new(params))
            }
            OverlayAction::CancelConnect => ActionOutcome::Handled, // run-loop-side
            // Also run-loop-side: the clipboard belongs to SDL, which this handler has no
            // handle on.
            OverlayAction::CopyText(_) => ActionOutcome::Handled,
            // This console is drawn OVER the session's stream, so the hold coming down is
            // the shell's own business and the picture is already behind it.
            OverlayAction::ShowStream => ActionOutcome::Handled,
            // The console offers these on Android only.
            OverlayAction::PinShortcut { .. } | OverlayAction::SaveLauncherFile { .. } => {
                ActionOutcome::Handled
            }
            OverlayAction::Quit => ActionOutcome::Quit,
        }
    }
}

pub fn run(target: Option<&str>) -> u8 {
    // Publish the desktop's theme to the shell ("Follow system theme"), and keep it
    // current: a 2 s poll, the same interval and reason as the GTK shell and the web
    // console — `~/.local/state/omarchy/current` is a SYMLINK `omarchy-theme-set`
    // re-points, which a file monitor on the resolved path cannot follow. Off Omarchy the
    // one `present()` stat is the whole cost and no thread starts.
    #[cfg(target_os = "linux")]
    if pf_client_core::omarchy::present() {
        std::thread::spawn(|| loop {
            let t = pf_client_core::omarchy::current().map(|t| pf_console_ui::os_theme::OsTheme {
                light: !t.dark,
                background: t.bg,
                foreground: t.fg,
                accent: t.accent,
            });
            // The revision only moves on a real change, so the idle case is one file read.
            pf_console_ui::os_theme::set_os_theme(t);
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
    }
    // The desktop's reduce-motion switch, followed while it answers. A desktop that says
    // nothing starts no thread and keeps the console's own row.
    if let Some(reduce) = pf_client_core::os_prefs::reduce_motion() {
        pf_console_ui::os_theme::set_os_reduce_motion(Some(reduce));
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let reduce = pf_client_core::os_prefs::reduce_motion();
            pf_console_ui::os_theme::set_os_reduce_motion(reduce);
        });
    }
    let identity = match trust::load_or_create_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("client identity: {e:#}");
            return exit::CONNECT_FAILED;
        }
    };

    let fake = std::env::var_os("PUNKTFUNK_FAKE_LIBRARY").is_some();
    let known = trust::KnownHosts::load();
    let Entry {
        entry,
        label: window_label,
        seed,
    } = console_entry(target, &known, fake, trust::Settings::load);
    let opts = ConsoleOptions::desktop(trust::device_name(), is_steam_deck());
    let (overlay, handles) = match SkiaOverlay::console(opts, entry) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("console UI: {e:#}");
            return exit::RENDERER_FAILED;
        }
    };
    let ConsoleHandles {
        console,
        library: library_model,
        bus,
    } = handles;

    // The service loop: discovery, probes, wake, pairing, persistence, fetches.
    let service = Service::start(
        console.clone(),
        library_model.clone(),
        bus.clone(),
        identity.clone(),
        seed,
    );

    // `--json-status`: a shell parent is reading stdout (the WinUI shell hides itself on
    // `{"ready":true}` and restores on exit) — plain CLI/gamescope runs stay silent.
    let json_status = arg_flag("--json-status");
    // Seeds the window only. Every launch re-resolves settings and the preset binding, and the
    // presenter adopts that launch's `SessionParams`: no setting waits for a console restart.
    let settings_at_start = trust::Settings::load();

    // Request-access hand-off: the launch handler stamps this when it starts a delegated-approval
    // connect; `on_connected` reads it once the host lets us in and persists the host as PAIRED,
    // so the next connect is an ordinary one. `None` for every normal launch, so `on_connected`
    // then only touches last-used.
    let pending_approval: Arc<Mutex<Option<PendingApproval>>> = Arc::new(Mutex::new(None));
    let pending_cb = pending_approval.clone();

    let opts = pf_presenter::SessionOpts {
        window_title: window_label.map_or_else(
            || "Punktfunk".to_string(),
            |label| format!("Punktfunk · {label}"),
        ),
        fullscreen: fullscreen_mode(),
        window_pos: window_pos(),
        stats_verbosity: stats_tier(&settings_at_start),
        touch_mode: settings_at_start.touch_mode(),
        mouse_mode: settings_at_start.mouse_mode(),
        invert_scroll: settings_at_start.invert_scroll,
        inhibit_shortcuts: settings_at_start.inhibit_shortcuts,
        present_priority: settings_at_start.present_priority(),
        vsync: settings_at_start.vsync,
        allow_vrr: settings_at_start.allow_vrr,
        json_status,
        on_connected: Some(Box::new(move |fingerprint: [u8; 32], mgmt_port: u16| {
            note_connected(&pending_cb, fingerprint, mgmt_port)
        })),
        overlay: Some(Box::new(overlay)),
        window_size: crate::session_main::window_size(&settings_at_start),
        match_window: settings_at_start.match_window,
        // The console owns its own window across every launch, and no parent is listening to
        // its stdout, so it keeps persisting the size itself.
        on_window_size: crate::session_main::on_window_size(true),
        render_scale: settings_at_start.render_scale,
        render_scale_max_dim: punktfunk_core::render_scale::max_dimension(&settings_at_start.codec),
        video_fit: punktfunk_core::video_fit::VideoFit::from_name(&settings_at_start.video_fit),
        // The desktop shell opened this console because a controller connected.
        until_no_pads: arg_flag("--until-no-controller"),
    };

    let launcher = Launcher {
        identity,
        all_screens: arg_flag("--all-screens"),
        pending_approval,
    };
    let result = pf_presenter::run_browse(
        opts,
        |action, gamepad, native, hdr, force_software, vulkan| {
            launcher.act(action, gamepad, native, hdr, force_software, vulkan)
        },
    );

    service.stop();

    match result {
        Ok(()) => 0,
        Err(e) => {
            // The shell contract's terminal line (a clean quit needs none — stdout EOF
            // already routes the shell back to its host list silently).
            if json_status {
                emit(SessionLine::Error {
                    msg: &format!("{e:#}"),
                    trust_rejected: Some(false),
                    refused: None,
                });
            }
            eprintln!("console: {e:#}");
            exit::RENDERER_FAILED
        }
    }
}

/// A console row key → its index in the known-hosts store. The key is the pinned
/// fingerprint when there is one, else `addr:port` (see the row builder) — which names
/// the placeholder there, never a record pinned at that address. A pinned CARD's key
/// carries the preset id past a NUL, dropped here: a card sends the key it shows.
fn index_for_key(known: &trust::KnownHosts, key: &str) -> Option<usize> {
    let key = key.split('\0').next().unwrap_or(key);
    known
        .hosts
        .iter()
        .position(|h| !h.fp_hex.is_empty() && h.fp_hex == key)
        .or_else(|| {
            let (addr, port) = key.rsplit_once(':')?;
            known.placeholder_at(addr, port.parse().ok()?)
        })
}

fn host_display_name(name: &str, addr: &str) -> String {
    if name.trim().is_empty() {
        addr.to_string()
    } else {
        name.to_string()
    }
}

/// A carousel row for the host the console is opening on: a store record when we have one,
/// else a bare `addr:port` nobody has reached yet. Offline and action-less by construction —
/// the refresh tick fills both in once the host answers.
fn seed_row(k: Option<&trust::KnownHost>, addr: &str, port: u16) -> HostRow {
    HostRow {
        key: k
            .filter(|h| !h.fp_hex.is_empty())
            .map_or_else(|| format!("{addr}:{port}"), |h| h.fp_hex.clone()),
        id: k.and_then(|h| h.id.clone()),
        name: k
            .map(|h| host_display_name(&h.name, &h.addr))
            .unwrap_or_else(|| addr.to_string()),
        addr: addr.to_string(),
        port,
        fp_hex: k.map(|h| h.fp_hex.clone()).unwrap_or_default(),
        paired: k.is_some_and(|h| h.paired),
        saved: k.is_some(),
        online: false,
        // Explicit --mgmt wins; else the port this host's advert taught us and we saved;
        // else 47990. The middle rung is what survives mDNS being unavailable later.
        mgmt_port: arg_value("--mgmt")
            .and_then(|p| p.parse().ok())
            .or_else(|| k.and_then(|h| h.mgmt_port))
            .unwrap_or(library::DEFAULT_MGMT_PORT),
        can_wake: false,
        clipboard_sync: k.is_some_and(|h| h.clipboard_sync),
        last_used: k.and_then(|h| h.last_used),
        os: k.map(|h| h.os.clone()).unwrap_or_default(),
        actions: Vec::new(),
        pin: None,
        bound_preset: None,
        running: String::new(),
        game_presets: Default::default(),
        profile: k.and_then(|h| h.profile.clone()),
    }
}

fn fake_host_row() -> HostRow {
    HostRow {
        key: "fake".into(),
        name: "Demo Host".into(),
        addr: "127.0.0.1".into(),
        port: 9777,
        paired: true,
        saved: true,
        online: true,
        mgmt_port: library::DEFAULT_MGMT_PORT,
        os: "linux/arch/steamos".into(),
        ..Default::default()
    }
}

/// The background service: owns discovery, probing, waking, pairing and persistence.
struct Service {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Service {
    fn start(
        console: ConsoleShared,
        library_model: LibraryShared,
        bus: pf_console_ui::ConsoleBus,
        identity: (String, String),
        seed: Option<HostRow>,
    ) -> Service {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-console".into())
            .spawn(move || {
                ServiceState {
                    console,
                    library: library_model,
                    bus,
                    identity,
                    seed,
                    discovered: HashMap::new(),
                    probed: Arc::new(Mutex::new(HashMap::new())),
                    probe_inflight: Arc::new(AtomicBool::new(false)),
                    last_probe: None,
                    wake_cancel: None,
                    pair_gen: Arc::new(AtomicU64::new(0)),
                    rescan: None,
                    stores: Stores::default(),
                    learn_due: true,
                }
                .run(stop_w)
            })
            .ok();
        Service { stop, thread }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A worker thread a command started. The service never joins one; a test waits on it.
type Worker = Option<std::thread::JoinHandle<()>>;

struct ServiceState {
    console: ConsoleShared,
    library: LibraryShared,
    bus: pf_console_ui::ConsoleBus,
    identity: (String, String),
    /// A `--browse` target that isn't in the store yet — kept on the list until the
    /// store or discovery covers it.
    seed: Option<HostRow>,
    discovered: HashMap<String, discovery::DiscoveredHost>,
    /// Probe results by row key, written by sweep threads.
    probed: Arc<Mutex<HashMap<String, bool>>>,
    probe_inflight: Arc<AtomicBool>,
    /// When the last sweep ran; `None` runs one on the next tick. Never an instant in the
    /// past: Windows' clock starts at boot, and subtracting past it panics.
    last_probe: Option<Instant>,
    /// Cancels the active wake thread (it owns the model's wake status).
    wake_cancel: Option<Arc<AtomicBool>>,
    /// Bumped per pairing. A ceremony the player left still persists, but only the
    /// newest one writes the shared phase.
    pair_gen: Arc<AtomicU64>,
    /// Forces the mDNS browse to re-query. Installed by `run`; `None` before it starts.
    rescan: Option<discovery::Rescan>,
    stores: Stores,
    /// The adverts or the saved hosts moved since each saved host last learned from its advert.
    learn_due: bool,
}

/// The saved hosts and the preset catalog as last read. Each is read again only when its file
/// changed, so a rebuild with nothing new costs two `stat`s and no lock.
#[derive(Default)]
struct Stores {
    known: trust::KnownHosts,
    known_seen: trust::FileStamp,
    catalog: pf_client_core::presets::PresetsFile,
    catalog_seen: trust::FileStamp,
}

impl Stores {
    /// `true` when the saved hosts were read again.
    fn refresh(&mut self) -> bool {
        if let Some(c) =
            pf_client_core::presets::PresetsFile::load_if_changed(&mut self.catalog_seen)
        {
            self.catalog = c;
        }
        let Some(k) = trust::KnownHosts::load_if_changed(&mut self.known_seen) else {
            return false;
        };
        self.known = k;
        true
    }

    /// Read both again at the next `refresh`. A command writes the store, and its write can
    /// carry the timestamp of the read before it.
    fn forget(&mut self) {
        self.known_seen = trust::FileStamp::default();
        self.catalog_seen = trust::FileStamp::default();
    }
}

impl ServiceState {
    fn run(mut self, stop: Arc<AtomicBool>) {
        let (discovery_rx, rescan) = discovery::browse();
        self.rescan = Some(rescan);
        // The rows hold probe results and cached host state that change with no event, so they
        // are rebuilt on a floor as well as when something could have moved them. The stores
        // under them are read again only when a file changed.
        let mut dirty = true;
        let mut last_rows = Instant::now();
        while !stop.load(Ordering::SeqCst) {
            // mDNS churn.
            while let Ok(ev) = discovery_rx.try_recv() {
                dirty = true;
                self.on_discovery(ev);
            }

            // Shell commands (plus the binary's own seeded initial fetch).
            for cmd in self.bus.drain() {
                dirty = true;
                self.handle(cmd);
                self.stores.forget();
            }

            let probe_due = self
                .last_probe
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(10));
            if dirty || probe_due || last_rows.elapsed() >= Duration::from_millis(500) {
                self.publish_rows(probe_due);
                last_rows = Instant::now();
                dirty = false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(c) = &self.wake_cancel {
            c.store(true, Ordering::SeqCst);
        }
    }

    /// An advert came or went: the list moves, and each saved host may learn from it.
    fn on_discovery(&mut self, ev: discovery::DiscoveryEvent) {
        self.learn_due = true;
        match ev {
            discovery::DiscoveryEvent::Resolved(host) => {
                self.discovered.insert(host.fullname.clone(), host);
            }
            discovery::DiscoveryEvent::Removed { fullname } => {
                self.discovered.remove(&fullname);
            }
        }
    }

    /// Rebuild the list for the shell, and on a due tick run the 10 s reachability sweep —
    /// saved hosts that don't advertise (routed / multicast-filtered networks) still get honest
    /// presence pips. The sweep reuses this tick's list rather than building two more.
    fn publish_rows(&mut self, probe_due: bool) {
        let rows = self.rows();
        if std::mem::take(&mut self.learn_due) {
            self.learn_from_adverts();
        }
        if probe_due {
            self.last_probe = Some(Instant::now());
            self.sweep(&rows);
            self.refresh_host_state(&rows);
        }
        self.console.set_hosts(rows);
    }

    fn handle(&mut self, cmd: ConsoleCmd) {
        match cmd {
            ConsoleCmd::FetchLibrary {
                addr,
                port,
                mgmt,
                fp_hex,
                req,
            } => self.fetch_library(addr, port, mgmt, fp_hex, req),
            ConsoleCmd::RefreshRunning { addr, mgmt, fp_hex } => {
                self.refresh_running(addr, mgmt, fp_hex);
            }
            ConsoleCmd::FetchProfiles { addr, mgmt, fp_hex } => {
                self.fetch_profiles(addr, mgmt, fp_hex)
            }
            ConsoleCmd::FetchAvatar {
                addr,
                mgmt,
                fp_hex,
                url,
            } => self.fetch_avatar(addr, mgmt, fp_hex, url),
            ConsoleCmd::WakeProfile {
                addr,
                mgmt,
                fp_hex,
                id,
            } => self.wake_profile(addr, mgmt, fp_hex, id),
            ConsoleCmd::SetProfile { key, profile } => self.set_profile(key, profile),
            ConsoleCmd::SendLogs {
                addr,
                mgmt,
                fp_hex,
                host_name,
            } => self.send_logs(addr, mgmt, fp_hex, host_name),
            ConsoleCmd::SpeedTest {
                key,
                addr,
                port,
                fp_hex,
                host_name,
            } => self.speed_test(key, addr, port, fp_hex, host_name),
            ConsoleCmd::HostAction {
                addr,
                mgmt,
                fp_hex,
                host_name,
                action_id,
                label,
            } => self.host_action(addr, mgmt, fp_hex, host_name, action_id, label),
            ConsoleCmd::EndGame {
                addr,
                mgmt,
                fp_hex,
                app_id,
                title,
            } => {
                self.end_game(addr, mgmt, fp_hex, app_id, title);
            }
            ConsoleCmd::Install {
                addr,
                mgmt,
                fp_hex,
                app_id,
                title,
                action,
            } => {
                self.change_install(addr, mgmt, fp_hex, app_id, title, action);
            }
            ConsoleCmd::Pair {
                addr,
                port,
                pin,
                device_name,
                req,
            } => self.run_pairing(
                addr,
                port,
                req,
                trust::pair_error_message,
                move |a, p, id| trust::pair_with_host(a, p, id, &pin, &device_name),
            ),
            ConsoleCmd::RequestAccess {
                addr,
                port,
                fp_hex,
                device_name,
                req,
            } => {
                let pin = trust::parse_hex32(&fp_hex);
                self.run_pairing(
                    addr,
                    port,
                    req,
                    trust::access_error_message,
                    move |a, p, id| {
                        trust::request_access_to_host(a, p, id, pin, &device_name, None)
                    },
                )
            }
            ConsoleCmd::SaveHost { name, addr, port } => self.save_host(name, addr, port),
            ConsoleCmd::UpdateHost {
                key,
                name,
                addr,
                port,
            } => self.update_host(key, name, addr, port),
            ConsoleCmd::ForgetHost { key } => self.forget_host(key),
            ConsoleCmd::UnpairHost { key } => self.unpair_host(key),
            ConsoleCmd::Wake {
                key,
                then_connect,
                req,
            } => self.wake(key, then_connect, req),
            // The shell closed the card as it sent this; a `Wake` behind it in the same drain
            // has already raised the next one.
            ConsoleCmd::CancelWake => {
                if let Some(c) = self.wake_cancel.take() {
                    c.store(true, Ordering::SeqCst);
                }
            }
            ConsoleCmd::Probe => {
                self.last_probe = None;
                // Home's Rescan tile. It re-queries mDNS too: the browse lives as long as the
                // process and `mdns-sd` backs its re-query off to an hour, so a host that
                // appeared since startup may otherwise never be asked for.
                if let Some(r) = &self.rescan {
                    r.request();
                }
            }
            // Grants and rumble tests from the controllers screen, Android-only: the settings row
            // that opens that screen is not on the desktop's list.
            ConsoleCmd::PadAction { .. } => {}
            // The Controllers tab offers no input test on the desktop.
            ConsoleCmd::PadTest { .. } => {}
            // Only a host that raised a prompt hears its answer; the desktop raises none.
            ConsoleCmd::PromptAnswer { .. } => {}
            // The console reads the catalog straight from this file, so a save is the whole job.
            ConsoleCmd::SavePreset {
                id,
                name,
                overrides,
            } => save_preset(id, name, overrides),
            ConsoleCmd::DeletePreset { id } => {
                let mut file = pf_client_core::presets::PresetsFile::load();
                file.presets.retain(|p| p.id != id);
                if let Err(e) = file.save() {
                    tracing::warn!(error = %e, "preset did not delete");
                }
            }
            // The notices this build ships beside it, compiled in: an installed session has
            // no reliable path to the file.
            ConsoleCmd::LoadLicenses => {
                #[cfg(windows)]
                const NOTICES: &str = include_str!("../../windows/THIRD-PARTY-NOTICES.txt");
                #[cfg(not(windows))]
                const NOTICES: &str = include_str!("../../linux/THIRD-PARTY-NOTICES.txt");
                self.console
                    .set_licenses(vec![pf_console_ui::LicenseSection {
                        heading: "Third-party software".into(),
                        text: NOTICES.into(),
                    }]);
            }
            ConsoleCmd::SetPin {
                key,
                preset_id,
                pin,
            } => self.set_pin(key, preset_id, pin),
            ConsoleCmd::BindPreset {
                key,
                game,
                preset_id,
            } => self.bind_preset(key, game, preset_id),
            ConsoleCmd::SetClipboard { key, on } => self.set_clipboard(key, on),
        }
    }

    fn fetch_library(&self, addr: String, port: u16, mgmt: u16, fp_hex: String, req: RequestKey) {
        // Opening a library is the earliest signal that somebody intends to play, so the box
        // is woken HERE, not at connect time, with the MACs of the record the pin names, else
        // of the placeholder at `addr:port`. Empty with auto-wake off or no MAC on record: the
        // fetch then asks once instead of retrying across a boot.
        let known = trust::KnownHosts::load();
        let macs = known
            .resolve(Some(&fp_hex), &addr, port)
            .filter(|_| trust::Settings::load().auto_wake)
            .map(|h| h.mac.clone())
            .unwrap_or_default();
        spawn_fetch(
            self.library.clone(),
            addr,
            (port, mgmt),
            self.identity.clone(),
            fp_hex,
            macs,
            req,
        );
    }

    fn refresh_running(&self, addr: String, mgmt: u16, fp_hex: String) -> Worker {
        // The carousel behind the shelf reads a different cache for the same fact;
        // dropping it here is what stops a tile advertising the game just quit.
        library::invalidate_running(&fp_hex);
        // Blocking network on a worker, like every other command here: the service
        // loop's own host refresh must keep running while a just-ended stream's host
        // is asked what it still has up.
        let shared = self.library.clone();
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        // A newer fetch owns the model by the time a slow host answers: its titles are not
        // this host's to badge.
        let epoch = shared.fetch_epoch();
        std::thread::Builder::new()
            .name("punktfunk-running".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                let status = library::fetch_status(&addr, mgmt, port, &identity, pin);
                if shared.fetch_epoch() == epoch {
                    shared.set_downloads(&status.downloads, status.grants);
                    shared.set_running(&status.games);
                }
            })
            .ok()
    }

    /// The box's profiles for the picker, off the service thread like every fetch here.
    fn fetch_profiles(&self, addr: String, mgmt: u16, fp_hex: String) {
        let console = self.console.clone();
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        std::thread::Builder::new()
            .name("punktfunk-profiles".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                let answer = match profiles::fetch_enumerate(&addr, mgmt, port, &identity, pin) {
                    Ok(Some(listed)) => ProfilesAnswer::Listed(listed),
                    Ok(None) => ProfilesAnswer::NoProfiles,
                    Err(e) => {
                        tracing::info!(%addr, error = %e, "profile list did not load");
                        ProfilesAnswer::Failed(e.to_string())
                    }
                };
                console.set_profiles(&fp_hex, answer);
            })
            .ok();
    }

    /// A profile's picture for the faces the shell draws.
    fn fetch_avatar(&self, addr: String, mgmt: u16, fp_hex: String, url: String) {
        let console = self.console.clone();
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        std::thread::Builder::new()
            .name("punktfunk-avatar".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                let api = library::HostApi::new(&addr, mgmt, port, &identity, pin);
                if let Some(bytes) = profiles::fetch_avatar(&api, &url) {
                    console.set_avatar(&url, bytes);
                }
            })
            .ok();
    }

    /// Starts a profile's stopped seat. The shell polls the list for `ready` itself.
    fn wake_profile(&self, addr: String, mgmt: u16, fp_hex: String, id: String) {
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        std::thread::Builder::new()
            .name("punktfunk-wake-seat".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                if let Err(e) = profiles::wake(&addr, mgmt, port, &identity, pin, &id) {
                    tracing::info!(%addr, error = %e, "profile seat did not wake");
                }
            })
            .ok();
    }

    fn send_logs(&self, addr: String, mgmt: u16, fp_hex: String, host_name: String) {
        // Blocking network (5 s connect / 10 s global, the library agent's budgets) —
        // a worker thread keeps the service loop's host refresh alive meanwhile. The
        // result lands as a shared-model notice; the shell toasts it on its next sync.
        let identity = self.identity.clone();
        let console = self.console.clone();
        std::thread::Builder::new()
            .name("punktfunk-sendlogs".into())
            .spawn(move || {
                console.set_notice(pf_client_core::logring::send_bundle(
                    "punktfunk-session",
                    &host_name,
                    &addr,
                    (mgmt, stream_port(&fp_hex)),
                    &identity,
                    &fp_hex,
                ));
            })
            .ok();
    }

    fn speed_test(&self, key: String, addr: String, port: u16, fp_hex: String, host_name: String) {
        // A worker like every other command here, but a long one: the probe opens
        // its own session and bursts for two seconds. The shell already raised the
        // takeover, so this only advances the phase and feeds its graph.
        let identity = self.identity.clone();
        let console = self.console.clone();
        std::thread::Builder::new()
            .name("punktfunk-speedtest".into())
            .spawn(move || {
                console.advance_speed(&key, SpeedPhase::Measuring);
                let fp = (!fp_hex.is_empty()).then_some(fp_hex.as_str());
                let progress = |kbps| console.advance_speed(&key, SpeedPhase::Progress { kbps });
                // The whole check, not just the speed: the takeover lists what it found.
                let run = pf_client_core::speed::run_network_check_with;
                match run(&addr, port, fp, identity, progress) {
                    Ok(r) => {
                        tracing::info!(
                            host = %host_name,
                            ceiling_kbps = r.speed.ceiling_kbps,
                            wall = r.speed.wall,
                            clean_loss = r.speed.clean.map(|c| c.loss_pct),
                            findings = ?r.findings.iter().map(|f| f.id).collect::<Vec<_>>(),
                            "network check finished"
                        );
                        console.advance_speed(
                            &key,
                            SpeedPhase::Done {
                                throughput_kbps: r.speed.ceiling_kbps,
                                wall: r.speed.wall,
                                clean: r.speed.clean.map(|c| pf_console_ui::model::CleanRound {
                                    rate_kbps: c.rate_kbps,
                                    loss_pct: c.loss_pct,
                                    jitter_us: c.jitter_us,
                                }),
                                recommended_kbps: pf_client_core::speed::recommended_kbps(
                                    r.speed.ceiling_kbps,
                                ),
                                findings: r
                                    .findings
                                    .iter()
                                    .map(|f| pf_console_ui::model::FindingRow {
                                        id: f.id as u8,
                                        severity: f.severity as u8,
                                        numbers: f.numbers,
                                    })
                                    .collect(),
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(host = %host_name, error = %e, "speed test failed");
                        console.advance_speed(&key, SpeedPhase::Failed(e));
                    }
                }
            })
            .ok();
    }

    fn host_action(
        &self,
        addr: String,
        mgmt: u16,
        fp_hex: String,
        host_name: String,
        action_id: String,
        label: String,
    ) {
        // Same lane and budgets as SendLogs above, and the same worker-thread reason.
        // The tile goes dark on its own once the host acts.
        let identity = self.identity.clone();
        let console = self.console.clone();
        std::thread::Builder::new()
            .name("punktfunk-hostaction".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                console.set_notice(pf_client_core::host_actions::run(
                    &host_name, &addr, mgmt, port, &identity, &fp_hex, &action_id, &label,
                ));
            })
            .ok();
    }

    fn end_game(
        &self,
        addr: String,
        mgmt: u16,
        fp_hex: String,
        app_id: String,
        title: String,
    ) -> Worker {
        // Same worker-thread reason as `refresh_running`; the re-read after it is what
        // takes the Resume badge off the poster.
        library::invalidate_running(&fp_hex);
        let shared = self.library.clone();
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        let console = self.console.clone();
        // As in `refresh_running`: the player may be on another host's shelf by the answer.
        let epoch = shared.fetch_epoch();
        std::thread::Builder::new()
            .name("punktfunk-endgame".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                let outcome = library::end_game(&addr, mgmt, port, &identity, pin, &app_id);
                tracing::info!(app = %app_id, ?outcome, "end game");
                console.set_notice(outcome.notice(&title));
                let running = library::fetch_running(&addr, mgmt, port, &identity, pin);
                if shared.fetch_epoch() == epoch {
                    shared.set_running(&running);
                }
            })
            .ok()
    }

    fn change_install(
        &self,
        addr: String,
        mgmt: u16,
        fp_hex: String,
        app_id: String,
        title: String,
        action: library::InstallAction,
    ) -> Worker {
        let shared = self.library.clone();
        let identity = self.identity.clone();
        let pin = trust::parse_hex32(&fp_hex);
        let console = self.console.clone();
        let epoch = shared.fetch_epoch();
        std::thread::Builder::new()
            .name("punktfunk-install".into())
            .spawn(move || {
                let port = stream_port(&fp_hex);
                let outcome =
                    library::change_install(&addr, mgmt, port, &identity, pin, &app_id, action);
                tracing::info!(app = %app_id, ?action, ?outcome, "title install");
                console.set_notice(outcome.notice(action, &title));
                let status = library::fetch_status(&addr, mgmt, port, &identity, pin);
                if shared.fetch_epoch() == epoch {
                    if outcome == library::InstallOutcome::Done
                        && action == library::InstallAction::Remove
                    {
                        shared.set_installed(&app_id, false);
                    }
                    shared.set_downloads(&status.downloads, status.grants);
                    shared.set_running(&status.games);
                }
            })
            .ok()
    }

    /// Run one pairing (`ask`: the PIN ceremony or a request access) on its own thread and
    /// report it under `req` as Busy, then Paired or Failed. A later pairing makes this
    /// answer stale.
    fn run_pairing(
        &mut self,
        addr: String,
        port: u16,
        req: RequestKey,
        wording: fn(&punktfunk_core::PunktfunkError) -> String,
        ask: impl FnOnce(&str, u16, &(String, String)) -> Result<[u8; 32], punktfunk_core::PunktfunkError>
            + Send
            + 'static,
    ) {
        // What the list calls each identity at this address (advert or store), picked
        // once the ceremony says which one answered: both OS installs of a dual-boot
        // box sit here, and the first row is not necessarily this one.
        let named: Vec<(String, String)> = self
            .rows()
            .into_iter()
            .filter(|r| r.addr == addr && r.port == port)
            .map(|r| (r.fp_hex, r.name))
            .collect();
        let console = self.console.clone();
        let say = move |phase| {
            console.set_pair(PairStatus {
                req: req.clone(),
                phase,
            })
        };
        say(PairPhase::Busy);
        let identity = self.identity.clone();
        let generation = self.pair_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let pair_gen = self.pair_gen.clone();
        std::thread::Builder::new()
            .name("punktfunk-pair".into())
            .spawn(move || {
                let current = || pair_gen.load(Ordering::SeqCst) == generation;
                match ask(&addr, port, &identity) {
                    Ok(fp) => {
                        let fp_hex = trust::hex(&fp);
                        let name = named
                            .iter()
                            .find(|(f, _)| *f == fp_hex)
                            .or_else(|| named.iter().find(|(f, _)| f.is_empty()))
                            .map_or_else(|| addr.clone(), |(_, n)| n.clone());
                        let paired = trust::save_paired(&name, &addr, port, fp, &[]);
                        if !current() {
                            return;
                        }
                        match paired.saved {
                            Ok(()) => say(PairPhase::Paired { key: paired.fp_hex }),
                            // Not "Paired": the pin is gone by the next launch.
                            Err(e) => say(PairPhase::Failed(format!(
                                "Paired, but couldn't save — {e:#}"
                            ))),
                        }
                    }
                    Err(e) => {
                        // Cause-specific wording (wrong PIN vs not-armed vs unreachable
                        // vs a typed host rejection) — shared with every other surface.
                        if current() {
                            say(PairPhase::Failed(wording(&e)));
                        }
                    }
                }
            })
            .ok();
    }

    /// A manual entry has no pin: it edits the placeholder at its address or files one. A
    /// record pinned there is another identity and keeps its name.
    fn save_host(&mut self, name: String, addr: String, port: u16) {
        let edit = trust::HostEdit {
            name: Some(name),
            addr: Some(addr),
            port: Some(port),
            ..Default::default()
        };
        let mut added = Ok(trust::AddOutcome::Unchanged);
        let saved = trust::KnownHosts::update_if(|known| {
            added = known.add(&edit);
            added.is_ok().then_some(())
        });
        if let Err(e) = added {
            tracing::warn!(error = %format!("{e:#}"), "typed host not saved");
            return;
        }
        if let Err(e) = saved {
            self.save_failed(&e);
        }
        self.last_probe = None; // probe it now
    }

    /// Edited in place, never removed and re-added: the pin, the learned MACs, the pinned
    /// cards and the preset binding hang off this record. A blank name is no edit.
    fn update_host(&mut self, key: String, name: String, addr: String, port: u16) {
        let edit = trust::HostEdit {
            name: Some(name),
            addr: Some(addr),
            port: Some(port),
            ..Default::default()
        };
        let found = self.edit_host(&key, "edit", |h| {
            h.apply_edit(&edit);
            true
        });
        if found {
            self.last_probe = None; // the address moved
        }
    }

    fn forget_host(&mut self, key: String) {
        match orchestrate::forget_host(|known| index_for_key(known, &key)) {
            Ok(Some(gone)) => {
                tracing::info!(name = %gone.name, addr = %gone.addr, "host forgotten")
            }
            Ok(None) => {
                tracing::warn!(%key, "forget for an unknown host — ignoring");
                return;
            }
            Err(e) => self.save_failed(&e),
        }
        // It may still be advertising, in which case it comes straight back as a
        // DISCOVERED row — unsaved and unpaired, which is the honest state.
        self.last_probe = None;
    }

    fn unpair_host(&self, key: String) {
        match orchestrate::unpair_host(|known| index_for_key(known, &key)) {
            Ok(Some(host)) => tracing::info!(name = %host.name, "host unpaired"),
            Ok(None) => tracing::warn!(%key, "unpair for an unknown host — ignoring"),
            Err(e) => self.save_failed(&e),
        }
    }

    fn wake(&mut self, key: String, then_connect: bool, req: RequestKey) {
        if let Some(c) = self.wake_cancel.take() {
            c.store(true, Ordering::SeqCst);
        }
        let Some(row) = self.rows().into_iter().find(|r| r.key == key) else {
            return;
        };
        let known = trust::KnownHosts::load();
        let macs = index_for_key(&known, &row.key)
            .map(|i| known.hosts[i].mac.clone())
            .unwrap_or_default();
        if macs.is_empty() {
            // Nothing to send: end the takeover the shell opened for this wake.
            self.console.set_wake(Some(WakeStatus {
                req,
                key: row.key,
                name: row.name,
                seconds: 0,
                timed_out: true,
                online: false,
                then_connect,
            }));
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.wake_cancel = Some(cancel.clone());
        spawn_wake(self.console.clone(), row, macs, then_connect, req, cancel);
    }

    fn set_pin(&self, key: String, preset_id: String, pin: bool) {
        // Presentation only (design §5.2a): order = card order, appended at the
        // end; never touches `preset_id` (the default binding). Idempotent, so
        // a repeated press inside one refresh window can't double-pin.
        self.edit_host(&key, "pin toggle", |h| {
            if pin && !h.pinned_presets.contains(&preset_id) {
                h.pinned_presets.push(preset_id);
            } else if !pin {
                h.pinned_presets.retain(|id| *id != preset_id);
            }
            true
        });
        // `run` refreshes the rows right after this drain, so the carousel and
        // the pin screen reflect the new card within the same service pass.
    }

    fn bind_preset(&self, key: String, game: Option<String>, preset_id: Option<String>) {
        // The BINDING half of the preset pair — `KnownHost::preset_id` for the
        // host, `game_presets` for one title. `SetPin` above is the presentation
        // half and never touches either; this never touches the pins. Same store
        // discipline, same refresh-after-drain.
        self.edit_host(&key, "preset bind", |h| match &game {
            Some(id) => {
                let moved = h.preset_for_game(id) != preset_id.as_deref();
                h.bind_game_preset(id, preset_id.as_deref());
                moved
            }
            None => {
                let moved = h.preset_id != preset_id;
                h.preset_id = preset_id;
                moved
            }
        });
    }

    /// The profile this device plays as on a saved host (`KnownHost::profile`). Same store
    /// discipline as `set_pin`; the next rows carry it to the card.
    fn set_profile(&self, key: String, profile: Option<profiles::ProfilePick>) {
        self.edit_host(&key, "profile pick", |h| {
            let changed = h.profile != profile;
            h.profile = profile;
            changed
        });
    }

    fn set_clipboard(&self, key: String, on: bool) {
        // Per-host clipboard trust (`KnownHost::clipboard_sync`) — the host
        // menu's toggle. Same store discipline as `set_pin`.
        self.edit_host(&key, "clipboard toggle", |h| {
            let changed = h.clipboard_sync != on;
            h.clipboard_sync = on;
            changed
        });
    }

    /// One parallel reachability pass over every row. Advertising ones are NOT online by
    /// definition — an advert is a cache entry with a 75-minute TTL that a suspending host sends
    /// no goodbye for, so skipping them left a sleeping machine reading Online (and, since the
    /// wake item is gated on `!online`, unwakeable). A saved host found at an address it left
    /// moves back there. Runs on its own thread; at most one in flight.
    fn sweep(&self, rows: &[HostRow]) {
        if self.probe_inflight.swap(true, Ordering::SeqCst) {
            return;
        }
        let known = trust::KnownHosts::load();
        let (keys, hosts): (Vec<String>, Vec<trust::KnownHost>) = rows
            .iter()
            .map(|r| {
                // A discovered-only row has no addresses to fall back on.
                let prev_addrs = known
                    .hosts
                    .iter()
                    .find(|h| r.saved && !r.fp_hex.is_empty() && h.fp_hex == r.fp_hex)
                    .map(|h| h.prev_addrs.clone())
                    .unwrap_or_default();
                let host = trust::KnownHost {
                    addr: r.addr.clone(),
                    port: r.port,
                    fp_hex: r.fp_hex.clone(),
                    prev_addrs,
                    ..Default::default()
                };
                (r.key.clone(), host)
            })
            .unzip();
        let probed = self.probed.clone();
        let inflight = self.probe_inflight.clone();
        std::thread::Builder::new()
            .name("punktfunk-probe".into())
            .spawn(move || {
                let results = trust::probe_known(&hosts, Duration::from_millis(900));
                let mut map = probed.lock().unwrap();
                for (key, ok) in keys.into_iter().zip(results) {
                    map.insert(key, ok);
                }
                inflight.store(false, Ordering::SeqCst);
            })
            .ok();
    }

    /// Apply `f` to the record `key` names, in the store as it is under its lock, and save
    /// when `f` says it changed. `what` names the edit in the log when the key matches no
    /// record. `true` when it matched one.
    fn edit_host(
        &self,
        key: &str,
        what: &str,
        f: impl FnOnce(&mut trust::KnownHost) -> bool,
    ) -> bool {
        let mut found = false;
        let saved = trust::KnownHosts::update_if(|known| {
            let Some(h) = index_for_key(known, key).and_then(|i| known.hosts.get_mut(i)) else {
                tracing::warn!(%key, "{what} for an unknown host — ignoring");
                return None;
            };
            found = true;
            f(h).then_some(())
        });
        if let Err(e) = saved {
            self.save_failed(&e);
        }
        found
    }

    /// SAY so when a host-store write fails. The console is the only surface the user has
    /// here — a warning in the log reaches nobody on a TV — and a dropped write means the
    /// change they just made is not on disk.
    fn save_failed(&self, e: &impl std::fmt::Display) {
        tracing::warn!(error = %format!("{e:#}"), "saving known hosts");
        self.console.set_notice(format!("Couldn't save — {e:#}"));
    }

    /// Keep every paired, reachable host's advertised actions and running title fresh (the
    /// shared TTL'd caches in `pf_client_core`). Idempotent and cheap — each only reaches the
    /// network when its own entry has lapsed, and the running one lapses far sooner: what a
    /// host has UP is what changes between two visits to the carousel.
    fn refresh_host_state(&self, rows: &[HostRow]) {
        for r in rows {
            if r.paired && r.online && r.pin.is_none() {
                pf_client_core::host_actions::refresh(&r.addr, r.mgmt_port, r.port, &r.fp_hex);
                library::refresh_running(&r.addr, r.mgmt_port, r.port, &r.fp_hex);
            }
        }
    }

    /// The console home's rows, from the stores as the last `refresh` left them.
    fn rows(&mut self) -> Vec<HostRow> {
        self.learn_due |= self.stores.refresh();
        let probed = self.probed.lock().unwrap();
        build_rows(
            &self.stores.known,
            &self.stores.catalog,
            &self.discovered,
            &probed,
            self.seed.as_ref(),
        )
    }

    /// What each saved host's advert teaches it, while the advert is visible: name, mgmt port,
    /// OS chain, wake MAC (a Deck in Gaming Mode runs only this console and the Decky panel),
    /// and an address the sweep asks; the card moves only once its pin answers there.
    fn learn_from_adverts(&self) {
        for h in &self.stores.known.hosts {
            let advert = self
                .discovered
                .values()
                .find(|d| discovery::same_host(h, d));
            if let Some(a) = advert {
                trust::learn_from_advert(&h.fp_hex, &h.addr, h.port, a);
            }
        }
    }
}

/// The console home's rows: saved hosts in store order, each followed by its pinned
/// preset cards (design §5.2a), then discovered-but-unsaved ones by name, then a
/// still-uncovered `--browse` seed. `home::arrange` applies the player's sort; the Apple
/// and Android producers send the same order (`clients/shared/host-row-vectors.json`).
fn build_rows(
    known: &trust::KnownHosts,
    catalog: &pf_client_core::presets::PresetsFile,
    discovered: &HashMap<String, discovery::DiscoveredHost>,
    probed: &HashMap<String, bool>,
    seed: Option<&HostRow>,
) -> Vec<HostRow> {
    let chip = |p: &pf_client_core::presets::StreamPreset| pf_console_ui::PresetChip {
        id: p.id.clone(),
        name: p.name.clone(),
        accent: p.accent.clone(),
        // Only the speed test reads this: a preset that PINS bitrate is the layer its
        // host streams at, so the console must not offer to write the global instead.
        bitrate_kbps: p.overrides.bitrate_kbps,
    };
    let mut rows: Vec<HostRow> = known
        .hosts
        .iter()
        .flat_map(|h| {
            let key = if h.fp_hex.is_empty() {
                format!("{}:{}", h.addr, h.port)
            } else {
                h.fp_hex.clone()
            };
            let advert = discovered.values().find(|d| discovery::same_host(h, d));
            let online = probed.get(&key).copied().unwrap_or(false);
            let row = HostRow {
                key: key.clone(),
                id: h.id.clone(),
                name: host_display_name(&h.name, &h.addr),
                addr: h.addr.clone(),
                port: h.port,
                fp_hex: h.fp_hex.clone(),
                paired: h.paired,
                saved: true,
                online,
                // Live advert first, then what we saved from an earlier one, then 47990 —
                // the same three rungs `os` uses just below. Reading the advert ALONE is why
                // a host on a moved mgmt port lost its library the moment mDNS went quiet.
                mgmt_port: advert
                    .and_then(|d| d.mgmt_port)
                    .or(h.mgmt_port)
                    .unwrap_or(library::DEFAULT_MGMT_PORT),
                can_wake: !online && !h.mac.is_empty(),
                clipboard_sync: h.clipboard_sync,
                last_used: h.last_used,
                os: advert
                    .filter(|d| !d.os.is_empty())
                    .map(|d| d.os.clone())
                    .unwrap_or_else(|| h.os.clone()),
                // Whatever this host last told us it lets this device do to it. Empty
                // until the first refresh answers, and empty forever for a host that has
                // no such route or a device without the grant — the menu simply has no
                // power rows then.
                actions: pf_client_core::host_actions::cached(&h.fp_hex)
                    .into_iter()
                    .map(|a| pf_console_ui::HostAction {
                        label: a.label().to_string(),
                        id: a.id,
                        danger: a.danger,
                        available: a.available,
                        unavailable_reason: a.unavailable_reason.unwrap_or_default(),
                    })
                    .collect(),
                pin: None,
                bound_preset: h
                    .preset_id
                    .as_deref()
                    .and_then(|id| catalog.find_by_id(id))
                    .map(chip),
                // What this host last said it has up, from the same TTL'd cache the
                // actions come from. Empty until the first refresh answers — and for
                // an unpaired host, which has nothing to authenticate the ask with.
                running: library::now_playing(&h.fp_hex),
                // Ids straight through, dangling ones included: the bind screen only
                // compares, and a deleted preset falls back at resolve, not here.
                game_presets: h.game_presets.clone(),
                profile: h.profile.clone(),
            };
            // A pinned card shares the primary tile's live state; its own key lets
            // cursor-follow and the wake path address the card itself.
            let pins = h
                .resolved_pins(catalog)
                .into_iter()
                .map(|p| HostRow {
                    key: pf_console_ui::model::pinned_key(&key, &p.id),
                    pin: Some(chip(p)),
                    bound_preset: None,
                    ..row.clone()
                })
                .collect::<Vec<_>>();
            std::iter::once(row).chain(pins)
        })
        .collect();

    let mut extra: Vec<HostRow> = discovered
        .values()
        .filter(|d| !known.hosts.iter().any(|h| discovery::same_host(h, d)))
        .map(|d| {
            let key = if d.fp_hex.is_empty() {
                format!("{}:{}", d.addr, d.port)
            } else {
                d.fp_hex.clone()
            };
            let online = probed.get(&key).copied().unwrap_or(false);
            HostRow {
                key,
                // Discovered, not saved: no store record, so no id to point at.
                id: None,
                name: host_display_name(&d.name, &d.addr),
                addr: d.addr.clone(),
                port: d.port,
                fp_hex: d.fp_hex.clone(),
                paired: false,
                saved: false,
                // The probe, as for saved rows: an advert outlives a host that went to sleep.
                online,
                mgmt_port: d.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
                can_wake: false,
                clipboard_sync: false,
                last_used: None,
                os: d.os.clone(),
                // Discovered but unsaved: not paired, so there is nothing it would let us
                // do, and no identity to ask what it is running.
                actions: Vec::new(),
                pin: None,
                bound_preset: None,
                running: String::new(),
                game_presets: Default::default(),
                profile: None,
            }
        })
        .collect();
    extra.sort_by_key(|h| h.name.to_lowercase());
    rows.extend(extra);

    if let Some(seed) = seed {
        if !rows
            .iter()
            .any(|r| r.addr == seed.addr && r.port == seed.port)
        {
            let mut seed = seed.clone();
            seed.online = probed.get(&seed.key).copied().unwrap_or(false);
            rows.push(seed);
        }
    }
    rows
}

/// The wake-and-wait loop (one per wake): [`orchestrate::wake_by_probe`], the cadence every
/// shell shares. The thread reports each tick under `req`; the shell reads
/// `online`/`timed_out` and acts.
fn spawn_wake(
    console: ConsoleShared,
    row: HostRow,
    macs: Vec<String>,
    then_connect: bool,
    req: RequestKey,
    cancel: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("punktfunk-wake".into())
        .spawn(move || {
            orchestrate::wake_by_probe(&row.addr, row.port, &row.fp_hex, &macs, |tick| {
                // A cancelled thread stops; a tick already in flight lands on no card, as the
                // shell closed this wake's before it sent `CancelWake`.
                if cancel.load(Ordering::SeqCst) {
                    return false;
                }
                // Awake → the shell connects and cancels; timed out → the card waits for Try
                // Again / Cancel. Either ends this thread; a retry spawns a fresh one.
                console.set_wake(Some(WakeStatus {
                    req: req.clone(),
                    key: row.key.clone(),
                    name: row.name.clone(),
                    seconds: tick.seconds as u32,
                    timed_out: tick.outcome == Some(WakeOutcome::TimedOut),
                    online: tick.outcome == Some(WakeOutcome::Online),
                    then_connect,
                }));
                true
            });
        })
        .ok();
}

/// How often to ask a host we have just sent a magic packet to, across the whole
/// [`WAKE_TIMEOUT_SECS`] a cold box may take to POST and start serving.
const WAKE_RETRY_EVERY: Duration = Duration::from_secs(5);

fn save_preset(id: String, name: String, overrides: serde_json::Value) {
    let mut file = pf_client_core::presets::PresetsFile::load();
    let overrides = serde_json::from_value(overrides).unwrap_or_default();
    match file.presets.iter_mut().find(|p| p.id == id) {
        Some(p) => {
            p.name = name;
            p.overrides = overrides;
        }
        None => {
            let mut p = pf_client_core::presets::StreamPreset::new(name);
            p.id = id;
            p.overrides = overrides;
            file.presets.push(p);
        }
    }
    if let Err(e) = file.save() {
        tracing::warn!(error = %e, "preset did not save");
    }
}

/// The stream port of the saved host `fp_hex` pins, where its management API answers without
/// TCP. 0 with no such record: HTTPS alone. Reads the hosts file, so call it on a worker.
fn stream_port(fp_hex: &str) -> u16 {
    trust::KnownHosts::load()
        .find_by_fp(fp_hex)
        .map_or(0, |h| h.port)
}

/// Fetch the library off the service thread, then stream poster art into the shared
/// model as results land (the renderer drains `push_art` per frame). `(port, mgmt)`: the
/// stream port the management tunnel tries first, then the HTTPS port.
///
/// Three things happen before the host is ever asked, and the order is the point:
/// 1. the CACHED catalog goes up immediately, marked stale — a library is the screen a player
///    uses to decide what to play, and an empty one while a sleeping box boots is the opposite
///    of useful;
/// 2. given `macs` (auto-wake on), a magic packet goes out so the box warms while they choose;
/// 3. only then does the live fetch start, retrying across the boot window.
///
/// A cached catalog also outranks a failure: if the host never answers, the titles on screen are
/// still the right ones to choose from, and replacing them with a red error because a box is
/// asleep is precisely what the cache exists to prevent.
fn spawn_fetch(
    shared: LibraryShared,
    addr: String,
    (port, mgmt): (u16, u16),
    identity: (String, String),
    fp_hex: String,
    macs: Vec<String>,
    req: RequestKey,
) -> Worker {
    // Drops what an older fetch left; the list and phase land only under `req`.
    shared.begin_fetch();
    let pin = trust::parse_hex32(&fp_hex);
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            // This worker retries for up to 90 s and cannot be cancelled, so the player can
            // be two hosts further on by the time it answers. Every write below asks first
            // whether this fetch still owns the model.
            let mine = || shared.fetch_epoch() == req.epoch;
            if let Ok(path) = std::env::var("PUNKTFUNK_FAKE_LIBRARY") {
                load_fake(&shared, &req, &path);
                return;
            }
            // Whatever we already know about this host, on screen before a single packet goes
            // out. Keyed on the pinned fingerprint, so a box that came back on a new DHCP lease
            // is still recognised as the same host with the same library.
            let mut cached_games = None;
            if let Some(cached) = pf_client_core::library_cache::load(&fp_hex) {
                if !cached.games.is_empty() && mine() {
                    shared.set_games_cached(&req, to_model(&cached.games));
                    cached_games = Some(cached.games);
                }
            }
            // Fire-and-forget whenever `macs` is given (auto-wake on), online or not: a magic
            // packet is one datagram an awake machine ignores, cheaper than checking first.
            let waking = !macs.is_empty();
            let last_ip = addr.parse::<Ipv4Addr>().ok();
            if waking {
                wol::wake(&macs, last_ip);
                if mine() {
                    shared.set_stale(pf_console_ui::Stale::Waking);
                }
            }

            let started = Instant::now();
            let mut last_packet = started;
            let mut last_err = None;
            let mut fetched = None;
            loop {
                match library::fetch_games(&addr, mgmt, port, &identity, pin) {
                    Ok(games) => {
                        fetched = Some(games);
                        break;
                    }
                    Err(e) => {
                        // Anything other than "can't reach it" is settled — a rejected
                        // certificate does not become acceptable by waiting, and retrying an
                        // unpaired host for 90 s only delays telling the user what is
                        // actually wrong.
                        let retryable = matches!(e, library::LibraryError::Unreachable(_));
                        last_err = Some(e);
                        let budget = Duration::from_secs(WAKE_TIMEOUT_SECS);
                        if !waking || !retryable || started.elapsed() >= budget {
                            break;
                        }
                        if last_packet.elapsed() >= Duration::from_secs(WAKE_RESEND_SECS) {
                            wol::wake(&macs, last_ip);
                            last_packet = Instant::now();
                        }
                        std::thread::sleep(WAKE_RETRY_EVERY);
                    }
                }
            }

            // The list the covers are for: the host's, or the cached one it left behind.
            // A cached shelf without its covers is a wall of monograms until the host is back.
            let (games, live) = match fetched {
                Some(games) => (games, true),
                None => {
                    let e =
                        last_err.expect("the loop runs at least once and every miss records why");
                    if !mine() {
                        return;
                    }
                    match cached_games.take() {
                        Some(cached) => {
                            // The shelf stays; only the words change. The player can still pick
                            // a title — the launch will wake and dial the host on its own.
                            tracing::info!(%addr, error = %e, "library fetch failed; keeping the cached shelf");
                            shared.set_stale(pf_console_ui::Stale::Offline);
                            (cached, false)
                        }
                        None => {
                            shared.set_phase(&req, LibraryPhase::Error {
                                title: "Couldn't load the library".into(),
                                body: e.to_string(),
                                can_retry: true,
                            });
                            return;
                        }
                    }
                }
            };

            if !mine() {
                return;
            }
            let base = library::base_url(&addr, mgmt);
            let jobs: VecDeque<(String, Vec<String>)> = games
                .iter()
                .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
                .filter(|(_, candidates)| !candidates.is_empty())
                .collect();
            if live {
                shared.set_games(&req, to_model(&games));
                // Remembered AFTER it is on screen: the disk write is not on the path to a shelf.
                pf_client_core::library_cache::store(&fp_hex, &games);
                // What the host has up right now, so a title the player can return to says so.
                // Deliberately after the catalog — a slow `/status` must not hold the titles
                // back — and never fatal: an older host answers nothing and every badge stays off.
                let running = library::fetch_running(&addr, mgmt, port, &identity, pin);
                if mine() {
                    shared.set_running(&running);
                }
            }
            if !jobs.is_empty() {
                let api = library::HostApi::new(&addr, mgmt, port, &identity, pin);
                let rx = library::spawn_art_fetch(api, jobs);
                while let Ok((id, bytes)) = rx.recv_blocking() {
                    if !mine() {
                        return;
                    }
                    shared.push_art(id, bytes);
                }
            }
        })
        .ok()
}

/// The wire catalog in the shell's own terms.
///
/// One conversion, because there are now three callers (a live fetch, a cached one and the dev
/// hook) and a fourth that quietly dropped a field would be a shelf missing its launcher grouping
/// or its platform line on one path only.
///
/// `running` is deliberately NOT derived here: it is host state that arrives from `/status`,
/// separately and later, and the catalog this reads from is the same shape that gets written to
/// the disk cache. Seeding it from a catalog would be how a cached shelf comes back claiming a
/// game is up because it was up the last time anybody looked.
fn to_model(games: &[library::GameEntry]) -> Vec<LibraryGame> {
    games
        .iter()
        .map(|g| LibraryGame {
            id: g.id.clone(),
            title: g.title.clone(),
            store: g.store.clone(),
            launcher: g.is_launcher(),
            icon: g.icon_token().unwrap_or_default().to_string(),
            platform: g.platform.clone(),
            developer: g.developer.clone(),
            year: g.release_year,
            genres: g.genres.clone(),
            stats: g.stats,
            running: false,
            endable: false,
            install: g.install.clone().map(Into::into),
        })
        .collect()
}

/// Dev hook: entries from a JSON file; portrait paths starting with `/` load from disk.
fn load_fake(shared: &LibraryShared, req: &RequestKey, path: &str) {
    let games: Vec<library::GameEntry> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for g in &games {
        if let Some(p) = g.art.portrait.as_deref().filter(|p| p.starts_with('/')) {
            if let Ok(bytes) = std::fs::read(p) {
                shared.push_art(g.id.clone(), bytes);
            }
        }
    }
    shared.set_games(req, to_model(&games));
}

#[cfg(test)]
mod tests;
