//! The WinUI 3 (windows-reactor) application shell.
//!
//! One root component, [`Shell`], holds the shell's state, and everything that changes it
//! arrives as a [`Msg`]: event handlers and worker threads (discovery, the spawned session's
//! reader, the probes) post through an [`AsyncSetState`], a typed handle on the root's queue.
//! Screens are functions of that state; the four with state of their own (hosts, pairing, the
//! speed test and the ring editor) are child components fed by props. Each screen lives in its
//! own submodule:
//!
//! * [`hosts`] — saved/discovered/manual host list, plus per-host forget + speed test
//! * [`connect`] — the trust gate and session lifecycle glue (connect / request-access flows)
//! * [`pair`] — the SPAKE2 PIN pairing ceremony
//! * [`speed`] — the per-host network speed test (probe burst over the real data plane)
//! * [`settings`] — persisted preferences · [`licenses`] — the license notices screen ·
//!   [`help`] — the in-stream keyboard-shortcuts reference (reached from the host list)
//! * [`stream`] — the stream status card (the stream itself runs in the spawned session window)
//! * [`style`] — the shared look (cards, pills, monograms), following the WinUI gallery: Mica
//!   backdrop, a centred max-width column, theme brushes

mod connect;
mod help;
mod hosts;
mod launcher_icons;
mod library;
mod licenses;
/// The shell's Lucide icon set — the console's own marks, drawn for WinUI.
mod lucide;
mod os_icons;
mod pair;
mod profiles;
/// The quick-action ring's editor — the ring itself, a section of the settings page.
mod quick_actions;
/// The shell's decisions as plain functions, table-tested without a window.
mod rules;
mod settings;
mod speed;
mod stream;
mod style;

use crate::trust::{KnownHosts, Settings};
use hosts::{HostRef, HostsProps};
use pf_client_core::collate::SortKey;
use pf_client_core::discovery::{self, DiscoveredHost};
use pf_client_core::gamepad::GamepadService;
use pf_client_core::orchestrate::{self, HostTarget};
use pf_client_core::settings::GamepadUi;
use pf_client_core::start;
use pf_fluent::motion::Tween;
use pf_fluent::Palette;
use rules::LinkGate;
use speed::{SpeedProps, SpeedState};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use windows_reactor::*;

#[derive(Clone, PartialEq)]
pub(crate) enum Screen {
    Hosts,
    Connecting,
    /// The no-PIN "request access" wait: an identified connect is in flight, parked by the host
    /// until the operator approves this device in its console. Cancelable.
    RequestAccess,
    /// Wake-on-LAN "wait until up": a magic packet was sent to an offline saved host and we're
    /// polling mDNS for it to reappear (re-sending periodically) before dialing. Cancelable.
    Waking,
    /// The wake ran out of budget: Try again replays it for the same target, Cancel goes home.
    WakeParked,
    Stream,
    Settings,
    /// Open-source / third-party license notices (reached from Settings).
    Licenses,
    /// In-stream keyboard-shortcuts reference + capture help (reached from the host list's
    /// Shortcuts button).
    Help,
    Pair,
    /// Per-host network speed test (probe burst + recommended bitrate).
    SpeedTest,
    /// The target host's game library (poster grid; tap-to-launch) — paired hosts only,
    /// since the fetch authenticates with the pairing identity.
    Library,
}

/// The host we're about to connect to / pair with / speed-test (carried into those screens
/// via `Shared::target`), plus this connect's own asks.
#[derive(Clone, Default)]
pub(crate) struct Target {
    /// A saved tile's is [`HostTarget::from`] its record; a discovered one's carries the
    /// advert's pin, MACs and mgmt port, which the library screen fetches from.
    pub(crate) host: HostTarget,
    pub(crate) pair_optional: bool,
    /// A ONE-OFF settings preset for this connect ("Connect with"): `Some(id)` overrides the
    /// host's binding for this launch, `Some("")` forces the global defaults on a bound host,
    /// `None` honors the binding. It never rebinds anything — the default changes only through
    /// the picker in the host editor (design/client-settings-profiles.md §5.2).
    pub(crate) preset: Option<String>,
    /// A library title id (`steam:570`, …) to launch on connect — carried on the target so it
    /// survives a detour through the PIN ceremony (a deep link's `launch=` toward an unpaired
    /// host must still launch the game once pairing succeeds).
    pub(crate) launch: Option<String>,
    /// A link's `as=`: the profile this connect plays as, an id or a name. It wins over the
    /// saved pick for this connect only.
    pub(crate) link_profile: Option<String>,
    /// Opened by a link: a pairing on its way continues into this connect.
    pub(crate) from_link: bool,
}

/// Finish a store write from a page: bump `rev`, so the page redraws from the files it reads
/// while drawing, and put a failure on the status line. `Some` holds what the edit returned.
pub(crate) fn saved<R, E: std::fmt::Display>(
    r: std::result::Result<R, E>,
    set_status: &AsyncSetState<String>,
    rev: Option<(u64, &AsyncSetState<u64>)>,
) -> Option<R> {
    if let Some((rev, set_rev)) = rev {
        set_rev.call(rev + 1);
    }
    r.map_err(|e| set_status.call(format!("Couldn't save \u{2014} {e:#}")))
        .ok()
}

/// Cross-thread shell state driven off the UI thread: the current target, the live spawned
/// session child (Disconnect/Cancel kill it) and its latest stats line, plus the connect-flow
/// cancel flag and the discovery/library/speed-test generation guards.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) target: Mutex<Target>,
    /// Forces the app's single LAN browse to re-query — the hosts page's Refresh. Installed by
    /// the discovery effect below; `None` until then (and if the browse never started, in which
    /// case Refresh is simply inert rather than a second, competing browse).
    pub(crate) rescan: Mutex<Option<discovery::Rescan>>,
    /// The live session child (spawn mode) — the status page's Disconnect and the
    /// request-access Cancel kill it. A FRESH handle is installed per spawn, so a stale
    /// handle never kills a newer session.
    pub(crate) session: Mutex<pf_client_core::orchestrate::CancelHandle>,
    /// Latest stats window from the session child (spawn mode); mirrored into the HUD
    /// sample for the session status page.
    pub(crate) stats: Mutex<Option<punktfunk_core::hud::StatsSnapshot>>,
    /// Cancel flag for the in-flight "request access" connect. A FRESH flag is installed per
    /// request: the waiting screen's Cancel button reads it back from here and sets it, and that
    /// request's event loop (which captured the same `Arc` at spawn) then tears down silently when
    /// the parked connect finally resolves. `None` outside a request-access flow.
    pub(crate) cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Speed-test run generation, bumped by the hosts page when it starts a run. A probe worker
    /// only publishes its outcome while its generation is still current, so a test abandoned
    /// mid-run can't overwrite a newer run's result when it finally resolves.
    pub(crate) speed_gen: std::sync::atomic::AtomicU64,
    /// Whether the live session child is a `--browse` console-UI run (vs a stream) — the
    /// session status page words itself accordingly. Set by each spawn.
    pub(crate) browse: std::sync::atomic::AtomicBool,
    /// Library-fetch generation (the speed test's guard pattern): bumped per fetch so a
    /// superseded worker (re-open, Retry, another host) stops publishing.
    pub(crate) library_gen: std::sync::atomic::AtomicU64,
    /// PIN-pairing generation (the same guard): bumped per attempt and by Cancel, so a
    /// ceremony the user left still saves its pin but neither connects nor navigates.
    pub(crate) pair_gen: std::sync::atomic::AtomicU64,
    /// Opens the profile picker over whatever screen is up. Installed by root; a connect's worker
    /// thread raises the picker through it ([`profiles::then_connect`]).
    pub(crate) set_picker: Mutex<Option<AsyncSetState<Option<profiles::PickerAsk>>>>,
    /// Opens the wait for a profile's seat the same way ([`profiles::seat_then`]).
    pub(crate) set_seat: Mutex<Option<AsyncSetState<Option<profiles::SeatWait>>>>,
    pub(crate) pictures: Mutex<profiles::Pictures>,
}

pub struct AppCtx {
    pub(crate) identity: (String, String),
    /// The settings snapshot the UI renders from. Loaded once at startup, and RE-BASED on
    /// the file when the settings page is (re)entered (`settings::refresh_snapshot`) and
    /// inside every `commit` — this process is not the file's only writer (session resize,
    /// console UI, Decky), so a plain process-lifetime snapshot goes stale on screen.
    pub(crate) settings: Mutex<Settings>,
    pub(crate) gamepad: GamepadService,
    pub(crate) shared: Arc<Shared>,
    /// The settings page's GPU and audio-endpoint lists, re-probed with the snapshot above.
    pub(crate) probes: Mutex<settings::DeviceProbes>,
}

/// A typed handle on the shell's message queue: `call` posts the value as its [`Msg`], from the
/// UI thread or any worker. Cloning is cheap and holds no value.
pub(crate) struct AsyncSetState<T> {
    queue: ComponentCompletion<Msg>,
    wrap: fn(T) -> Msg,
}

impl<T> Clone for AsyncSetState<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            wrap: self.wrap,
        }
    }
}

impl<T> AsyncSetState<T> {
    pub(crate) fn call(&self, value: T) {
        // `false` once the window is gone, when there is nothing left to update.
        let _ = self.queue.clone().complete((self.wrap)(value));
    }
}

/// Stable app services handed to the pages as props: the app, the theme's colours, and the
/// setters a page navigates and reports through. Equal by `ctx` identity and palette, so a page
/// whose props are just `Svc` redraws on a theme change and on nothing else.
#[derive(Clone)]
pub(crate) struct Svc {
    pub(crate) ctx: Arc<AppCtx>,
    pub(crate) pal: Palette,
    pub(crate) set_screen: AsyncSetState<Screen>,
    pub(crate) set_status: AsyncSetState<String>,
    /// The hosts page resets the speed test to `Running` before navigating; the probe worker
    /// completes it.
    pub(crate) set_speed: AsyncSetState<SpeedState>,
    /// The hosts page starts a library fetch before navigating; the worker and the art
    /// stream complete it.
    pub(crate) set_library: AsyncSetState<library::LibraryState>,
}

impl PartialEq for Svc {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx) && self.pal == other.pal
    }
}

/// The shell's input: the app, equal by identity.
#[derive(Clone)]
pub(crate) struct ShellInput(Arc<AppCtx>);

impl PartialEq for ShellInput {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Everything that changes the shell: one variant per piece of state, the tweens' frames, and
/// the window's own reports.
pub(crate) enum Msg {
    Screen(Screen),
    Status(String),
    Hosts(Vec<DiscoveredHost>),
    Probed(HashMap<String, bool>),
    Hud(Box<stream::HudSample>),
    Speed(SpeedState),
    Library(library::LibraryState),
    EndGame(library::EndGameUi),
    LibrarySort(SortKey),
    Forget(Option<HostRef>),
    Rename(Option<HostRef>),
    ShowAdd(bool),
    Hover(Option<String>),
    HostsRev(u64),
    SettingsNav(String),
    SettingsScope(String),
    SettingsDelete(Option<String>),
    SettingsEdit(bool),
    CustomRes(bool),
    CustomScale(bool),
    SettingsRev(u64),
    Picker(Option<profiles::PickerAsk>),
    Seat(Option<profiles::SeatWait>),
    /// A `punktfunk://` link, from argv or a later instance.
    Link(String),
    LinkConfirm(Option<Box<orchestrate::ConnectPlan>>),
    Pads(usize),
    Width(f64),
    Scheme(ColorScheme),
    /// Windows changed its accent or contrast.
    Colors(Palette),
    /// Escape in the window: the topmost open sheet closes.
    Escape,
    Tick(Anim),
    /// The window exists; [`crate::shell_window`] has its handle.
    Window,
}

/// Which tween a [`Msg::Tick`] advances.
#[derive(Clone, Copy)]
pub(crate) enum Anim {
    Screen,
    Section,
    AddHost,
}

/// 14 × 16 ms ≈ 220 ms for a navigation, 12 for the Add-host sheet.
const ENTER_FRAMES: u32 = 14;
const ADD_FRAMES: u32 = 12;

/// The shell's state. `settings_*` and `custom_*` are the settings page's, held here because
/// that page is a plain function of this state.
pub(crate) struct Shell {
    ctx: Arc<AppCtx>,
    queue: ComponentCompletion<Msg>,
    pal: Palette,
    /// The window's client width, which the tile grids lay out by.
    width: f64,
    screen: Screen,
    status: String,
    hosts: Vec<DiscoveredHost>,
    /// Saved-host reachability, keyed by `KnownHost::card_key`, from the probe sweep.
    probed: HashMap<String, bool>,
    hud: stream::HudSample,
    speed: SpeedState,
    library: library::LibraryState,
    end_game: library::EndGameUi,
    /// The shelf's order: the shared `library_sort`, read at start and written on each pick.
    library_sort: SortKey,
    forget: Option<HostRef>,
    rename: Option<HostRef>,
    show_add: bool,
    /// The hovered host tile's stable id, for the card's pointer-over fill.
    hover: Option<String>,
    /// Bumped when a menu action rewrites the known-hosts store behind the tiles.
    hosts_rev: u64,
    /// The Settings sidebar section, kept across visits this run.
    settings_nav: String,
    /// The layer the settings page edits: "" for the defaults, else a preset id.
    settings_scope: String,
    settings_delete: Option<String>,
    settings_edit: bool,
    /// Resolution / Render scale sit on Custom… while the stored value is a listed one.
    custom_res: bool,
    custom_scale: bool,
    /// Bumped by every settings commit: a preset-scope edit changes what the page shows
    /// without changing any state it reads.
    settings_rev: u64,
    picker: Option<profiles::PickerAsk>,
    seat: Option<profiles::SeatWait>,
    /// A link that named its host by something guessable, awaiting "Open this link?".
    link_confirm: Option<Box<orchestrate::ConnectPlan>>,
    pads: usize,
    enter: Tween,
    section_enter: Tween,
    add_enter: Tween,
    /// Keeps [`Msg::Colors`] arriving while the window lives.
    _colors: Option<pf_fluent::Watch>,
}

impl Shell {
    fn set<T>(&self, wrap: fn(T) -> Msg) -> AsyncSetState<T> {
        AsyncSetState {
            queue: self.queue.clone(),
            wrap,
        }
    }

    fn svc(&self) -> Svc {
        Svc {
            ctx: self.ctx.clone(),
            pal: self.pal,
            set_screen: self.set(Msg::Screen),
            set_status: self.set(Msg::Status),
            set_speed: self.set(Msg::Speed),
            set_library: self.set(Msg::Library),
        }
    }
}

impl Component for Shell {
    type Input = ShellInput;
    type Message = Msg;

    fn create(input: &ShellInput, cx: &ComponentContext<Self>) -> Self {
        let ctx = input.0.clone();
        let library_sort = SortKey::parse(&ctx.settings.lock().unwrap().library_sort);
        let shell = Shell {
            ctx: ctx.clone(),
            queue: cx.completion(),
            pal: Palette::system(),
            width: WINDOW_W,
            screen: Screen::Hosts,
            status: String::new(),
            hosts: Vec::new(),
            probed: HashMap::new(),
            hud: stream::HudSample::default(),
            speed: SpeedState::Running,
            library: library::LibraryState::default(),
            end_game: library::EndGameUi::default(),
            library_sort,
            forget: None,
            rename: None,
            show_add: false,
            hover: None,
            hosts_rev: 0,
            // Opens on General, the first sidebar item, as the Apple client does.
            settings_nav: "general".into(),
            settings_scope: String::new(),
            settings_delete: None,
            settings_edit: false,
            custom_res: false,
            custom_scale: false,
            settings_rev: 0,
            picker: None,
            seat: None,
            link_confirm: None,
            pads: 0,
            enter: Tween::start(0.0, 1.0, ENTER_FRAMES, Msg::Tick(Anim::Screen), cx),
            section_enter: Tween::settled(1.0),
            add_enter: Tween::settled(0.0),
            _colors: {
                let queue = cx.completion();
                pf_fluent::watch(move || {
                    let _ = queue.clone().complete(Msg::Colors(Palette::system()));
                })
            },
        };
        // A connect's worker thread raises the picker and the seat wait through these.
        *ctx.shared.set_picker.lock().unwrap() = Some(shell.set(Msg::Picker));
        *ctx.shared.set_seat.lock().unwrap() = Some(shell.set(Msg::Seat));
        // Before the link poll starts: a link queued at startup is explicit intent and wins.
        open_start_screen(&shell.svc());
        poll_links(shell.set(Msg::Link));
        browse_hosts(&ctx, shell.set(Msg::Hosts));
        poll_pads(ctx.gamepad.clone(), shell.set(Msg::Pads));
        poll_hud(ctx.shared.clone(), shell.set(|h| Msg::Hud(Box::new(h))));
        probe_saved_hosts(ctx.shared.clone(), shell.set(Msg::Probed));
        let _ = cx.run_window(|w| {
            crate::shell_window::adopt(w.as_raw());
            Msg::Window
        });
        shell
    }

    fn update(&mut self, msg: Msg, cx: &ComponentContext<Self>) {
        match msg {
            Msg::Screen(s) => {
                if s != self.screen {
                    self.screen = s;
                    self.enter = Tween::start(0.0, 1.0, ENTER_FRAMES, Msg::Tick(Anim::Screen), cx);
                }
            }
            Msg::Status(s) => self.status = s,
            Msg::Hosts(h) => self.hosts = h,
            Msg::Probed(p) => self.probed = p,
            Msg::Hud(h) => self.hud = *h,
            Msg::Speed(s) => self.speed = s,
            Msg::Library(l) => self.library = l,
            Msg::EndGame(e) => self.end_game = e,
            Msg::LibrarySort(k) => self.library_sort = k,
            Msg::Forget(f) => self.forget = f,
            Msg::Rename(r) => self.rename = r,
            Msg::ShowAdd(open) => {
                if open != self.show_add {
                    self.show_add = open;
                    // Closing unmounts the sheet at once: nothing to animate.
                    self.add_enter = if open {
                        Tween::start(0.0, 1.0, ADD_FRAMES, Msg::Tick(Anim::AddHost), cx)
                    } else {
                        Tween::settled(0.0)
                    };
                }
            }
            Msg::Hover(h) => self.hover = h,
            Msg::HostsRev(r) => self.hosts_rev = r,
            Msg::SettingsNav(tag) => {
                // The content column slides in on a section switch; the sidebar stays put.
                if tag != self.settings_nav {
                    self.settings_nav = tag;
                    self.section_enter =
                        Tween::start(0.0, 1.0, ENTER_FRAMES, Msg::Tick(Anim::Section), cx);
                }
            }
            Msg::SettingsScope(s) => self.settings_scope = s,
            Msg::SettingsDelete(d) => self.settings_delete = d,
            Msg::SettingsEdit(e) => self.settings_edit = e,
            Msg::CustomRes(c) => self.custom_res = c,
            Msg::CustomScale(c) => self.custom_scale = c,
            Msg::SettingsRev(r) => self.settings_rev = r,
            Msg::Picker(p) => self.picker = p,
            Msg::Seat(s) => self.seat = s,
            Msg::Link(url) => route_link(
                &self.ctx,
                &url,
                &self.screen,
                &self.set(Msg::Screen),
                &self.set(Msg::Status),
                &self.set(Msg::LinkConfirm),
            ),
            Msg::LinkConfirm(p) => self.link_confirm = p,
            Msg::Pads(n) => {
                // Controller-optimized UI, on each change of the count. While a connect, a
                // stream, the console or a who-plays question holds the shell, the console
                // waits for the next first pad.
                if n != self.pads {
                    let prev = std::mem::replace(&mut self.pads, n);
                    let busy = rules::console_waits(
                        &self.screen,
                        self.picker.is_some(),
                        self.seat.is_some(),
                    );
                    open_console_on_pad(&self.svc(), busy, prev, n);
                }
            }
            Msg::Width(w) => self.width = w,
            Msg::Scheme(s) => self.pal = self.pal.with_scheme(s),
            Msg::Colors(p) => self.pal = p.with_scheme(self.pal.scheme),
            // The topmost sheet first, each closed the way its own Cancel closes it.
            Msg::Escape => {
                if let Some(w) = self.seat.take() {
                    w.abandon();
                } else if let Some(a) = self.picker.take() {
                    a.abandon();
                } else if self.settings_edit {
                    self.settings_edit = false;
                    self.settings_rev += 1;
                } else if self.rename.is_some() {
                    self.rename = None;
                } else {
                    self.update(Msg::ShowAdd(false), cx);
                }
            }
            Msg::Tick(anim) => match anim {
                Anim::Screen => self.enter.tick(Msg::Tick(anim), cx),
                Anim::Section => self.section_enter.tick(Msg::Tick(anim), cx),
                Anim::AddHost => self.add_enter.tick(Msg::Tick(anim), cx),
            },
            Msg::Window => {}
        }
    }

    fn view(&self, _input: &ShellInput, cx: &mut ViewContext<'_, Self>) -> View {
        cx.window_title("Punktfunk");
        // A floor under every layout: below it the header and cards would clip rather than
        // reflow (the pages adapt down to it).
        cx.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .constraints(WindowConstraints {
                    min_width: Some(420.0),
                    min_height: Some(360.0),
                    max_width: None,
                    max_height: None,
                }),
        );
        let scheme = cx.callback(Msg::Scheme);
        cx.on_color_scheme(scheme);
        let size = cx.callback(|s: WindowSize| Msg::Width(s.width));
        cx.on_window_size(size);

        let svc = self.svc();
        let pal = &self.pal;
        let body: View = match &self.screen {
            Screen::Hosts => component::<hosts::HostsPage>(
                "hosts",
                HostsProps {
                    svc: svc.clone(),
                    hosts: self.hosts.clone(),
                    probed: self.probed.clone(),
                    status: self.status.clone(),
                    forget: self.forget.clone(),
                    rename: self.rename.clone(),
                    show_add: self.show_add,
                    add_anim: self.add_enter.eased(),
                    hover: self.hover.clone(),
                    hosts_rev: self.hosts_rev,
                    width: self.width,
                    set_forget: self.set(Msg::Forget),
                    set_rename: self.set(Msg::Rename),
                    set_show_add: self.set(Msg::ShowAdd),
                    set_hover: self.set(Msg::Hover),
                    set_hosts_rev: self.set(Msg::HostsRev),
                },
            )
            .into(),
            Screen::Connecting => connect::connecting_page(pal, &self.ctx, &self.status),
            Screen::RequestAccess => connect::request_access_page(pal, &self.ctx, &svc.set_screen),
            Screen::Waking => connect::waking_page(pal, &self.ctx, &svc.set_screen),
            Screen::WakeParked => {
                connect::wake_parked_page(pal, &self.ctx, &svc.set_screen, &svc.set_status)
            }
            Screen::Settings => settings::settings_page(self),
            Screen::Licenses => licenses::licenses_page(pal, &self.ctx, &svc.set_screen),
            Screen::Help => help::help_page(pal, &svc.set_screen),
            Screen::Pair => component::<pair::PairPage>("pair", svc.clone()).into(),
            Screen::SpeedTest => component::<speed::SpeedPage>(
                "speed",
                SpeedProps {
                    svc: svc.clone(),
                    state: self.speed.clone(),
                },
            )
            .into(),
            Screen::Library => library::library_page(self),
            // The stream runs in the punktfunk-session child's own window; this screen is
            // its status card.
            Screen::Stream => stream::session_page(pal, &self.ctx, &self.hud),
        };
        // The caption, which Settings' back arrow rides on: NavigationView's own back button
        // raises no event here, and the WinUI pattern puts it in the title bar.
        let caption = TitleBar::new()
            .title("Punktfunk")
            .icon(Icon::image_data(EncodedImage::from_static(APP_ICON)))
            .is_back_button_visible(self.screen == Screen::Settings)
            .is_back_button_enabled(true)
            .on_back_requested(cx.callback(|()| Msg::Screen(Screen::Hosts)));
        let (escape, sheet_open) = (cx.callback(|()| Msg::Escape), self.sheet_open());
        Border::new()
            .content(
                Grid::new()
                    .rows([GridLength::Auto, GridLength::STAR])
                    .children((
                        caption,
                        Grid::new().grid_row(1).children((
                            entrance(body, &self.screen, self.enter.eased()),
                            profiles::picker_slot(pal, &self.picker, &self.set(Msg::Picker)),
                            profiles::seat_slot(pal, &self.seat, &self.set(Msg::Seat)),
                        )),
                    )),
            )
            // Escape tunnels through here from any focused element, so it closes a sheet
            // that never took focus.
            .on_preview_key_down(RoutedCallback::new(move |k: KeyEventInfo| {
                let close = sheet_open && k.key == VirtualKey::ESCAPE;
                if close {
                    escape.call(());
                }
                close
            }))
            // At root, not on a page: a link can arrive while any screen is up.
            .content_dialog(self.link_confirmation())
    }
}

/// The caption's mark: the unplated app logo, sharp at the title bar's 16 DIPs up to 300 %.
const APP_ICON: &[u8] =
    include_bytes!("../../packaging/assets/Square44x44Logo.targetsize-48_altform-unplated.png");

/// The window's first client size, in DIPs.
const WINDOW_W: f64 = 1000.0;
const WINDOW_H: f64 = 720.0;

pub fn run(identity: (String, String), gamepad: GamepadService) -> windows::core::Result<()> {
    let ctx = Arc::new(AppCtx {
        identity,
        settings: Mutex::new(Settings::load()),
        gamepad,
        shared: Arc::new(Shared::default()),
        probes: Mutex::default(),
    });
    // Re-apply the persisted forwarded-controller pin (stable key; the service matches it
    // whenever such a pad connects) — GTK-shell parity.
    {
        let forward = ctx.settings.lock().unwrap().forward_pad.clone();
        if !forward.is_empty() {
            ctx.gamepad.set_pinned(Some(forward));
        }
    }
    App::run_component_with_policy::<Shell>(
        ShellInput(ctx),
        WindowPolicy::new()
            .title("Punktfunk")
            .client_size(WINDOW_W, WINDOW_H),
    )
}

impl Shell {
    /// Whether a sheet is up for [`Msg::Escape`] to close, in the set it walks.
    fn sheet_open(&self) -> bool {
        self.seat.is_some()
            || self.picker.is_some()
            || (self.screen == Screen::Settings && self.settings_edit)
            || (self.screen == Screen::Hosts && (self.rename.is_some() || self.show_add))
    }

    /// The "Open this link?" confirmation for a guessable-reference link: Connect runs the
    /// plan a stable-id link would have dialled, Cancel (and Escape) does nothing.
    fn link_confirmation(&self) -> ContentDialog {
        let pending = self.link_confirm.clone();
        let content = pending
            .as_ref()
            .map(|p| rules::link_prompt(&p.host.name, &p.host.addr, p.launch.as_deref()))
            .unwrap_or_default();
        let (ctx, ss, st, sc) = (
            self.ctx.clone(),
            self.set(Msg::Screen),
            self.set(Msg::Status),
            self.set(Msg::LinkConfirm),
        );
        ContentDialog::new()
            .title("Open this link?")
            .content(content)
            .primary_button_text("Connect")
            .close_button_text("Cancel")
            .is_open(pending.is_some())
            .on_closed(move |r: ContentDialogResult| {
                sc.call(None);
                if r == ContentDialogResult::Primary
                    && let Some(plan) = &pending
                {
                    dial_link(&ctx, plan, &ss, &st);
                }
            })
    }
}

/// The screen's body inside its navigation entrance: slides up and fades in with `progress`.
/// The Stream screen is a plain status card (the session child owns the real stream window)
/// and is shown without the tween.
fn entrance(body: View, screen: &Screen, progress: f64) -> View {
    if matches!(screen, Screen::Stream) {
        return body;
    }
    Border::new()
        .content(body)
        .opacity(progress)
        .translation(0.0, (1.0 - progress) * 22.0)
        .into()
}

/// `punktfunk://` links: the receiver thread queues them (from this launch's argv, or from a
/// later instance over WM_COPYDATA), and this poll posts each one to the shell.
fn poll_links(set_link: AsyncSetState<String>) {
    std::thread::Builder::new()
        .name("pf-deeplink-poll".into())
        .spawn(move || loop {
            for url in crate::deeplink::drain() {
                set_link.call(url);
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        })
        .ok();
}

/// The connected-controller count, mirrored from the gamepad service on each change.
fn poll_pads(gamepad: GamepadService, set_pads: AsyncSetState<usize>) {
    std::thread::Builder::new()
        .name("pf-pads".into())
        .spawn(move || {
            let mut last = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let n = gamepad.pads().len();
                if n != last {
                    last = n;
                    set_pads.call(n);
                }
            }
        })
        .ok();
}

/// The spawned session child's latest stats window, mirrored for the stream status page on
/// each change.
fn poll_hud(shared: Arc<Shared>, set_hud: AsyncSetState<stream::HudSample>) {
    std::thread::Builder::new()
        .name("pf-hud".into())
        .spawn(move || {
            let mut last = stream::HudSample::default();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let now = stream::HudSample {
                    stats: shared.stats.lock().unwrap().clone(),
                };
                if now != last {
                    last = now.clone();
                    set_hud.call(now);
                }
            }
        })
        .ok();
}

/// Controller-optimized UI: the first controller opens the console unless `busy` holds the
/// shell or a session child is alive.
fn open_console_on_pad(svc: &Svc, busy: bool, prev: usize, now: usize) {
    if hosts::CONSOLE_UI_AVAILABLE
        && !busy
        && !svc.ctx.shared.session.lock().unwrap().is_running()
        && orchestrate::console_opens(Settings::load().gamepad_ui(), prev, now)
    {
        connect::open_console(&svc.ctx, &svc.set_screen, &svc.set_status, true);
    }
}

/// Open where a bare launch lands (`start::start_screen`): the console on Always, else the
/// default host's library, plus a connect for Stream.
fn open_start_screen(svc: &Svc) {
    if crate::deeplink::pending() {
        return;
    }
    let settings = svc.ctx.settings.lock().unwrap().clone();
    // On Always the console is home, and it follows Start in itself.
    if hosts::CONSOLE_UI_AVAILABLE && settings.gamepad_ui() == GamepadUi::Always {
        connect::open_console(&svc.ctx, &svc.set_screen, &svc.set_status, false);
        return;
    }
    let known = pf_client_core::trust::KnownHosts::load();
    let (default, source) = start::default_host_with_source(&settings, &known);
    tracing::info!(
        start_in = start::StartIn::parse(&settings.start_in).as_str(),
        default = default.map_or("none", |i| known.hosts[i].name.as_str()),
        source = source.as_str(),
        "client start"
    );
    let screen = start::start_screen(&settings, &known);
    let Some(i) = screen.host_index() else { return };
    let target = Target {
        host: HostTarget::from(&known.hosts[i]),
        ..Target::default()
    };
    library::open_library(svc, target.clone());
    // Stream is the library PLUS a connect, never a screen of its own: the session
    // window is the overlay, so ending it leaves the shelf on screen underneath.
    if matches!(screen, start::Start::Stream(_)) {
        connect::initiate_waking(&svc.ctx, target, &svc.set_screen, &svc.set_status);
    }
}

/// The app's single LAN browse: installs its rescan handle and mirrors the live advert list
/// into `set_hosts` from a worker thread. A saved host stays on the page from the trust store
/// when its advert goes.
fn browse_hosts(ctx: &AppCtx, set_hosts: AsyncSetState<Vec<DiscoveredHost>>) {
    let (rx, rescan) = discovery::browse();
    *ctx.shared.rescan.lock().unwrap() = Some(rescan);
    std::thread::spawn(move || {
        let mut acc: Vec<DiscoveredHost> = Vec::new();
        while let Ok(event) = rx.recv_blocking() {
            rules::fold_advert(&mut acc, event);
            set_hosts.call(acc.clone());
        }
    });
}

/// Periodic reachability sweep: a saved host reached only over a routed network (Tailscale or
/// VPN) never advertises on mDNS, so presence can't come from the discovery stream alone. Probes
/// every saved host (bounded, trust-agnostic QUIC handshake) and posts each changed result to
/// `set_probed`, which is the whole of the Online pip.
fn probe_saved_hosts(shared: Arc<Shared>, set_probed: AsyncSetState<HashMap<String, bool>>) {
    std::thread::Builder::new()
        .name("pf-probe".into())
        .spawn(move || {
            let mut last = HashMap::new();
            loop {
                // A spawned session/browse child is running: the shell is hidden
                // (nobody sees the pips) and one of these hosts is mid-stream —
                // probing it is pure noise. Sleep through and sweep after it ends.
                if shared.session.lock().unwrap().is_running() {
                    std::thread::sleep(crate::trust::PROBE_INTERVAL);
                    continue;
                }
                // `probe_known`, not a probe per host: it fans out, asks who answered
                // (so a stranger on a sleeping host's lease cannot light its pip), and
                // re-points a host found at an address it left.
                let hosts: Vec<_> = KnownHosts::load()
                    .hosts
                    .into_iter()
                    .filter(|h| !h.addr.is_empty())
                    .collect();
                let online = crate::trust::probe_known(&hosts, crate::trust::PROBE_TIMEOUT);
                let map: HashMap<String, bool> =
                    hosts.iter().map(|h| h.card_key()).zip(online).collect();
                if map != last {
                    last = map.clone();
                    set_probed.call(map);
                }
                std::thread::sleep(crate::trust::PROBE_INTERVAL);
            }
        })
        .ok();
}

/// Route an arriving link. Parsing, preset resolution and every refusal rule (only a stable
/// record id dials unattended) live in `plan_from_link`. This end turns the outcome into the
/// call a tile click makes, so a link gets the tile's wake, trust and error handling.
fn route_link(
    ctx: &Arc<AppCtx>,
    url: &str,
    screen_now: &Screen,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    set_link_confirm: &AsyncSetState<Option<Box<pf_client_core::orchestrate::ConnectPlan>>>,
) {
    let refuse = |msg: String| {
        tracing::info!(%msg, "deep link refused");
        set_status.call(msg);
        set_screen.call(Screen::Hosts);
    };
    // Rule 2 of §3: never preempt a live session. Only this layer knows one is running,
    // which is why the brain leaves the check here.
    let running = ctx.shared.session.lock().unwrap().is_running();
    let link = match rules::gate_link(url, screen_now, running) {
        LinkGate::Plan(l) => l,
        LinkGate::Refuse(msg) => return refuse(msg),
        LinkGate::Hold(msg) => {
            tracing::info!(%msg, "deep link refused");
            set_status.call(msg);
            return;
        }
    };
    let known = KnownHosts::load();
    let plan = pf_client_core::orchestrate::plan_from_link(
        &link,
        &known,
        &pf_client_core::presets::PresetsFile::load(),
        &ctx.settings.lock().unwrap().clone(),
    );
    use pf_client_core::orchestrate::PlanOutcome;
    match plan {
        Ok(PlanOutcome::Connect(mut p)) => {
            // The plan's profile is the link's `as=`, else the saved pick: only the
            // link's own wins over the picker.
            p.profile = link.as_profile.clone();
            dial_link(ctx, &p, set_screen, set_status)
        }
        // A pinned host named by its label or address, which any web page could guess: arm
        // the confirmation, whose OK runs `dial_link` on this same plan. Not the PIN
        // ceremony: re-pairing would throw the pin away.
        Ok(PlanOutcome::ConfirmConnect(mut p)) => {
            p.profile = link.as_profile.clone();
            set_link_confirm.call(Some(p))
        }
        // Known but never pinned, or unknown: a link may not pair or trust on its own,
        // so it opens the PIN ceremony seeded with what it CLAIMED: the name as claimed,
        // the fingerprint pre-filling the pin (verified, not blind TOFU), and the launch,
        // preset and `as=` profile kept through the detour (§3.1, as in the GTK shell).
        Ok(PlanOutcome::ConfirmUnknown(u)) => {
            let name = u.name.clone().unwrap_or_else(|| u.addr.clone());
            // A link carries no mgmt port (nor a MAC), so those stay unknown until an advert
            // teaches them — same fallback as the hand-added case.
            *ctx.shared.target.lock().unwrap() = Target {
                host: HostTarget {
                    name: name.clone(),
                    addr: u.addr.clone(),
                    port: u.port,
                    fp_hex: u.fp.clone(),
                    ..HostTarget::default()
                },
                preset: u.preset.clone(),
                launch: u.launch.clone(),
                link_profile: u.profile.clone(),
                from_link: true,
                ..Target::default()
            };
            set_status.call(format!(
                "{name} isn't paired with this device yet \u{2014} pair it to continue."
            ));
            set_screen.call(Screen::Pair);
        }
        Ok(PlanOutcome::Unsupported(route)) => refuse(format!(
            "Punktfunk can't open \u{201c}{}\u{201d} links yet.",
            route.as_str()
        )),
        Err(e) => refuse(e.message()),
    }
}

/// Run a resolved link plan: the same four calls a host tile's click makes, so a link gets the
/// identical wake, trust and error surfaces rather than a second connect path of its own. Shared
/// by the two outcomes that dial — `PlanOutcome::Connect` (the link named the stable record id)
/// and a confirmed `PlanOutcome::ConfirmConnect` — so the confirmation is one click in front of
/// this, never a second implementation of it.
fn dial_link(
    ctx: &Arc<AppCtx>,
    plan: &pf_client_core::orchestrate::ConnectPlan,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let target = Target {
        host: plan.host.clone(),
        preset: plan.preset_override.clone(),
        launch: None, // routed explicitly below (initiate_launch*)
        link_profile: plan.profile.clone(),
        ..Target::default()
    };
    // With a MAC it takes the dial first wake path, so a sleeping host wakes instead of
    // erroring — exactly what clicking its tile would do. The link's `launch=` id must reach
    // the session (`--launch`) — this used to drop it, so a game link opened a plain desktop
    // session.
    match (
        plan.launch.clone(),
        plan.wake && !target.host.mac.is_empty(),
    ) {
        (Some(id), true) => {
            connect::initiate_launch_waking(ctx, target, id, set_screen, set_status);
        }
        (Some(id), false) => connect::initiate_launch(ctx, target, id, set_screen, set_status),
        (None, true) => connect::initiate_waking(ctx, target, set_screen, set_status),
        (None, false) => connect::initiate(ctx, target, set_screen, set_status),
    }
}
