//! The application shell as a relm4 component tree: [`AppModel`] owns the window, the
//! [`Store`], navigation, and the spawned session child's lifecycle. Hosts and Library are
//! child components, the two destinations of one view stack (design §2.1); dialogs are plain
//! GTK invoked from `update`. The connect flow is in `connect.rs`, host requests beside a
//! stream in `host_ops.rs`. Every stream runs in the `punktfunk-session` Vulkan binary —
//! the shell never touches video.

mod connect;
pub mod gate;
mod host_ops;
pub mod spawn;

use crate::hosts::{self, ConnectRequest, HostsMsg, HostsOutput, HostsPage, Phase};
use crate::library::{LibraryInit, LibraryMsg, LibraryOutput, LibraryPage};
use crate::store::{Changed, Store};
use crate::trust;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::orchestrate::{trust_route, ConnectOutcome, TrustRoute};
use pf_client_core::settings::GamepadUi;
use pf_client_core::start;
use relm4::prelude::*;
use spawn::{CancelHandle, SpawnOpts};
use std::cell::RefCell;
use std::rc::Rc;

pub const APP_ID: &str = "io.unom.Punktfunk";

/// How long a session asked to quit gets to close the host before it is killed.
const DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Everything the shell shares below the component tree.
pub struct AppModel {
    pub window: adw::ApplicationWindow,
    pub nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    pub store: Rc<Store>,
    pub identity: (String, String),
    /// App-lifetime SDL gamepad service (Settings' controller list + pinning). Streams
    /// run in the session binary, which has its own.
    pub gamepad: crate::gamepad::GamepadService,
    /// Device lists for the settings pickers (GPUs via `punktfunk-session
    /// --list-adapters` — the shell deliberately links no Vulkan itself — and audio
    /// endpoints via the PipeWire registry), probed once at startup on a worker thread.
    /// Empty until the probe lands — empty lists simply hide their pickers.
    pub probes: Rc<RefCell<crate::settings::DeviceProbes>>,
    hosts: Controller<HostsPage>,
    library: Controller<LibraryPage>,
    /// Hosts · Library.
    views: adw::ViewStack,
    /// One session child at a time — connects while one runs are ignored.
    busy: bool,
    /// Armed by [`AppMsg::WakeConnect`] (a dial to a host that isn't advertising but has a
    /// known MAC): if THAT dial's child exits with a connect failure, `SessionExited` falls
    /// back into the visible wake-and-wait instead of an error. Consumed on the next exit and
    /// matched against the exiting request, so it can never redirect an unrelated failure.
    wake_fallback: Option<ConnectRequest>,
    /// The request-access "waiting for approval" dialog, closed on the first child
    /// event. A shared slot (not a message): dialogs are main-thread GTK objects and
    /// `AppMsg` must stay `Send` for the session child's reader thread.
    ///
    /// The handler id rides along because `close()` EMITS the close response: closing this
    /// dialog in code is indistinguishable from the user pressing Cancel unless the handler
    /// is disconnected first ([`AppModel::close_waiting`]).
    waiting: crate::app::gate::WaitingSlot,
    /// Set when this shell kills the session child itself (the request-access Cancel). Read
    /// once on the child's exit: without it, EVERY signal death read as "we meant that" and
    /// an OOM-killed or crashed stream vanished with no message at all.
    session_cancelled: bool,
    /// Controllers attached at the last poll — the edge Controller-optimized UI opens on.
    pads: usize,
    /// Fullscreen Always put the window there, so turning it off may take it back out.
    fullscreen_by_setting: bool,
    /// The running session's child, from spawn to exit.
    session: Option<CancelHandle>,
    /// "Streaming from …" on both pages, with Disconnect.
    streaming: Banners,
}

#[derive(Debug)]
pub enum AppMsg {
    /// A `punktfunk://` URL arrived (scheme handler, a shortcut, or a second invocation
    /// forwarded to this instance by GApplication) — design/client-deep-links.md §4.1.
    DeepLink(String),
    /// The trust gate in front of every connect (see `AppModel::connect`).
    Connect(ConnectRequest),
    /// Connect to a saved host that isn't advertising but has a known MAC: fire a wake
    /// packet and DIAL IMMEDIATELY (mDNS absence ≠ unreachable — routed/Tailscale hosts
    /// never advertise here); only a failed dial falls into the visible wake-and-wait.
    WakeConnect(ConnectRequest),
    /// The SPAKE2 PIN ceremony dialog.
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    /// The Library on this host's shelf.
    OpenLibrary(ConnectRequest),
    /// Show a destination by its view name.
    ShowView(&'static str),
    Find,
    /// Rescan the network, or reload the shelf.
    Reload,
    /// Spawn the session child now (trust already decided; `tofu` = persist the
    /// fingerprint once the child proves it).
    StartSession {
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        opts: SpawnOpts,
    },
    /// The child presented its first frame. `cancel` remains live across the reader-to-main
    /// queue so a Cancel that overtakes this message still wins.
    SessionReady {
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        persist_paired: bool,
        cancel: Option<CancelHandle>,
    },
    /// The child exited (the session is over, or the connect failed).
    SessionExited {
        req: ConnectRequest,
        code: i32,
        error: Option<(String, bool)>,
        ended: Option<String>,
        tofu: bool,
    },
    /// Hand over to the gamepad console (`punktfunk-session --browse`) — the couch UI's
    /// door from the desktop shell.
    OpenConsole,
    /// The attached controller count changed (a 1 s poll).
    Pads(usize),
    /// Preferences closed: re-read what the window itself follows.
    SettingsClosed,
    ToggleFullscreen,
    /// The console child exited; `Some` carries why it ended badly.
    ConsoleExited(Option<String>),
    /// Request-access Cancel: the child was killed; release busy quietly.
    CancelPending,
    /// Upload the client log ring to this paired host (`logring::send_to_host`); the
    /// outcome lands as a Toast either way. The mgmt port rides along when an advert has one.
    SendLogs(ConnectRequest, Option<u16>),
    /// Run one of the host's own actions — sleep / restart / shut down it
    /// (`design/host-actions.md` §7). `danger` asks first; the outcome is a toast.
    HostAction {
        req: ConnectRequest,
        mgmt: Option<u16>,
        action_id: String,
        label: String,
        danger: bool,
    },
    /// The speed-test dialog resolved (either way) — release `busy`.
    SpeedTestDone,
    ShowPreferences,
    /// Re-open Preferences editing a specific layer — the settings scope switcher's
    /// destination (design/client-settings-profiles.md §5.1).
    ShowPreferencesScoped(crate::settings::Scope),
    ShowShortcuts,
    ShowAbout,
    ShowAddHost,
    /// End the running stream the way its window does.
    Disconnect,
    Toast(String),
}

fn ready_was_cancelled(cancel: Option<&CancelHandle>) -> bool {
    cancel.is_some_and(CancelHandle::is_cancelled)
}

/// Whether Controller-optimized UI opens the console on this poll: on the first controller
/// and never on a later one, so a console the player quit stays closed until every
/// controller has gone and one comes back. Always opens it once, at launch.
fn console_opens(ui: GamepadUi, prev: usize, now: usize) -> bool {
    ui == GamepadUi::WithController && prev == 0 && now > 0
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    #[test]
    fn the_console_opens_on_the_first_controller_only() {
        assert!(console_opens(GamepadUi::WithController, 0, 1));
        assert!(!console_opens(GamepadUi::WithController, 1, 2));
        assert!(!console_opens(GamepadUi::WithController, 2, 1));
        assert!(!console_opens(GamepadUi::WithController, 1, 0));
        assert!(!console_opens(GamepadUi::Off, 0, 1));
        assert!(!console_opens(GamepadUi::Always, 0, 1));
    }

    #[test]
    fn cancelled_request_rejects_late_ready() {
        let cancel = CancelHandle::default();
        assert!(!ready_was_cancelled(Some(&cancel)));

        cancel.kill();

        assert!(ready_was_cancelled(Some(&cancel)));
    }
}

pub struct AppInit {
    pub gamepad: crate::gamepad::GamepadService,
}

pub struct AppWidgets {}

impl SimpleComponent for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        // The minimum size is what the narrow breakpoint is checked against.
        adw::ApplicationWindow::builder()
            .title("Punktfunk")
            .default_width(1200)
            .default_height(780)
            .width_request(360)
            .height_request(294)
            .build()
    }

    fn init(
        init: Self::Init,
        window: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let identity = match trust::load_or_create_identity() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!("client identity: {e:#}");
                std::process::exit(1);
            }
        };
        install_resources();
        load_css();
        // Screenshot scenes must capture settled frames: kill every GTK/libadwaita
        // animation (a headless session may starve the frame clock and leave a
        // transition frozen mid-flight in the capture).
        if crate::shots::shot_scene().is_some() {
            if let Some(s) = gtk::Settings::default() {
                s.set_gtk_enable_animations(false);
            }
        }

        let store = Store::open();
        match crate::shots::shot_size() {
            Some((w, h)) => window.set_default_size(w, h),
            None if crate::shots::shot_scene().is_none() => remember_size(&window, &store),
            None => {}
        }
        // Recolour the shell from the desktop theme (Omarchy only; one stat everywhere else).
        // Every colour in `data/style.css` resolves through libadwaita's named palette, so
        // redefining those names is all it takes. After the settings load, because the
        // "Follow the Omarchy theme" switch decides whether it draws.
        crate::desktop::omarchy::install(store.settings().follow_os_theme);
        // Device lists for the settings pickers: probe in the background, ready long
        // before the dialog opens. A missing session binary or absent PipeWire just
        // leaves the corresponding list empty (and its picker hidden).
        let probes: Rc<RefCell<crate::settings::DeviceProbes>> = Rc::default();
        {
            let (tx, rx) = async_channel::bounded::<crate::settings::DeviceProbes>(1);
            std::thread::spawn(move || {
                let adapters: Vec<String> =
                    std::process::Command::new(crate::app::spawn::session_binary())
                        .arg("--list-adapters")
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                let (speakers, mics) = pf_client_core::audio::devices().unwrap_or_default();
                let _ = tx.send_blocking(crate::settings::DeviceProbes {
                    adapters,
                    speakers,
                    mics,
                });
            });
            let probes = probes.clone();
            glib::spawn_future_local(async move {
                if let Ok(found) = rx.recv().await {
                    *probes.borrow_mut() = found;
                }
            });
        }
        // Re-apply the persisted forwarded-controller pin (stable key; the service
        // matches it whenever such a pad connects).
        {
            let forward = store.settings().forward_pad.clone();
            if !forward.is_empty() {
                init.gamepad.set_pinned(Some(forward));
            }
        }

        let nav = adw::NavigationView::new();
        let views = adw::ViewStack::new();
        let narrow = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 550sp").expect("a breakpoint condition"),
        );
        window.add_breakpoint(narrow.clone());
        let hosts = HostsPage::builder()
            .launch(hosts::HostsInit {
                store: store.clone(),
                nav: nav.clone(),
                views: views.clone(),
                narrow: narrow.clone(),
            })
            .forward(sender.input_sender(), |out| match out {
                HostsOutput::Connect(req) => AppMsg::Connect(req),
                HostsOutput::WakeConnect(req) => AppMsg::WakeConnect(req),
                HostsOutput::Pair(req) => AppMsg::Pair(req),
                HostsOutput::SpeedTest(req) => AppMsg::SpeedTest(req),
                HostsOutput::Library(req) => AppMsg::OpenLibrary(req),
                HostsOutput::SendLogs(req, mgmt) => AppMsg::SendLogs(req, mgmt),
                HostsOutput::HostAction {
                    req,
                    mgmt,
                    action_id,
                    label,
                    danger,
                } => AppMsg::HostAction {
                    req,
                    mgmt,
                    action_id,
                    label,
                    danger,
                },
                HostsOutput::Toast(msg) => AppMsg::Toast(msg),
            });

        let library = LibraryPage::builder()
            .launch(LibraryInit {
                store: store.clone(),
                identity: identity.clone(),
                views: views.clone(),
                narrow,
            })
            .forward(sender.input_sender(), |out| match out {
                LibraryOutput::Connect(req) => AppMsg::Connect(req),
                LibraryOutput::WakeConnect(req) => AppMsg::WakeConnect(req),
                LibraryOutput::Toast(msg) => AppMsg::Toast(msg),
                LibraryOutput::ShowHosts => AppMsg::ShowView("hosts"),
                LibraryOutput::Pair(req) => AppMsg::Pair(req),
            });
        views.add_titled_with_icon(hosts.widget(), Some("hosts"), "Hosts", "computer-symbolic");
        views.add_titled_with_icon(
            library.widget(),
            Some("library"),
            "Library",
            "applications-games-symbolic",
        );
        let pages = [hosts.widget().clone(), library.widget().clone()];
        store_health_banner(&pages, &store);
        let streaming = Banners::new(&pages, {
            let sender = sender.clone();
            move || sender.input(AppMsg::Disconnect)
        });
        nav.add(
            &adw::NavigationPage::builder()
                .title("Punktfunk")
                .tag("main")
                .child(&views)
                .build(),
        );
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        window.set_content(Some(&toasts));

        let mut model = AppModel {
            window: window.clone(),
            nav,
            toasts,
            store,
            identity,
            gamepad: init.gamepad,
            probes,
            hosts,
            library,
            views,
            busy: false,
            wake_fallback: None,
            waiting: Rc::new(RefCell::new(None)),
            session_cancelled: false,
            pads: 0,
            session: None,
            streaming,
            fullscreen_by_setting: false,
        };
        install_actions(&model.window, &sender);
        // Controller-optimized UI reads the count on every change; the service has no event
        // for a pad arriving, and a 1 s poll is one lock and a short Vec.
        if cfg!(feature = "console") && crate::shots::shot_scene().is_none() {
            let (gamepad, sender) = (model.gamepad.clone(), sender.clone());
            let mut last = 0;
            glib::timeout_add_seconds_local(1, move || {
                let n = gamepad.pads().len();
                if n != last {
                    last = n;
                    sender.input(AppMsg::Pads(n));
                }
                glib::ControlFlow::Continue
            });
        }

        // CI screenshot mode: dispatch the scripted scene once the window is actually
        // mapped (AdwDialogs need a live window; relm4 maps it after `init` returns, so
        // this can't run inline like the pre-relm4 `activate` path did).
        if let Some(scene) = crate::shots::shot_scene() {
            let ctx = crate::shots::ShotCtx {
                window: model.window.clone(),
                hosts: model.hosts.sender().clone(),
                library: model.library.sender().clone(),
                views: model.views.clone(),
                store: model.store.clone(),
                gamepad: model.gamepad.clone(),
                identity: model.identity.clone(),
                sender: sender.clone(),
            };
            let fired = std::cell::Cell::new(false);
            model.window.connect_map(move |_| {
                if fired.replace(true) {
                    return; // map can fire more than once; the scene runs on the first
                }
                crate::shots::run_shot(&ctx, &scene);
            });
        }
        window.present();
        model.apply_fullscreen();

        // The deep-link seam is live from here: anything GApplication delivered during a cold
        // start has been parked, and everything from now on arrives as a message.
        LINK_TX.with_borrow_mut(|tx| *tx = Some(sender.input_sender().clone()));
        let parked = PENDING_LINKS.with_borrow_mut(std::mem::take);
        // Where a bare launch opens (design/default-host.md). Only a bare one: `--connect`,
        // `--browse` and every headless verb have already exec'd or returned before the
        // application object exists, and a parked link is explicit intent that wins outright.
        let console_home = cfg!(feature = "console")
            && crate::shots::shot_scene().is_none()
            && model.store.settings().gamepad_ui() == GamepadUi::Always;
        if parked.is_empty() && console_home {
            // The console follows Start in itself.
            sender.input(AppMsg::OpenConsole);
        } else if parked.is_empty() {
            let settings = model.store.settings();
            let known = model.store.hosts();
            let (default, source) = start::default_host_with_source(&settings, &known);
            tracing::info!(
                start_in = start::StartIn::parse(&settings.start_in).as_str(),
                default = default.map_or("none", |i| known.hosts[i].name.as_str()),
                source = source.as_str(),
                "client start"
            );
            let screen = start::start_screen(&settings, &known);
            drop(settings);
            if let Some(i) = screen.host_index() {
                let req = hosts::saved_request(&known.hosts[i]);
                sender.input(AppMsg::OpenLibrary(req.clone()));
                // Stream is the library PLUS a connect, never a screen of its own: the session
                // window is the overlay, so ending it leaves the shelf on screen underneath.
                if matches!(screen, start::Start::Stream(_)) {
                    sender.input(AppMsg::WakeConnect(req));
                }
            }
        }
        for url in parked {
            sender.input(AppMsg::DeepLink(url));
        }

        ComponentParts {
            model,
            widgets: AppWidgets {},
        }
    }

    fn update(&mut self, msg: AppMsg, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::DeepLink(url) => self.open_deep_link(&url, &sender),
            AppMsg::Connect(req) => self.connect(req, &sender),
            AppMsg::WakeConnect(req) => {
                if !self.busy {
                    // Never gate the dial on mDNS presence: a routed host (Tailscale/VPN) never
                    // advertises. The magic packet goes first, fire-and-forget, so an asleep box
                    // boots while the dial times out, and the fallback is armed for THIS request.
                    // Auto-wake off means no packet and no wake-and-wait, just the normal dial
                    // error; the host-card menu's explicit "Wake host" stays ungated.
                    if self.store.settings().auto_wake {
                        crate::wol::wake(&req.mac, req.addr.parse().ok());
                        self.wake_fallback = Some(req.clone());
                    }
                    sender.input(AppMsg::Connect(req));
                }
            }
            AppMsg::Pair(req) => {
                if !self.busy {
                    crate::app::gate::pin_dialog(&self.window, &sender, self.identity.clone(), req);
                }
            }
            AppMsg::SpeedTest(req) => self.speed_test(req, &sender),
            AppMsg::SendLogs(req, mgmt_port) => self.send_logs(req, mgmt_port, &sender),
            AppMsg::HostAction {
                req,
                mgmt,
                action_id,
                label,
                danger,
            } => self.host_action(req, mgmt, action_id, label, danger, &sender),
            AppMsg::SpeedTestDone => self.busy = false,
            AppMsg::OpenLibrary(req) => {
                self.show_view("library");
                self.library.emit(LibraryMsg::Open(req));
            }
            AppMsg::ShowView(name) => self.show_view(name),
            AppMsg::Find => {
                self.show_view("library");
                self.library.emit(LibraryMsg::FocusSearch);
            }
            AppMsg::Reload => match self.views.visible_child_name().as_deref() {
                Some("library") => self.library.emit(LibraryMsg::Reload),
                _ => self.hosts.emit(HostsMsg::Rescan),
            },
            AppMsg::StartSession {
                req,
                fp_hex,
                tofu,
                opts,
            } => self.start_session(req, fp_hex, tofu, opts, &sender),
            AppMsg::SessionReady {
                req,
                fp_hex,
                tofu,
                persist_paired,
                cancel,
            } => self.session_ready(req, fp_hex, tofu, persist_paired, cancel),
            AppMsg::SessionExited {
                req,
                code,
                error,
                ended,
                tofu,
            } => self.session_exited(req, code, error, ended, tofu, &sender),
            AppMsg::OpenConsole => self.open_console(&sender, false),
            AppMsg::Pads(n) => {
                let prev = std::mem::replace(&mut self.pads, n);
                if !self.busy && console_opens(self.store.settings().gamepad_ui(), prev, n) {
                    self.open_console(&sender, true);
                }
            }
            AppMsg::SettingsClosed => self.apply_fullscreen(),
            AppMsg::ToggleFullscreen => {
                if self.window.is_fullscreen() {
                    self.window.unfullscreen();
                } else {
                    self.window.fullscreen();
                }
            }
            AppMsg::ConsoleExited(err) => {
                self.busy = false;
                // Quitting the console (B at its root) exits 0 and returns here silently.
                if let Some(e) = err {
                    self.hosts
                        .emit(HostsMsg::ShowError(format!("Console UI ended — {e}")));
                }
                self.hosts.emit(HostsMsg::Refresh);
            }
            AppMsg::CancelPending => {
                // The child is being killed by the handler that sent this; its exit is ours.
                self.session_cancelled = true;
                self.close_waiting();
                self.busy = false;
                self.hosts.emit(HostsMsg::SetSession(None));
                self.toast("Cancelled — the request may still be pending on the host.");
            }
            AppMsg::ShowPreferences => sender.input(AppMsg::ShowPreferencesScoped(
                crate::settings::Scope::Defaults,
            )),
            AppMsg::ShowPreferencesScoped(scope) => self.show_preferences(scope, &sender),
            AppMsg::ShowShortcuts => shortcuts_dialog().present(Some(&self.window)),
            AppMsg::ShowAbout => crate::settings::show_about(&self.window),
            AppMsg::ShowAddHost => self.hosts.emit(HostsMsg::ShowAddHost),
            AppMsg::Disconnect => {
                if let Some(child) = &self.session {
                    // Our own end: an escalated kill must not read as a crash.
                    self.session_cancelled = true;
                    child.terminate(DISCONNECT_GRACE);
                    self.streaming.show("Disconnecting\u{2026}", None);
                }
            }
            AppMsg::Toast(msg) => self.toast(&msg),
        }
    }
}

impl AppModel {
    pub fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    /// A destination, over any host page pushed on top of it.
    fn show_view(&self, name: &str) {
        self.nav.pop_to_tag("main");
        self.views.set_visible_child_name(name);
    }

    /// The rows write through the store as they change, so the pages follow on their own.
    fn show_preferences(&self, scope: crate::settings::Scope, sender: &ComponentSender<Self>) {
        let (reopen, closed) = (sender.clone(), sender.clone());
        crate::settings::show_scoped(
            &self.window,
            self.store.clone(),
            &self.gamepad,
            &self.probes.borrow(),
            scope,
            // A scope change closes the dialog and asks for it back in the new scope.
            move |next| reopen.input(AppMsg::ShowPreferencesScoped(next)),
            move || closed.input(AppMsg::SettingsClosed),
        );
    }

    /// Fullscreen Always: on enters fullscreen, off leaves only a fullscreen it entered.
    fn apply_fullscreen(&mut self) {
        let always = self.store.settings().fullscreen_always();
        if always && !self.window.is_fullscreen() {
            self.window.fullscreen();
            self.fullscreen_by_setting = true;
        } else if !always && std::mem::take(&mut self.fullscreen_by_setting) {
            self.window.unfullscreen();
        }
    }
}

thread_local! {
    /// Where a delivered URL goes once the window exists. Both ends of this live on the GTK
    /// main thread: `connect_open` fires there, and so does the model's `init`.
    static LINK_TX: std::cell::RefCell<Option<relm4::Sender<AppMsg>>> =
        const { std::cell::RefCell::new(None) };
    /// URLs that arrived before the model existed — the cold-start case, where GApplication
    /// runs `open` before `activate` builds the window. A dropped URL is the one outcome a
    /// link must never have, so they wait here instead.
    static PENDING_LINKS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Hand a URL to the running app, or park it until there is one.
fn deliver_deep_link(url: String) {
    let queued = LINK_TX.with_borrow(|tx| match tx {
        Some(tx) => {
            let _ = tx.send(AppMsg::DeepLink(url.clone()));
            false
        }
        None => true,
    });
    if queued {
        PENDING_LINKS.with_borrow_mut(|q| q.push(url));
    }
}

/// The crate's one runtime env mutation, isolated so `main.rs`'s `deny(unsafe_code)` covers
/// everything else and the exemption is a named function rather than a whole call site.
#[allow(unsafe_code)]
fn clear_steam_sdl_device_filter() {
    for var in [
        "SDL_GAMECONTROLLER_IGNORE_DEVICES",
        "SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT",
    ] {
        if let Ok(v) = std::env::var(var) {
            tracing::info!(var, value = %v, "clearing Steam's SDL device filter");
            // SAFETY: called at the top of `run()`, before GTK init or any other thread
            // exists in this process — nothing reads the environment concurrently.
            unsafe { std::env::remove_var(var) };
        }
    }
}

pub fn run() -> glib::ExitCode {
    // Logs to stdout and the ring "Send logs to host" uploads. The spawned session's stderr
    // joins the ring too, so a bundle carries the stream's trail.
    pf_client_core::logring::init_tracing(std::io::stdout, true);
    // Steam launches its shortcuts with SDL_GAMECONTROLLER_IGNORE_DEVICES naming every
    // physical pad Steam Input has virtualized; the Settings controller list needs the
    // real devices (same rationale as the session binary).
    clear_steam_sdl_device_filter();
    // Headless paths (no GTK window). Pairing, wake, host listing and reset live in the
    // `punktfunk` CLI, which ships in the same package.
    if let Some(target) = crate::cli::arg_value("--library") {
        return crate::cli::headless_library(&target);
    }
    // Headless known-hosts management (list/add/edit/forget/reset) + reachability probes —
    // the shared store the Decky plugin drives; returns None when argv names none of them.
    if let Some(code) = crate::cli::headless_host_command() {
        return code;
    }
    // Streams and the console library live in the session binary now — exec it,
    // forwarding the relevant argv (the Decky wrapper keeps working through the shell
    // until it's repointed).
    // `--browse` may be bare now (the console home — hosts, pairing, settings), so the
    // gate is the flag, not a value after it.
    if crate::cli::arg_value("--connect").is_some()
        || crate::cli::arg_flag("--browse")
        || crate::cli::couch_launch()
    {
        return crate::cli::exec_session();
    }

    // HANDLES_OPEN is what makes `Exec=punktfunk-client %u` work: GApplication turns the URI
    // into an `open` call, and — this is the part that matters — a SECOND invocation forwards
    // its URI to the already-running instance over D-Bus and exits, so clicking a link with
    // Punktfunk open reuses the window instead of racing a new one.
    let mut builder = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN);
    // Screenshot mode launches the app once per scene back-to-back; NON_UNIQUE keeps
    // each launch its own primary instance.
    if crate::shots::shot_scene().is_some() {
        builder =
            builder.flags(gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_OPEN);
    }
    let adw_app = builder.build();
    adw_app.connect_open(|app, files, _hint| {
        for f in files {
            deliver_deep_link(f.uri().to_string());
        }
        // `open` does not raise a window on its own; the model's activate handler does.
        app.activate();
    });
    // One SDL context for the whole process, started while single-threaded.
    let gamepad = crate::gamepad::GamepadService::start();
    // argv stays withheld from GApplication — except for a positional URL, which is exactly
    // what GIO's single-instance forwarding is for. Passing it through means the FIRST
    // instance's `open` fires locally and a later one's is delivered to the primary, with no
    // IPC of our own.
    let args: Vec<String> = match crate::cli::deep_link_arg() {
        Some(url) => vec![
            std::env::args()
                .next()
                .unwrap_or_else(|| "punktfunk-client".into()),
            url,
        ],
        None => Vec::new(),
    };
    let app = relm4::RelmApp::from_app(adw_app).with_args(args);
    app.run::<AppModel>(AppInit { gamepad });
    glib::ExitCode::SUCCESS
}

/// Register the embedded gresource (built by build.rs from `data/`: the stylesheet and the
/// OS and launcher marks) and point the icon theme at it, so the `pf-os-*-symbolic` marks
/// resolve — and recolour — like any themed icon.
fn install_resources() {
    if let Err(e) = gio::resources_register_include!("punktfunk-client.gresource") {
        tracing::warn!(error = %e, "gresource did not register — no stylesheet, no OS marks");
        return;
    }
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_resource_path("/io/unom/Punktfunk/icons");
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    // Registered with the icons just before.
    provider.load_from_resource("/io/unom/Punktfunk/style.css");
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Window actions behind the hosts page's header (the primary menu + "+") — thin
/// forwards into the message loop.
fn install_actions(window: &adw::ApplicationWindow, sender: &ComponentSender<AppModel>) {
    let add = |name: &str, msg: fn() -> AppMsg| {
        let action = gio::SimpleAction::new(name, None);
        let sender = sender.clone();
        action.connect_activate(move |_, _| sender.input(msg()));
        action
    };
    window.add_action(&add("preferences", || AppMsg::ShowPreferences));
    window.add_action(&add("shortcuts", || AppMsg::ShowShortcuts));
    window.add_action(&add("about", || AppMsg::ShowAbout));
    window.add_action(&add("add-host", || AppMsg::ShowAddHost));
    window.add_action(&add("console", || AppMsg::OpenConsole));
    window.add_action(&add("fullscreen", || AppMsg::ToggleFullscreen));
    window.add_action(&add("show-hosts", || AppMsg::ShowView("hosts")));
    window.add_action(&add("show-library", || AppMsg::ShowView("library")));
    window.add_action(&add("find", || AppMsg::Find));
    window.add_action(&add("reload", || AppMsg::Reload));
    let app = relm4::main_application();
    for (_, action, accels) in ACCELS {
        app.set_accels_for_action(action, accels);
    }
}

/// The window's keys, as the shortcuts dialog lists them.
const ACCELS: &[(&str, &str, &[&str])] = &[
    ("Show Hosts", "win.show-hosts", &["<Control>1"]),
    ("Show Library", "win.show-library", &["<Control>2"]),
    ("Search the Library", "win.find", &["<Control>f"]),
    ("Add a host", "win.add-host", &["<Control>n"]),
    ("Rescan or reload", "win.reload", &["<Control>r", "F5"]),
    ("Preferences", "win.preferences", &["<Control>comma"]),
    (
        "Keyboard shortcuts",
        "win.shortcuts",
        &["<Control>question"],
    ),
    ("Toggle fullscreen", "win.fullscreen", &["F11"]),
    (
        "Close the window",
        "window.close",
        &["<Control>w", "<Control>q"],
    ),
];

/// The `Settings::extra` key the window's size is kept under: width, height, maximized.
const WINDOW_KEY: &str = "shell_window";

/// Open at the size the window last closed at, and keep it on close.
fn remember_size(window: &adw::ApplicationWindow, store: &Rc<Store>) {
    let kept: Vec<i64> = store
        .settings()
        .extra
        .get(WINDOW_KEY)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_i64()).collect())
        .unwrap_or_default();
    if let [w, h, maximized] = kept[..] {
        window.set_default_size(w.clamp(360, 8192) as i32, h.clamp(294, 8192) as i32);
        window.set_maximized(maximized != 0);
    }
    let store = store.clone();
    window.connect_close_request(move |w| {
        let (width, height) = w.default_size();
        let size = vec![
            i64::from(width),
            i64::from(height),
            i64::from(w.is_maximized()),
        ];
        store.update_settings(|s| s.extra.insert(WINDOW_KEY.into(), size.into()));
        glib::Propagation::Proceed
    });
}

/// One banner under each destination's header, kept in step.
#[derive(Clone)]
struct Banners(Vec<adw::Banner>);

impl Banners {
    fn new(pages: &[adw::ToolbarView], on_button: impl Fn() + Clone + 'static) -> Banners {
        Banners(
            pages
                .iter()
                .map(|page| {
                    let banner = adw::Banner::new("");
                    let on_button = on_button.clone();
                    banner.connect_button_clicked(move |_| on_button());
                    page.add_top_bar(&banner);
                    banner
                })
                .collect(),
        )
    }

    fn show(&self, title: &str, button: Option<&str>) {
        for b in &self.0 {
            b.set_title(title);
            b.set_button_label(button);
            b.set_revealed(true);
        }
    }

    fn hide(&self) {
        for b in &self.0 {
            b.set_revealed(false);
        }
    }
}

/// "Your changes aren't being saved", while the last write to the config dir failed.
fn store_health_banner(pages: &[adw::ToolbarView], store: &Store) {
    let banners = Banners::new(pages, || {});
    let check = move || match pf_client_core::trust::store_health::last_error() {
        Some(_) => banners.show(
            "Your changes aren\u{2019}t being saved \u{2014} Punktfunk can\u{2019}t write to its \
             settings folder.",
            None,
        ),
        None => banners.hide(),
    };
    check();
    store.subscribe(move |_| check());
}

/// The Keyboard Shortcuts dialog: this window's keys, then the session window's, kept here
/// as discoverable documentation.
pub fn shortcuts_dialog() -> adw::ShortcutsDialog {
    let shell = adw::ShortcutsSection::new(Some("This window"));
    for (title, action, _) in ACCELS {
        shell.add(adw::ShortcutsItem::from_action(title, action));
    }
    let stream = adw::ShortcutsSection::new(Some("Stream (session window)"));
    for (title, accel) in [
        ("Toggle fullscreen", "F11 <Alt>Return"),
        (
            "Release captured input (click the stream to capture)",
            "<Control><Alt><Shift>q",
        ),
        ("Disconnect", "<Control><Alt><Shift>d"),
        (
            "Cycle the statistics overlay (off · compact · normal · detailed)",
            "<Control><Alt><Shift>s",
        ),
        (
            "Mute or unmute your microphone (only while the stream sends one)",
            "<Control><Alt><Shift>v",
        ),
        (
            "Open the quick actions dial (a pad opens it with Select + A, and aims it with the left stick)",
            "<Control><Alt><Shift>o",
        ),
    ] {
        stream.add(adw::ShortcutsItem::new(title, accel));
    }
    let dialog = adw::ShortcutsDialog::new();
    dialog.add(shell);
    dialog.add(stream);
    dialog
}
