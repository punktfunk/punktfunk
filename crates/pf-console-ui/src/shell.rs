//! Console shell: the screen stack, shared chrome, and modal overlays.
//!
//! Screens draw content. This module owns push/pop, the pinned title, controller
//! chip, hint bar, and connecting/wake/toast takeovers.
//!
//! A transition is one sprung 0→1 scalar; both layers composite through
//! `save_layer_alpha` so a screen fades as a unit. The backdrop crossfades when
//! the layers disagree (aurora ↔ form). Paint recipes live in `render.rs`.

use crate::anim::{springs, Spring};
use crate::glyphs::GlyphStyle;
use crate::library::LibraryShared;
use crate::model::{
    ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, SpeedPhase, SpeedStatus, WakeStatus,
};
use crate::platform::Platform;
#[cfg(test)]
use crate::pointer::DRAG_TICK_DP;
use crate::pointer::{Pointer, PointerKind, Touch};
use crate::screens::home::HomeScreen;
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use crate::store::SettingsStore;
use anyhow::Result;
use pf_client_core::console::OverlayAction;
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::start;
use pf_client_core::trust;
use skia_safe::{Canvas, Color4f, Image, Rect};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

mod backdrop;
mod connect;
mod overlays;
mod render;

use backdrop::Backdrop;
use connect::{Asking, Connecting, Launching, SeatWait};

/// Reduced-motion nav: 0.22 s, critically damped. `render.rs` draws it as a
/// crossfade (no slide, no scale). Instant swap would drop the only spatial cue.
const REDUCED_NAV: crate::anim::SpringSpec = crate::anim::SpringSpec {
    response: 0.22,
    damping: 1.0,
};
/// Push/pop slide, design units. Named so a test can pin it; a paint-site
/// literal cannot.
const NAV_SLIDE_DP: f64 = 36.0;
/// Incoming start scale on a push. Just under 1 so the arrival is felt.
const NAV_ENTER_SCALE: f64 = 0.985;
/// Outgoing/revealed scale. Deeper than enter: the screen being left reads further away.
const NAV_EXIT_SCALE: f64 = 0.96;
/// Revealed-screen alpha at the start of a pop. Not 0: it was already behind the leaving screen.
const NAV_REVEAL_ALPHA: f64 = 0.4;
/// Spring position at which non-Back input is accepted. Position, not elapsed time:
/// the question is whether the screen under the cursor is the one being aimed at.
const NAV_INPUT_OPENS: f64 = 0.85;
/// Chrome bands, design units: pinned title above, hints below.
const TOP_BAND: f64 = 88.0;
const BOTTOM_BAND: f64 = 86.0;
/// A tab switch slides the new root this share of the width.
pub(crate) const TAB_SLIDE: f64 = 0.25;
/// Seconds OK stays down on a remote before it opens the focused card's menu instead.
const HOLD_S: f64 = 0.5;

/// Top-level tabs, in strip order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tab {
    Hosts,
    Games,
    Players,
    Settings,
}

pub(crate) const TABS: [Tab; 4] = [Tab::Hosts, Tab::Games, Tab::Players, Tab::Settings];

impl Tab {
    /// The id `console-vectors.json` pins; also the pill's element id.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Tab::Hosts => "hosts",
            Tab::Games => "games",
            Tab::Players => "players",
            Tab::Settings => "settings",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Tab::Hosts => "Hosts",
            Tab::Games => "Games",
            // Named for what it lists today; a players feature would rename it back.
            Tab::Players => "Controllers",
            Tab::Settings => "Settings",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The tab a root screen belongs to.
    fn of(root: &Screen) -> Tab {
        match root {
            Screen::Library(_) => Tab::Games,
            Screen::Players(_) => Tab::Players,
            Screen::Settings(_) => Tab::Settings,
            _ => Tab::Hosts,
        }
    }
}

/// Paint recipe for a transition. Distinct from spring direction: a reversed
/// push still paints as a push.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NavKind {
    Push,
    Pop,
}

/// One transition as a sprung scalar, not a timer.
///
/// Back mid-push retargets this spring 1.0 → 0.0 with the current velocity.
/// A tween is elapsed time; reversing it is a snap or a second animation.
enum Motion {
    None,
    Nav {
        spring: Spring,
        /// 1.0 = completing, 0.0 = undoing. Only a push is retargeted to 0.0
        /// (see [`Shell::nav_back`]).
        target: f64,
        kind: NavKind,
        /// Screen no longer on the stack, still needed to paint. A pop always
        /// carries one. A REPLACE does too: `n - 2` would be the replaced
        /// screen's parent. A plain push does not — the parent stays at `n - 2`.
        leaving: Option<Box<Screen>>,
    },
    /// A tab switch: the new root slides in a quarter width over the parked one.
    Tab {
        spring: Spring,
        from: Tab,
    },
}

/// Toast severity: mark plus hairline. Error is the one kind that must not
/// take its colour from the palette.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Info,
    Success,
    /// Failure. Colour is fixed: a pale field's accent can be mint or orange.
    Error,
}

/// Mark ahead of toast text. Geometric on purpose: glyph art is Skia paths
/// that must read from 0.75× to 3× `k`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ToastMark {
    Dot,
    Check,
    Bang,
}

impl ToastKind {
    pub(crate) fn look(self) -> (Color4f, ToastMark) {
        match self {
            ToastKind::Info => (crate::theme::fg(0.55), ToastMark::Dot),
            ToastKind::Success => (crate::theme::accent(1.0), ToastMark::Check),
            // Fixed RGB: `moss` accent is green, `ember` is orange — either
            // would paint a failure in the "this is fine" colour.
            ToastKind::Error => (Color4f::new(0.93, 0.31, 0.28, 1.0), ToastMark::Bang),
        }
    }
}

struct Toast {
    text: String,
    at: f64,
    kind: ToastKind,
    /// Slide-in 0 → 1. Same spring spec as the keyboard tray.
    seat: Spring,
}

/// Seconds between `/status` reads while a shelf tile shows a live download.
const DOWNLOADS_POLL: f64 = 2.0;

/// Host-supplied construction options.
pub struct ConsoleOptions {
    /// Hostname registered as the default pairing device name.
    pub device_name: String,
    /// The About row's version, verbatim. `None` shows this kit's version: right where the
    /// app ships from this workspace.
    pub version: Option<String>,
    /// The device's own keyboard types into an open field: Steam's on a Steam Deck, tvOS's
    /// on an Apple TV. This shell never raises its keyboard tray.
    pub system_keyboard: bool,
    /// A TV (Apple TV, Android TV): rows for a clipboard or a phone's sensors do nothing.
    pub tv: bool,
    /// Host has another UI when the console is off (phone/tablet touch shell).
    /// False on desktop and Android TV — offering "off" would strand the user.
    pub fallback_ui: bool,
    /// This device's GPU decodes PyroWave. Answer from the same probe that gates the
    /// client's `CODEC_PYROWAVE` advertisement: a row that offers what the Hello never
    /// asks for is a setting that silently does nothing.
    pub pyrowave_ok: bool,
    /// This device decodes AV1 in hardware — the same answer that gates the client's
    /// `CODEC_AV1` advertisement (`pf_client_core::video::av1_advertised`). A host
    /// that learns it only once its GPU exists starts `true` and corrects it.
    pub av1_ok: bool,
    /// Settings and preset catalog. `None` uses the desktop file store
    /// (`pf_client_core::trust`); every other host must supply one.
    pub store: Option<Arc<dyn SettingsStore>>,
    /// Which settings rows exist.
    pub platform: Platform,
    /// Skia GPU resource-cache budget, bytes. Desktop default is
    /// [`DEFAULT_GPU_CACHE_BYTES`]; a memory-tight box may go down to
    /// [`MIN_GPU_CACHE_BYTES`] but never below it.
    pub gpu_cache_bytes: usize,
    /// This device's own screen, for the Aspect row. `None` where streams go to a window or a
    /// TV: only a panel of an unusual shape (a phone) changes what the row offers.
    pub screen: Option<DeviceScreen>,
    /// The host answers [`ConsoleCmd::FetchProfiles`], so a connect checks the box's profiles
    /// first. False connects with the card's saved pick, unchecked.
    pub profiles: bool,
}

/// A built-in screen in landscape pixels, whole and clear of its cutout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceScreen {
    pub full: (u32, u32),
    pub safe: (u32, u32),
}

impl ConsoleOptions {
    pub fn desktop(device_name: String, system_keyboard: bool) -> ConsoleOptions {
        ConsoleOptions {
            device_name,
            version: None,
            system_keyboard,
            tv: false,
            fallback_ui: false,
            // The desktop probe reads the session's Vulkan device, which the console does
            // not own yet. A GPU that runs this shell is a Vulkan 1.3 one, so it is yes.
            pyrowave_ok: true,
            // AV1 is not that safe an assumption, so the overlay corrects this from the
            // presenter's device (`SkiaOverlay::init`) before the first frame.
            av1_ok: true,
            store: None,
            platform: Platform::Desktop,
            gpu_cache_bytes: DEFAULT_GPU_CACHE_BYTES,
            screen: None,
            profiles: true,
        }
    }
}

/// Skia GPU resource-cache ceiling (not an allocation), bytes.
///
/// 160 MB. 64 MB sat under a full-grid working set (~100 MB of covers), so
/// `GrResourceCache` evicted a third every submit and the next frame
/// re-decoded JPEG on the render thread. A TV box passes its own through
/// [`ConsoleOptions::gpu_cache_bytes`].
pub const DEFAULT_GPU_CACHE_BYTES: usize = 160 << 20;

/// The floor under [`ConsoleOptions::gpu_cache_bytes`], bytes.
///
/// A screenful of covers plus the render targets is ~74 MB at `k` 1.35, the
/// scale a 1080p surface gives. Under that the cache evicts covers it is about
/// to draw again and the next frame re-decodes them on the render thread, which
/// on a television costs more than the frame. 96 MB leaves that ~30% of room.
/// It is a ceiling, not an allocation, and the shell hands its covers back
/// before a stream takes the GPU.
pub const MIN_GPU_CACHE_BYTES: usize = 96 << 20;

/// The full-screen card that owns input, in the order input checks them. Asking and
/// connecting coexist (the card shows after a beat), and a woken host goes on to connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Takeover {
    Launching,
    Asking,
    SeatWait,
    Connecting,
    Wake,
    Speed,
}

pub(crate) struct Shell {
    /// `stack[0]` is [`Self::tab`]'s root.
    stack: Vec<Screen>,
    tab: Tab,
    /// The other tabs' roots, kept while another tab is up. Indexed by [`Tab::index`].
    parked: [Option<Screen>; TABS.len()],
    /// The host key the Games root was built for.
    games_key: Option<String>,
    /// Whose list the shared library holds: the host of the last `FetchLibrary` sent.
    library_fp: Option<String>,
    /// The host the last `Pair` went to. The one pairing phase is its, so another host's
    /// Pair screen on top neither shows it nor closes on it.
    pairing: Option<(String, u16)>,
    /// Focus is on the tab strip, not the screen.
    strip_focus: bool,
    /// The shell moved focus to the strip because the root had nothing to focus. It goes
    /// back once the root has something, unless the strip is used meanwhile.
    strip_parked: bool,
    /// Focus targets the root placed when last painted; `None` until it paints.
    root_targets: Option<usize>,
    /// The strip's pills and focus plate.
    strip: crate::el::Tree,
    /// OK down on a remote: when, and whether the hold already fired.
    ok_down: Option<(f64, bool)>,
    motion: Motion,
    console: ConsoleShared,
    library: LibraryShared,
    /// When [`Shell::tick_downloads`] last asked the host.
    downloads_polled: f64,
    bus: ConsoleBus,
    actions: VecDeque<OverlayAction>,
    settings: trust::Settings,
    store: Arc<dyn SettingsStore>,
    /// This device. The overlay corrects `av1_ok` and `native_mode` in place.
    pub(crate) device: crate::screens::Device,
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    /// The host was last told the input test is on.
    pad_testing: bool,
    /// The `host_sort` / `host_grouping` values `hosts` was last arranged by.
    hosts_order: (Option<serde_json::Value>, Option<serde_json::Value>),
    pub(crate) in_stream: bool,
    connecting: Option<Connecting>,
    launching: Option<Launching>,
    /// Input waits while this is up; Back drops it.
    asking: Option<Asking>,
    /// Input waits while this is up; Back drops it.
    seat_wait: Option<SeatWait>,
    /// The dialed connect as it would ask again, while it might still come back
    /// `profile-unknown`. `None` once it streams, fails otherwise, or is itself the second ask.
    reask: Option<ConnectIntent>,
    /// Set by [`Self::profile_gone`]; the failure that follows asks again instead of toasting.
    reask_now: Option<ConnectIntent>,
    /// The ask in flight is that second ask.
    reasking: bool,
    /// Host title of the last connect. [`Self::session_reconnecting`] has no
    /// `Launch` of its own, so nothing else can name the host.
    last_connect_title: Option<String>,
    wake: Option<WakeStatus>,
    /// `wake` is a local placeholder raised in [`Self::apply`] before the
    /// first `WakeStatus` (~100 ms–1 s). `sync` must not clear it in that
    /// window or navigation races the wake ungated.
    wake_optimistic: bool,
    /// The speed-test takeover. No optimistic twin: [`Self::apply`] seeds the shared slot
    /// itself, so the service thread only ever advances the phase and `sync` can mirror
    /// the slot verbatim — including the `None` a dismiss writes.
    speed: Option<SpeedStatus>,
    /// The speed chart as drawn, chasing `speed` every frame.
    speed_view: overlays::SpeedView,
    toast: Option<Toast>,
    /// Fingerprint of a first pairing whose shelf has not opened yet. See
    /// [`Self::open_first_paired_library`].
    first_pair: Option<String>,
    /// The field behind every screen and takeover.
    backdrop: Backdrop,
    glyphs: GlyphStyle,
    /// Last input device (pad or keys), noted at [`Shell::note_input_source`]
    /// and [`Shell::key`]. `None` until anything drives: then the connected
    /// pad, or the platform's key device.
    input_source: Option<crate::console::InputSource>,
    chip: Option<String>,
    pads: Vec<PadInfo>,
    /// Settled top screen's hint-bar hit boxes, from [`Shell::render`].
    /// For a pointer, which has no face buttons, this *is* the button bar.
    hint_rects: Vec<(crate::glyphs::HintKey, Rect)>,
    /// (left, top) inset of the last layout. Pointer coords arrive in surface
    /// pixels; hit boxes were published in this space.
    last_insets: (f32, f32),
    /// Full surface of the last layout, insets included. A backdrop paints here,
    /// not in the safe rect, or it seams at the cutout edge.
    last_full: (f32, f32),
    /// Design-unit scale of the last frame. Touch slop and drag ticks grow with it.
    last_k: f64,
    /// Finger state. The stream overlay resets it while the ring owns the pointer.
    pub(crate) touch: Touch,
    pub(crate) gpu_cache_bytes: usize,
    t0: Instant,
    last_frame: Option<Instant>,
    /// Last menu, pointer, key or text input — the idle clock ([`crate::console::IDLE_AFTER`]).
    last_input: Instant,
    /// Test-only `(t, step)`: clock reads `t` and each frame adds `step`.
    /// The aurora phase *is* the clock; wall time never agrees across dumps.
    #[cfg(test)]
    pub(crate) fake_clock: Option<(f64, f64)>,
}

impl Shell {
    pub(crate) fn new(
        console: ConsoleShared,
        library: LibraryShared,
        bus: ConsoleBus,
        opts: ConsoleOptions,
        stack: Vec<Screen>,
    ) -> Result<Shell> {
        anyhow::ensure!(!stack.is_empty(), "the console needs a root screen");
        let store: Arc<dyn SettingsStore> = match opts.store {
            Some(store) => store,
            None => {
                #[cfg(any(target_os = "linux", windows))]
                {
                    Arc::new(crate::store::FileSettingsStore)
                }
                #[cfg(not(any(target_os = "linux", windows)))]
                {
                    anyhow::bail!("the console needs a settings store on this platform")
                }
            }
        };
        let settings = store.load();
        let backdrop = Backdrop::new(
            &settings.ui_palette,
            match stack.last().expect("non-empty").background() {
                Bg::Aurora => 0.0,
                Bg::Form => 1.0,
            },
        )?;
        Ok(Shell {
            tab: Tab::of(&stack[0]),
            stack,
            parked: [None, None, None, None],
            games_key: None,
            library_fp: None,
            pairing: None,
            strip_focus: false,
            strip_parked: false,
            root_targets: None,
            strip: crate::el::Tree::new(),
            ok_down: None,
            motion: Motion::None,
            console,
            library,
            downloads_polled: f64::NEG_INFINITY,
            bus,
            actions: VecDeque::new(),
            settings,
            store,
            device: crate::screens::Device {
                platform: opts.platform,
                screen: opts.screen,
                native_mode: None,
                system_keyboard: opts.system_keyboard,
                tv: opts.tv,
                fallback_ui: opts.fallback_ui,
                pyrowave_ok: opts.pyrowave_ok,
                av1_ok: opts.av1_ok,
                profiles: opts.profiles,
                name: opts.device_name,
                version: opts.version.unwrap_or_else(|| crate::VERSION.into()),
            },
            hosts: Vec::new(),
            hosts_gen: u64::MAX,
            hosts_order: (None, None),
            pad_testing: false,
            in_stream: false,
            connecting: None,
            launching: None,
            asking: None,
            seat_wait: None,
            reask: None,
            reask_now: None,
            reasking: false,
            last_connect_title: None,
            wake: None,
            wake_optimistic: false,
            speed: None,
            speed_view: overlays::SpeedView::default(),
            toast: None,
            first_pair: None,
            backdrop,
            glyphs: GlyphStyle::Keyboard,
            input_source: None,
            chip: None,
            pads: Vec::new(),
            hint_rects: Vec::new(),
            last_insets: (0.0, 0.0),
            last_full: (0.0, 0.0),
            last_k: 1.0,
            touch: Touch::default(),
            gpu_cache_bytes: opts.gpu_cache_bytes,
            t0: Instant::now(),
            last_frame: None,
            last_input: Instant::now(),
            #[cfg(test)]
            fake_clock: None,
        })
    }

    /// The screen on top of the stack.
    pub(crate) fn top(&self) -> Option<&Screen> {
        self.stack.last()
    }

    /// Push a screen the host asked for ([`crate::console::Console::prompt`]).
    pub(crate) fn push_screen(&mut self, screen: Screen) {
        self.apply_nav(Nav::Push(Box::new(screen)));
    }

    /// Replace the stack (deep link, return-to-shelf). Cut, no transition:
    /// this is re-entry, not navigation the user watched.
    pub(crate) fn replace_stack(&mut self, stack: Vec<Screen>) {
        if stack.is_empty() {
            return;
        }
        let tab = Tab::of(&stack[0]);
        if tab != self.tab {
            self.parked[self.tab.index()] = self.stack.drain(..).next();
        }
        self.parked[tab.index()] = None;
        self.tab = tab;
        self.strip_focus = false;
        self.root_targets = None;
        self.stack = stack;
        self.motion = Motion::None;
        self.backdrop.bg_mix = match self.stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
    }

    /// Host pointer events through the shared touch model ([`Touch`]): a finger acts
    /// on its lift or scrolls, a mouse acts on press.
    pub(crate) fn pointer_input(&mut self, input: pf_client_core::console::PointerInput) -> bool {
        self.last_input = Instant::now();
        let now = self.t();
        let mut touch = std::mem::take(&mut self.touch);
        let consumed = touch.feed(input, self.last_k, now, |p| self.pointer(p));
        self.touch = touch;
        consumed
    }

    /// Once a frame: a finger held still becomes a long press.
    pub(crate) fn tick_touch(&mut self) {
        let now = self.t();
        let mut touch = std::mem::take(&mut self.touch);
        touch.tick(now, |p| self.pointer(p));
        self.touch = touch;
    }

    /// Host session edge. `Connecting` is a no-op: the shell already showed
    /// the takeover when it raised Launch.
    pub(crate) fn session_phase(&mut self, phase: pf_client_core::console::SessionPhase) {
        use pf_client_core::console::SessionPhase;
        match phase {
            SessionPhase::Connecting => {}
            SessionPhase::Streaming => self.session_streaming(),
            SessionPhase::Failed(msg) => self.session_failed(msg),
            SessionPhase::Ended(reason) => self.session_ended(reason),
            SessionPhase::Reconnecting(msg) => self.session_reconnecting(msg),
        }
    }

    fn t(&self) -> f64 {
        #[cfg(test)]
        if let Some((t, _)) = self.fake_clock {
            return t;
        }
        self.t0.elapsed().as_secs_f64()
    }

    /// Nothing to back out of: one screen, focus on its tab, no modal, no takeover. A host
    /// whose Back belongs to the system when the console does not want it (tvOS's Menu)
    /// asks before it binds.
    pub(crate) fn at_root(&self) -> bool {
        self.stack.len() == 1 && self.strip_focus && self.takeover().is_none()
    }

    /// The takeover that owns input: the first one up, in [`Takeover`]'s order.
    pub(crate) fn takeover(&self) -> Option<Takeover> {
        let up = [
            (self.launching.is_some(), Takeover::Launching),
            (self.asking.is_some(), Takeover::Asking),
            (self.seat_wait.is_some(), Takeover::SeatWait),
            (self.connecting.is_some(), Takeover::Connecting),
            (self.wake.is_some(), Takeover::Wake),
            (self.speed.is_some(), Takeover::Speed),
        ];
        up.into_iter().find_map(|(on, t)| on.then_some(t))
    }

    pub(crate) fn editing(&self) -> bool {
        !self.in_stream
            && self.connecting.is_none()
            && !self.holds_stream()
            && self.stack.last().is_some_and(|s| s.view().editing())
    }

    pub(crate) fn edit_field(&self) -> Option<crate::screens::EditField> {
        if !self.editing() {
            return None;
        }
        self.stack.last()?.view().edit_field()
    }

    /// What a screen reader should speak for the focused row. `None` while a takeover owns
    /// the input, or on a screen that names no focus.
    /// `&mut` only to hand `Ctx` the settings it wants by `&mut`; nothing on this
    /// path writes them. A host polls this once per frame, so cloning the settings
    /// to get an `&self` here would be ~9 string allocations per frame on a still
    /// screen — the per-frame-work-that-changes-nothing shape this shell has
    /// already paid to remove once.
    pub(crate) fn focus_announcement(&mut self) -> Option<String> {
        if self.in_stream || self.takeover().is_some() {
            return None;
        }
        if self.strip_focus && self.stack.len() == 1 {
            return Some(format!("{} tab", self.tab.name()));
        }
        let (ctx, screen) = self.ctx_and_top();
        screen.view().announcement(&ctx)
    }

    /// The top screen and a [`Ctx`] over the rest of the shell, borrowed apart.
    fn ctx_and_top(&mut self) -> (Ctx<'_>, &mut Screen) {
        let t = self.t();
        let ctx = Ctx {
            hosts: &self.hosts,
            library: &self.library,
            settings: &mut self.settings,
            store: &*self.store,
            pads: &self.pads,
            device: &self.device,
            t,
        };
        (ctx, self.stack.last_mut().expect("non-empty stack"))
    }

    /// The console is covering a live stream — a launch hold — and wants the
    /// pad as menu events, masked off the wire.
    pub(crate) fn holds_stream(&self) -> bool {
        self.launching.is_some()
    }

    /// No input for [`crate::console::IDLE_AFTER`].
    pub(crate) fn idle(&self) -> bool {
        self.last_input.elapsed() >= crate::console::IDLE_AFTER
    }

    pub(crate) fn take_action(&mut self) -> Option<OverlayAction> {
        self.actions.pop_front()
    }

    pub(crate) fn device_name(&self) -> &str {
        &self.device.name
    }

    /// The OS's answer when the host read one, else the console's own row.
    pub(crate) fn reduce_motion(&self) -> bool {
        crate::os_theme::os_reduce_motion().unwrap_or(self.settings.reduce_motion)
    }

    fn show_toast(&mut self, text: String) {
        self.show_toast_kind(text, ToastKind::Info);
    }

    fn show_toast_kind(&mut self, text: String, kind: ToastKind) {
        self.toast = Some(Toast {
            text,
            at: self.t(),
            kind,
            seat: Spring::rest(0.0),
        });
    }

    fn sync(&mut self) {
        self.backdrop.sync(&self.settings);
        self.sync_hosts();
        if let Some(text) = self.console.take_notice() {
            self.show_toast(text);
        }
        self.feed_top();
        self.sync_pair();
        self.open_first_paired_library();
        self.sync_wake();
        self.tick_asking();
        self.tick_seat_wait();
        self.home_shelf();
        self.tick_launch();
        self.tick_downloads();
        self.settle_focus();
    }

    /// While the shelf on top shows a live download and no launch hold polls already, re-read
    /// `/status` every [`DOWNLOADS_POLL`] so its tile's percentage moves.
    fn tick_downloads(&mut self) {
        let t = self.t();
        if self.launching.is_some() || t - self.downloads_polled < DOWNLOADS_POLL {
            return;
        }
        if !self.library.any_live_download() {
            return;
        }
        let Some(lib) = self.stack.last().and_then(Screen::shelf) else {
            return;
        };
        self.downloads_polled = t;
        self.bus.send(ConsoleCmd::RefreshRunning {
            addr: lib.host_addr().to_string(),
            mgmt: lib.host_mgmt_port(),
            fp_hex: lib.host_fp_hex().to_string(),
        });
    }

    /// The row's order is a setting too: re-arrange when either the list or it moves.
    fn sync_hosts(&mut self) {
        let order = (
            self.settings
                .extra
                .get(crate::screens::home::HOST_SORT_KEY)
                .cloned(),
            self.settings
                .extra
                .get(crate::screens::home::HOST_GROUPING_KEY)
                .cloned(),
        );
        if self.console.hosts_gen() != self.hosts_gen || order != self.hosts_order {
            (self.hosts, self.hosts_gen) = self.console.hosts_snapshot();
            crate::screens::home::arrange(&mut self.hosts, &self.settings);
            self.hosts_order = order;
        }
    }

    /// Feeds the top screen what the service reported: pad test, other players, licences.
    /// The host's test mode follows the test screen, whatever took it off the top.
    fn feed_top(&mut self) {
        let testing = matches!(self.stack.last(), Some(Screen::InputTest(_))) && !self.in_stream;
        if testing != self.pad_testing {
            self.pad_testing = testing;
            self.bus.send(ConsoleCmd::PadTest { on: testing });
        }
        let t = self.t();
        if let Some(Screen::InputTest(test)) = self.stack.last_mut() {
            if let Some(state) = self.console.take_pad_test() {
                test.set_state(state, t);
            }
            if test.done {
                self.apply_nav(Nav::Pop);
            }
        }
        if let Some(Screen::Players(p)) = self.stack.last_mut() {
            p.others = self.console.other_devices();
        }
        if let Some(Screen::Licenses(l)) = self.stack.last_mut() {
            if l.waiting() {
                if let Some(sections) = self.console.licenses() {
                    l.set_host(sections.as_ref().clone());
                }
            }
        }
        if let Some(Screen::Profiles(p)) = self.stack.last_mut() {
            if p.waiting() {
                if let Some(answer) = self.console.take_profiles(p.fp_hex()) {
                    p.set_answer(answer);
                }
            }
        }
    }

    fn sync_pair(&mut self) {
        let pair = self.console.pair();
        let pairing = self.pairing.clone();
        let ours = |s: &Screen| match (s, &pairing) {
            (Screen::Pair(p), Some((addr, port))) => p.target() == (addr.as_str(), *port),
            _ => false,
        };
        let top_is_ours = self.stack.last().is_some_and(ours);
        match &pair {
            PairPhase::Idle => {}
            PairPhase::Paired { key } => {
                let name = self
                    .hosts
                    .iter()
                    .find(|h| &h.key == key)
                    .map_or_else(|| "the host".to_string(), |h| h.name.clone());
                self.show_toast_kind(format!("Paired with {name}"), ToastKind::Success);
                self.console.set_pair(PairPhase::Idle);
                self.pairing = None;
                if top_is_ours {
                    self.apply_nav(Nav::Pop);
                }
                // The first pairing is where the default host comes into being, so show
                // the user where the next launch will land. A later pairing resolves to
                // no default and leaves the toast and the list exactly as they are.
                if self.pairing_made_the_default(key) {
                    self.first_pair = Some(key.clone());
                }
            }
            phase => {
                if let (true, Some(Screen::Pair(p))) = (top_is_ours, self.stack.last_mut()) {
                    p.apply_phase(phase);
                }
                if matches!(phase, PairPhase::Failed(_)) {
                    self.console.set_pair(PairPhase::Idle);
                    self.pairing = None;
                }
            }
        }
    }

    /// Mirrors the service's wake and speed status. A woken host with `then_connect` goes
    /// straight into its connect, once: the slot is cleared here, not at the service's next
    /// bus drain.
    fn sync_wake(&mut self) {
        match self.console.wake() {
            Some(w) => {
                self.wake_optimistic = false;
                self.wake = Some(w);
            }
            // No service status yet: keep the placeholder. Clearing it here
            // reopens the ungated window it exists to close.
            None if !self.wake_optimistic => self.wake = None,
            None => {}
        }
        self.speed = self.console.speed();
        if let Some(w) = &self.wake {
            if w.online {
                let intent = w.then_connect.then(|| {
                    self.hosts
                        .iter()
                        .find(|h| h.key == w.key)
                        .map(|h| ConnectIntent::to_host(h, None))
                });
                self.bus.send(ConsoleCmd::CancelWake);
                self.console.set_wake(None);
                self.wake = None;
                if let Some(Some(intent)) = intent {
                    self.start_connect(intent);
                    // Wake takeover was already full-screen; skip the connect
                    // fade so home does not flash through.
                    if let Some(c) = &mut self.connecting {
                        c.appear = 1.0;
                    }
                }
            }
        }
    }

    /// A root with nothing to focus parks focus on the tab strip; once it has something
    /// again, a parked focus goes back.
    fn settle_focus(&mut self) {
        if self.stack.len() > 1 {
            return;
        }
        match self.root_targets {
            Some(0) if !self.strip_focus => (self.strip_focus, self.strip_parked) = (true, true),
            Some(n) if n > 0 && self.strip_parked => {
                (self.strip_focus, self.strip_parked) = (false, false);
            }
            _ => {}
        }
    }

    /// Fetch the games under the Hosts row once it rests on a host. Lives here: a screen
    /// cannot send while it draws.
    fn home_shelf(&mut self) {
        let Some(Screen::Home(home)) = self.stack.last_mut() else {
            return;
        };
        let Some(host) = home.wants_shelf(&self.hosts, self.library_fp.as_deref()) else {
            return;
        };
        let host = host.clone();
        home.set_shelf(crate::screens::library::LibraryScreen::embedded(&host));
        self.note_fetch(&host.fp_hex);
        self.bus.send(ConsoleCmd::FetchLibrary {
            addr: host.addr.clone(),
            mgmt: host.mgmt_port,
            fp_hex: host.fp_hex.clone(),
        });
    }

    /// This pairing is what turned its host into the default one. False for a second or
    /// later pairing, which leaves several paired records and so no derived default, and
    /// false under `start_in = hosts`, where a launch stays on the list by choice.
    fn pairing_made_the_default(&self, key: &str) -> bool {
        if start::StartIn::parse(&self.settings.start_in) == start::StartIn::Hosts {
            return false;
        }
        let known = self.store.known_hosts();
        let paired = known.hosts.iter().position(|h| h.fp_hex == key);
        paired.is_some() && start::default_host(&self.settings, &known) == paired
    }

    /// Hand a completed first pairing to that host's shelf. Deferred by a frame or two:
    /// the carousel row keyed by the new fingerprint arrives after the pairing does.
    /// Dropped once the user navigates — this is a handoff, not a queued action.
    fn open_first_paired_library(&mut self) {
        let Some(key) = self.first_pair.take() else {
            return;
        };
        if !matches!(self.stack.as_slice(), [Screen::Home(_)]) {
            return;
        }
        let Some(row) = self.hosts.iter().find(|h| h.key == key).cloned() else {
            self.first_pair = Some(key);
            return;
        };
        let root = self.shelf_root(&row);
        self.mount(Tab::Games, root);
    }

    /// The strip's share of a menu event: L1/R1 from anywhere but a text field, every
    /// direction while the strip has focus. `None` leaves the event to the screen. Over a
    /// root with nothing to focus, Down stays on the strip and OK is the screen's.
    fn tab_menu(&mut self, ev: MenuEvent) -> Option<Option<MenuPulse>> {
        let editing = self.stack.last().is_some_and(|s| s.view().editing());
        match ev {
            MenuEvent::JumpBack if !editing => return Some(self.step_tab(-1)),
            MenuEvent::JumpForward if !editing => return Some(self.step_tab(1)),
            _ if !self.strip_focus || self.stack.len() > 1 => return None,
            _ => {}
        }
        self.strip_parked = false;
        let empty = self.root_targets == Some(0);
        match ev {
            MenuEvent::Move(MenuDir::Left) => Some(self.step_tab(-1)),
            MenuEvent::Move(MenuDir::Right) => Some(self.step_tab(1)),
            MenuEvent::Move(MenuDir::Down) if empty => Some(Some(MenuPulse::Boundary)),
            MenuEvent::Confirm if empty => None,
            MenuEvent::Move(MenuDir::Down) | MenuEvent::Confirm => {
                self.strip_focus = false;
                if let Some(s) = self.stack.last_mut() {
                    s.enter_from_top();
                }
                Some(Some(MenuPulse::Move))
            }
            MenuEvent::Move(MenuDir::Up) => Some(Some(MenuPulse::Boundary)),
            MenuEvent::Back => Some(self.ask_exit()),
            _ => Some(None),
        }
    }

    /// The next tab `delta` along the strip that has something to show.
    fn step_tab(&mut self, delta: i32) -> Option<MenuPulse> {
        let mut i = self.tab.index() as i32;
        loop {
            i += delta;
            let Some(&tab) = usize::try_from(i).ok().and_then(|i| TABS.get(i)) else {
                return Some(MenuPulse::Boundary);
            };
            if self.switch_tab(tab) {
                return Some(MenuPulse::Move);
            }
        }
    }

    /// Make `to` the active tab. `false` when it has nothing to show.
    fn switch_tab(&mut self, to: Tab) -> bool {
        if to == self.tab {
            return false;
        }
        match self.tab_root(to) {
            Some(root) => {
                self.mount(to, root);
                true
            }
            None => false,
        }
    }

    /// `root` becomes the stack; the current root parks and a pushed screen is dropped.
    /// What it can focus is unknown until it paints.
    fn mount(&mut self, to: Tab, root: Screen) {
        self.stack.truncate(1);
        self.parked[self.tab.index()] = self.stack.pop();
        self.stack.push(root);
        self.root_targets = None;
        let from = std::mem::replace(&mut self.tab, to);
        self.motion = Motion::Tab {
            spring: Spring::rest(0.0),
            from,
        };
    }

    /// `tab`'s parked root, or a fresh one. Games needs a paired host and follows the one
    /// focused on Hosts.
    fn tab_root(&mut self, tab: Tab) -> Option<Screen> {
        let parked = self.parked[tab.index()].take();
        match tab {
            Tab::Hosts => Some(parked.unwrap_or_else(|| Screen::Home(HomeScreen::new()))),
            Tab::Players => {
                Some(parked.unwrap_or_else(|| {
                    Screen::Players(crate::screens::players::PlayersScreen::new())
                }))
            }
            Tab::Settings => Some(parked.unwrap_or_else(|| {
                Screen::Settings(crate::screens::settings::SettingsScreen::new(&*self.store))
            })),
            Tab::Games => {
                let Some(host) = self.games_host() else {
                    self.parked[tab.index()] = parked;
                    return None;
                };
                // The shared list is one host's: a root whose host is not the last fetched
                // would show another host's games.
                let mine = self.library_fp.as_deref() == Some(host.fp_hex.as_str());
                match parked {
                    Some(root) if mine && self.games_key.as_deref() == Some(host.key.as_str()) => {
                        Some(root)
                    }
                    _ => Some(self.shelf_root(&host)),
                }
            }
        }
    }

    /// Tour the tabs once into `canvas`, off the glass, so the GPU programs they use compile
    /// now, behind the host's splash, instead of on the first visit to each. It switches tabs
    /// the way a player does, from a Hosts home through Games, Players and Settings, on a
    /// stand-in library with covers and a bus nobody reads, then puts every part of the shell
    /// back: nothing reaches the host, and the first real visits build and fetch as before.
    /// `frame_done` runs after every frame of the tour — see [`crate::Console::warm_up`].
    pub(crate) fn warm_up(
        &mut self,
        canvas: &Canvas,
        viewport: &crate::console::Viewport,
        fonts: &crate::theme::Fonts,
        frame_done: &mut dyn FnMut(&Canvas),
    ) {
        let tab = std::mem::replace(&mut self.tab, Tab::Hosts);
        let stack = std::mem::replace(&mut self.stack, vec![Screen::Home(HomeScreen::new())]);
        let parked = std::mem::take(&mut self.parked);
        let motion = std::mem::replace(&mut self.motion, Motion::None);
        let strip_focus = std::mem::replace(&mut self.strip_focus, false);
        let keys = (self.games_key.take(), self.library_fp.take());
        let bus = std::mem::take(&mut self.bus);
        let library = std::mem::take(&mut self.library);
        // A host embedding this shell ticks its model after the warm-up, so no host is known
        // yet: without a stand-in, Games and its shelf would never draw here.
        let hosts = (
            std::mem::replace(&mut self.hosts, vec![stand_in_host()]),
            self.hosts_gen,
        );
        self.hosts_gen = self.console.hosts_gen();
        self.library.set_games(stand_in_games());
        let poster = stand_in_poster();
        let mut draw = |s: &mut Shell, frames: usize| {
            for _ in 0..frames {
                s.render_in(canvas, viewport, fonts, None, None, &[]);
                if let Some(p) = &poster {
                    s.warm_shelves(p);
                }
                frame_done(canvas);
            }
        };
        draw(self, 3);
        for t in [Tab::Games, Tab::Players, Tab::Settings, Tab::Hosts] {
            if self.switch_tab(t) {
                draw(self, 12);
            }
        }
        (self.tab, self.stack, self.parked, self.motion) = (tab, stack, parked, motion);
        (self.strip_focus, self.bus, self.library) = (strip_focus, bus, library);
        (self.games_key, self.library_fp) = keys;
        // The saved generation, so a list published meanwhile is still taken on the next sync.
        (self.hosts, self.hosts_gen) = hosts;
        crate::el::forget_handoff();
    }

    /// Warm-up only: every shelf drawn or parked shows the stand-in cover, entrance over.
    fn warm_shelves(&mut self, poster: &Image) {
        for screen in self
            .stack
            .iter_mut()
            .chain(self.parked.iter_mut().flatten())
        {
            match screen {
                Screen::Library(l) => l.warm(poster),
                Screen::Home(h) => h.shelf_mut().into_iter().for_each(|l| l.warm(poster)),
                _ => {}
            }
        }
    }

    /// The host Games shows: the one focused on Hosts when it is paired, else the first
    /// paired one.
    fn games_host(&self) -> Option<HostRow> {
        let home = self
            .stack
            .first()
            .into_iter()
            .chain(self.parked[Tab::Hosts.index()].as_ref())
            .find_map(|s| match s {
                Screen::Home(h) => Some(h),
                _ => None,
            });
        let usable = |h: &&HostRow| h.paired && h.saved;
        home.and_then(HomeScreen::focused_key)
            .and_then(|k| self.hosts.iter().find(|h| h.key == k))
            .filter(usable)
            .or_else(|| self.hosts.iter().find(usable))
            .cloned()
    }

    /// A library fetch for `fp_hex` is about to go out. Another host's list leaves the model
    /// now, before a shelf pushed with the fetch syncs it as its own. Call it before the send:
    /// after, it could wipe what the platform's fetch already delivered.
    pub(crate) fn note_fetch(&mut self, fp_hex: &str) {
        if self.library_fp.as_deref() != Some(fp_hex) {
            self.library.begin_host_fetch();
        }
        self.library_fp = Some(fp_hex.to_string());
    }

    /// A fresh shelf for `host`, its fetch sent.
    fn shelf_root(&mut self, host: &HostRow) -> Screen {
        self.note_fetch(&host.fp_hex);
        self.bus.send(ConsoleCmd::FetchLibrary {
            addr: host.addr.clone(),
            mgmt: host.mgmt_port,
            fp_hex: host.fp_hex.clone(),
        });
        self.games_key = Some(host.key.clone());
        Screen::Library(crate::screens::library::LibraryScreen::new(host))
    }

    /// OK from a remote, both edges. A press acts on release; held [`HOLD_S`] it opens the
    /// focused card's menu, as Y does on a pad. In a text field the press types at once and
    /// never holds: the keyboard has no menu, and its Secondary closes the field.
    pub(crate) fn ok(&mut self, down: bool) -> Option<MenuPulse> {
        self.last_input = Instant::now();
        if self.editing() {
            self.ok_down = None;
            return down.then(|| self.handle_menu(MenuEvent::Confirm)).flatten();
        }
        let t = self.t();
        if down {
            // A fresh press restarts the hold, so a lost release cannot strand it.
            self.ok_down = Some((t, false));
            self.dip();
            return None;
        }
        match self.ok_down.take() {
            Some((_, false)) => {
                let pulse = self.menu_event(MenuEvent::Confirm);
                self.felt(pulse)
            }
            _ => None,
        }
    }

    /// The pulse a pad may play: none while Controller rumble is off.
    pub(crate) fn felt(&self, pulse: Option<MenuPulse>) -> Option<MenuPulse> {
        pulse.filter(|_| self.settings.pad_rumble)
    }

    /// OK went down on what has focus: its plate and the element dip. Under a takeover
    /// nothing dips: its card has the input, and the screen beneath is hidden.
    fn dip(&mut self) {
        if self.takeover().is_some() {
            return;
        }
        if self.strip_focus && self.stack.len() == 1 {
            self.strip.press();
        } else if let Some(s) = self.stack.last_mut() {
            s.view_mut().press();
        }
    }

    /// Once a frame: an OK held long enough becomes the hold.
    fn tick_ok(&mut self) {
        if let Some((t0, false)) = self.ok_down {
            if self.t() - t0 >= HOLD_S {
                self.ok_down = Some((t0, true));
                self.handle_menu(MenuEvent::Secondary);
            }
        }
    }

    /// A menu event from a pad, the keys, or a clicked hint.
    pub(crate) fn handle_menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        // A pad's A dips what it acts on; a remote's OK dipped on its way down.
        if ev == MenuEvent::Confirm {
            self.dip();
        }
        let pulse = self.menu_event(ev);
        self.felt(pulse)
    }

    /// [`Self::handle_menu`] without the Confirm dip.
    fn menu_event(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        self.last_input = Instant::now();
        self.sync();
        if let Some(t) = self.takeover() {
            return self.takeover_menu(t, ev);
        }
        // Back is always heard by the transition (`nav_back`). Other events
        // wait until the spring is past `NAV_INPUT_OPENS` so a double-tapped
        // A cannot push two screens. Threshold is position, not elapsed time.
        if !matches!(self.motion, Motion::None) {
            if ev == MenuEvent::Back {
                if self.nav_back() {
                    return Some(MenuPulse::Confirm);
                }
            } else if self.nav_pos() < NAV_INPUT_OPENS {
                return None;
            }
        }
        if let Some(pulse) = self.tab_menu(ev) {
            return pulse;
        }

        let mut fx = Outbox::default();
        let pulse = {
            let (mut ctx, top) = self.ctx_and_top();
            top.view_mut().menu(ev, &mut ctx, &mut fx)
        };
        // Up that a root screen bumps or leaves unanswered lands on its tab.
        let to_strip = self.stack.len() == 1
            && ev == MenuEvent::Move(MenuDir::Up)
            && matches!(pulse, Some(MenuPulse::Boundary) | None)
            && fx.nav.is_none()
            && !self.stack[0].view().editing();
        self.apply(fx);
        if to_strip {
            self.strip_focus = true;
            return Some(MenuPulse::Move);
        }
        pulse
    }

    /// A menu event while takeover `t` owns the input. B drops every card; the launch
    /// hold before its dial lands and the connect card also cancel the dial. After the
    /// dial lands, A or B on the launch hold shows the stream.
    fn takeover_menu(&mut self, t: Takeover, ev: MenuEvent) -> Option<MenuPulse> {
        match (t, ev) {
            (Takeover::Launching, MenuEvent::Confirm | MenuEvent::Back)
                if self.launching.as_ref().is_some_and(|l| l.connected) =>
            {
                self.reveal_stream();
            }
            (Takeover::Launching, MenuEvent::Back) => {
                self.launching = None;
                self.actions.push_back(OverlayAction::CancelConnect);
            }
            // Dropped here, not on the next `session_phase`: a dial blocked on the host
            // never sends one. Hosts handle a dial that lands after the cancel.
            (Takeover::Connecting, MenuEvent::Back) => {
                self.connecting = None;
                self.actions.push_back(OverlayAction::CancelConnect);
            }
            // Nothing has dialed yet: no cancel to send.
            (Takeover::Asking, MenuEvent::Back) => {
                self.asking = None;
                self.connecting = None;
            }
            (Takeover::SeatWait, MenuEvent::Back) => self.seat_wait = None,
            (Takeover::Wake, MenuEvent::Back) => {
                self.bus.send(ConsoleCmd::CancelWake);
                self.wake = None;
                self.wake_optimistic = false;
            }
            (Takeover::Wake, MenuEvent::Confirm) => {
                let w = self.wake.as_ref().filter(|w| w.timed_out)?;
                self.bus.send(ConsoleCmd::Wake {
                    key: w.key.clone(),
                    then_connect: w.then_connect,
                });
            }
            // Dismissing mid-burst abandons the measurement, not the burst: the host
            // finishes it either way, and `advance_speed` drops the late report.
            (Takeover::Speed, MenuEvent::Back) => self.close_speed(),
            (Takeover::Speed, MenuEvent::Confirm) => {
                let kbps = self.speed_recommendation()?;
                let text = self.apply_speed_bitrate(kbps);
                self.close_speed();
                self.show_toast(text);
            }
            _ => return None,
        }
        Some(MenuPulse::Confirm)
    }

    /// Mouse and touch, device pixels. `true` = consumed.
    ///
    /// Same modal/motion precedence as [`Self::handle_menu`]. The hint bar
    /// sits above the screens: a pointer has no face buttons.
    pub(crate) fn pointer(&mut self, p: Pointer) -> bool {
        self.sync();
        // Surface pixels → last frame's safe-area space.
        let p = Pointer {
            x: p.x - f64::from(self.last_insets.0),
            y: p.y - f64::from(self.last_insets.1),
            kind: p.kind,
        };
        // A takeover swallows every pointer event: clicking through a connect card onto
        // the library would start a second session. Right button is its B.
        match self.takeover() {
            Some(Takeover::Launching) => {
                let connected = self.launching.as_ref().is_some_and(|l| l.connected);
                if connected && (p.press() || p.kind == PointerKind::Back) {
                    self.reveal_stream();
                } else if !connected && p.kind == PointerKind::Back {
                    self.launching = None;
                    self.actions.push_back(OverlayAction::CancelConnect);
                }
                return true;
            }
            Some(_) if p.kind == PointerKind::Back => {
                self.handle_menu(MenuEvent::Back);
                return true;
            }
            // Cancel is the one button on the seat wait; the rest of it is not clickable.
            Some(Takeover::SeatWait) => {
                let on_cancel = self
                    .hint_rects
                    .iter()
                    .any(|(key, r)| *key == crate::glyphs::HintKey::Back && p.hits(*r));
                if p.press() && on_cancel {
                    self.handle_menu(MenuEvent::Back);
                }
                return true;
            }
            Some(_) => return true,
            None => {}
        }
        // Right button is B, but not on a root: a right-click there is too easy to fire
        // by accident.
        if p.kind == PointerKind::Back {
            if self.stack.len() > 1 {
                self.handle_menu(MenuEvent::Back);
            }
            return true;
        }
        if !matches!(self.motion, Motion::None) {
            return true;
        }
        match p.kind {
            // The pad's Secondary on whatever the finger rests on: hover it, then press.
            PointerKind::LongPress => {
                self.screen_pointer(Pointer {
                    kind: PointerKind::Move,
                    ..p
                });
                self.handle_menu(MenuEvent::Secondary);
                return true;
            }
            // A drag on a screen's menu list pans it; anywhere else it scrolls by ticks.
            PointerKind::PanStart { .. } | PointerKind::Pan { .. } | PointerKind::Fling { .. } => {
                return self.stack.last_mut().is_some_and(|s| s.pan(p));
            }
            _ => {}
        }
        if p.press() {
            if let Some((key, _)) = self.hint_rects.iter().find(|(_, r)| p.hits(*r)) {
                // Click only hints that name an action. Shoulders/Adjust name
                // a direction already under the pointer.
                let ev = match key {
                    crate::glyphs::HintKey::Confirm => Some(MenuEvent::Confirm),
                    crate::glyphs::HintKey::Back => Some(MenuEvent::Back),
                    crate::glyphs::HintKey::Secondary => Some(MenuEvent::Secondary),
                    crate::glyphs::HintKey::Tertiary => Some(MenuEvent::Tertiary),
                    _ => None,
                };
                if let Some(ev) = ev {
                    self.handle_menu(ev);
                }
                return true;
            }
        }
        if let Some(tab) = self.pill_at(p) {
            if p.press() {
                self.strip_focus = false;
                self.switch_tab(tab);
            }
            return true;
        }
        if !p.press() {
            return self.screen_pointer(p);
        }
        self.strip_focus = false;
        let depth = self.stack.len();
        let used = self.screen_pointer(p);
        // A tap dips what it landed on, unless it opened a screen over it.
        if self.stack.len() == depth && matches!(self.motion, Motion::None) {
            self.dip();
        }
        used
    }

    /// The strip pill under `p`, when the strip is up.
    fn pill_at(&self, p: Pointer) -> Option<Tab> {
        let id = self.strip.hit(p.x as f32, p.y as f32)?;
        (self.stack.len() == 1).then_some(())?;
        TABS.into_iter().find(|t| render::pill_id(*t) == id)
    }

    /// The top screen's turn at a pointer already in safe-area space.
    fn screen_pointer(&mut self, p: Pointer) -> bool {
        let mut fx = Outbox::default();
        let consumed = {
            let (mut ctx, top) = self.ctx_and_top();
            top.view_mut().pointer(p, &mut ctx, &mut fx)
        };
        self.apply(fx);
        consumed
    }

    /// Record what is producing menu events. The hint legend follows it.
    pub(crate) fn note_input_source(&mut self, source: crate::console::InputSource) {
        self.input_source = Some(source);
    }

    /// Keyboard fallback. Arrows and Enter/Esc are menu events; Space confirms and Y/X
    /// mirror Secondary/Tertiary (suppressed while editing — those keys are text).
    /// `shift` only affects Tab.
    pub(crate) fn key(&mut self, key: crate::input::Key, shift: bool, repeat: bool) -> bool {
        use crate::input::Key as S;
        self.last_input = Instant::now();
        self.input_source = Some(crate::console::InputSource::Keys);
        if self.editing() {
            let (mut ctx, top) = self.ctx_and_top();
            if top.view_mut().edit_key(key, &mut ctx) {
                return true;
            }
            // Editing consumed nothing: arrows still drive the OSK grid.
        }
        let editing = self.stack.last().is_some_and(|s| s.view().editing());
        let ev = match key {
            S::Left => MenuEvent::Move(MenuDir::Left),
            S::Right => MenuEvent::Move(MenuDir::Right),
            S::Up => MenuEvent::Move(MenuDir::Up),
            S::Down => MenuEvent::Move(MenuDir::Down),
            S::Return if !repeat => MenuEvent::Confirm,
            S::Space if !repeat && !editing => MenuEvent::Confirm,
            S::Escape | S::Backspace if !repeat => MenuEvent::Back,
            S::PageUp if !repeat => MenuEvent::JumpBack,
            S::PageDown if !repeat => MenuEvent::JumpForward,
            // Tab = JumpForward even with a pad attached (legend otherwise
            // only spells PgUp/PgDn when no pad). Shift+Tab = JumpBack.
            S::Tab if !repeat && shift => MenuEvent::JumpBack,
            S::Tab if !repeat => MenuEvent::JumpForward,
            S::Y if !repeat && !editing => MenuEvent::Secondary,
            S::X if !repeat && !editing => MenuEvent::Tertiary,
            _ => return false,
        };
        self.handle_menu(ev); // no pad to pulse
        true
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        self.last_input = Instant::now();
        if let Some(top) = self.stack.last_mut() {
            top.view_mut().text_input(text);
        }
    }

    /// Push a command with no screen: in-stream ring host actions, a re-rooted shelf's fetch.
    pub(crate) fn send_cmd(&self, cmd: ConsoleCmd) {
        self.bus.send(cmd);
    }

    /// Drop the takeover on both sides. Clearing the shared slot is what makes a late
    /// phase from the still-running burst a no-op rather than a reopened dialog.
    fn close_speed(&mut self) {
        self.console.set_speed(None);
        self.speed = None;
    }

    /// The bitrate Confirm would write, or `None` when there is nothing to write: no answer
    /// yet, or a bound preset that pins bitrate — see [`Self::speed_pinned_by`].
    fn speed_recommendation(&self) -> Option<u32> {
        let sp = self.speed.as_ref()?;
        let SpeedPhase::Done {
            recommended_kbps, ..
        } = sp.phase
        else {
            return None;
        };
        self.speed_pinned_by(&sp.key)
            .is_none()
            .then_some(recommended_kbps)
    }

    /// Name of the preset this host resolves bitrate from, when that preset PINS one.
    ///
    /// The console writes the global default and has no preset editor, so a pinned
    /// bitrate makes the measurement read-only here: applying the default would leave the
    /// tested host streaming at the preset's number and quietly retune every other host.
    /// A preset that inherits bitrate is not pinned, and the default is the right layer.
    fn speed_pinned_by(&self, key: &str) -> Option<&str> {
        self.hosts
            .iter()
            .find(|h| h.key == key)
            .and_then(|h| h.bound_preset.as_ref())
            .filter(|p| p.bitrate_kbps.is_some())
            .map(|p| p.name.as_str())
    }

    /// Write a measured bitrate to the global default. Rebase-then-save like the settings
    /// screen's typed field, and clamped to the same platform ceiling — a 2.5 Gbps LAN
    /// measures well above what a webOS TV will keep.
    fn apply_speed_bitrate(&mut self, kbps: u32) -> String {
        self.settings = self.store.load();
        let ceiling = crate::screens::settings::rows::bitrate_ceiling_kbps(self.device.platform);
        self.settings.bitrate_kbps = kbps.min(ceiling);
        self.store.save(&self.settings);
        format!(
            "{} Mb/s set as the default",
            self.settings.bitrate_kbps / 1_000
        )
    }

    fn apply(&mut self, fx: Outbox) {
        for cmd in fx.cmds {
            if let ConsoleCmd::FetchLibrary { fp_hex, .. } = &cmd {
                self.note_fetch(fp_hex);
            }
            if let ConsoleCmd::Pair { addr, port, .. } = &cmd {
                self.pairing = Some((addr.clone(), *port));
            }
            if let ConsoleCmd::FetchProfiles { fp_hex, .. } = &cmd {
                self.console.take_profiles(fp_hex);
            }
            // Gate wake in this call, like `connecting`. First WakeStatus is
            // ~100 ms–1 s away; without a placeholder the cursor keeps moving
            // and a fast wake never shows "Waking…". `sync` supersedes it.
            if let ConsoleCmd::Wake { key, then_connect } = &cmd {
                let name = self
                    .hosts
                    .iter()
                    .find(|h| &h.key == key)
                    .map(|h| h.name.clone())
                    .unwrap_or_default();
                self.wake = Some(WakeStatus {
                    key: key.clone(),
                    name,
                    seconds: 0,
                    timed_out: false,
                    online: false,
                    then_connect: *then_connect,
                });
                self.wake_optimistic = true;
            }
            // The shell seeds the slot, not the service thread: the takeover must be up
            // before the connect blocks, and seeding it on the side that clears it is what
            // makes a dismissed test's late report a no-op (`ConsoleShared::advance_speed`).
            if let ConsoleCmd::SpeedTest { key, host_name, .. } = &cmd {
                let status = SpeedStatus::new(key.clone(), host_name.clone());
                self.console.set_speed(Some(status));
                self.speed = self.console.speed();
            }
            self.bus.send(cmd);
        }
        if let Some(text) = fx.toast {
            self.show_toast(text);
        }
        if let Some(text) = fx.copy {
            self.actions.push_back(OverlayAction::CopyText(text));
        }
        if let Some(intent) = fx.connect {
            self.start_connect(intent);
        }
        if let Some(tab) = fx.tab {
            self.switch_tab(tab);
        }
        if let Some(nav) = fx.nav {
            self.apply_nav(nav);
        }
        if fx.quit {
            self.actions.push_back(OverlayAction::Quit);
        }
        if fx.browse {
            let hosts = &self.hosts;
            let below = match self.stack.first_mut() {
                Some(Screen::Home(home)) => home.browse(hosts),
                _ => false,
            };
            if !below {
                self.switch_tab(Tab::Games);
            }
        }
    }

    /// Spring for the next push/pop. Sampled at nav time so a mid-flight
    /// settings change cannot retune the in-progress transition.
    /// Reduced motion stays a spring (`REDUCED_NAV`); `render.rs` flattens
    /// geometry into the crossfade the setting promises.
    fn nav_spec(&self) -> crate::anim::SpringSpec {
        if self.reduce_motion() {
            REDUCED_NAV
        } else {
            springs::NAV
        }
    }

    fn begin_nav(&mut self, kind: NavKind, leaving: Option<Box<Screen>>) {
        self.motion = Motion::Nav {
            spring: Spring::rest(0.0),
            target: 1.0,
            kind,
            leaving,
        };
    }

    fn apply_nav(&mut self, nav: Nav) {
        match nav {
            Nav::Push(screen) => {
                self.stack.push(*screen);
                self.begin_nav(NavKind::Push, None);
            }
            Nav::Replace(screen) => {
                // Same push recipe, but the outgoing screen is dropped from
                // the stack. Carry it as `leaving`: a push paints `n - 2` as
                // the receding layer, and after pop that would be the parent
                // (host list flashing under an Edit push).
                let leaving = self.stack.pop().map(Box::new);
                self.stack.push(*screen);
                self.begin_nav(NavKind::Push, leaving);
            }
            Nav::Pop => {
                if self.stack.len() > 1 {
                    let leaving = self.stack.pop().expect("len > 1");
                    self.begin_nav(NavKind::Pop, Some(Box::new(leaving)));
                } else if !self.strip_focus {
                    // A root screen's Back lifts focus to its tab; the tab's Back asks.
                    self.strip_focus = true;
                } else {
                    self.ask_exit();
                }
            }
        }
    }

    /// Back on the tab strip. The same button backs out of every screen, so exit is a
    /// question, not a press. Where the app cannot quit the press does nothing, and a TV
    /// remote's Menu is the system's (`at_root`).
    fn ask_exit(&mut self) -> Option<MenuPulse> {
        if !self.device.platform.can_quit() {
            return Some(MenuPulse::Boundary);
        }
        let exit = crate::screens::prompt::PromptScreen::exit();
        self.apply_nav(Nav::Push(Box::new(Screen::Prompt(exit))));
        Some(MenuPulse::Confirm)
    }

    /// In-flight spring position. `1.0` when there is no transition, so
    /// callers that compare against `NAV_INPUT_OPENS` treat idle as seated.
    fn nav_pos(&self) -> f64 {
        match &self.motion {
            Motion::None => 1.0,
            Motion::Nav { spring, .. } | Motion::Tab { spring, .. } => spring.pos,
        }
    }

    /// Back during a transition. `true` = the transition consumed it;
    /// the event must not reach the screens.
    fn nav_back(&mut self) -> bool {
        let Motion::Nav {
            spring,
            target,
            kind,
            ..
        } = &mut self.motion
        else {
            return false;
        };
        match *kind {
            // Retarget the same spring. Velocity carries; `finish_nav` pops
            // the entering screen at 0. Refused at the root: B there goes to the tab.
            NavKind::Push if *target == 1.0 && self.stack.len() > 1 => {
                *target = 0.0;
                true
            }
            // Already reversing. Letting this through would pop the parent
            // from under a screen still leaving.
            NavKind::Push if *target == 0.0 => true,
            NavKind::Push => false,
            // Held B: finish this pop's bookkeeping (stack is already
            // correct) and start the next one.
            NavKind::Pop => {
                let _ = spring;
                self.motion = Motion::None;
                self.apply_nav(Nav::Pop);
                true
            }
        }
    }

    /// Step the in-flight transition. `None` once settled; [`Self::finish_nav`]
    /// has then already run. Separate from `render` so tests can pass a `dt`
    /// wall-clock render would measure in microseconds.
    fn advance_nav(&mut self, dt: f64) -> Option<f64> {
        let spec = self.nav_spec();
        let p = match &mut self.motion {
            Motion::None => None,
            Motion::Nav { spring, target, .. } => {
                spring.step_spec(*target, spec, dt);
                spring.settle(*target, 0.001, 0.01);
                // `settle` snaps to rest, so inequality is exact — and the
                // only way out of Nav.
                (spring.pos != *target || spring.vel != 0.0).then_some(spring.pos)
            }
            Motion::Tab { spring, .. } => {
                spring.step_spec(1.0, spec, dt);
                spring.settle(1.0, 0.001, 0.01);
                (spring.pos != 1.0 || spring.vel != 0.0).then_some(spring.pos)
            }
        };
        if p.is_none() {
            self.finish_nav();
        }
        p
    }

    fn finish_nav(&mut self) {
        if let Motion::Nav {
            kind,
            target,
            leaving,
            ..
        } = &mut self.motion
        {
            // Reversed push never happened: pop its screen. Length-guarded
            // because the root must stay; `nav_back` already refuses there.
            if *kind == NavKind::Push && *target == 0.0 && self.stack.len() > 1 {
                self.stack.pop();
                // Reversed REPLACE restores what it swapped out. Undo means
                // the menu that was on glass, not the parent one level out.
                if let Some(back) = leaving.take() {
                    self.stack.push(*back);
                }
            }
        }
        // Dropping Nav drops a completed pop's `leaving` screen.
        self.motion = Motion::None;
    }

    /// Backdrop shader clock. Reduced motion freezes the phase; the colour
    /// is the picked palette and a still gradient is what an OLED can hold.
    /// Calm mix is not frozen — that tracks which screen is up.
    fn field_clock(&self, t: f64) -> f64 {
        if self.reduce_motion() {
            0.0
        } else {
            t
        }
    }

    fn draw_aurora(&self, canvas: &Canvas, w: f64, h: f64, t: f64, calm: f64) {
        // One clock read: the takeover's `draw_aurora` inherits it.
        let t = self.field_clock(t);
        let reduced = crate::screens::settings::rows::reduce_ui_res(
            &self.settings,
            self.device.platform,
            self.device.fallback_ui,
        );
        self.backdrop.draw(canvas, w, h, t, calm, reduced);
    }
}

/// Every glyph a title commonly has, split across the stand-in titles: the warm-up puts each
/// into the glyph atlas at the sizes and weights a shelf draws it, not on the first real visit.
const STAND_IN_GLYPHS: &str = "ABCDEFGHIJKLM NOPQRSTUVWXYZ abcdefghijklm nopqrstuvwxyz \
     0123456789 :'-.,!?&()/+ ®™©éèêëáàâäóòôöúùûüíìîïçñß ÉÀÖÜ–—’“”…";

/// The warm-up's library: a title of each shape a shelf draws, store badges, a running one and
/// launchers, so the programs and glyphs a loaded shelf needs are all asked for.
fn stand_in_games() -> Vec<crate::library::LibraryGame> {
    let glyphs: Vec<char> = STAND_IN_GLYPHS.chars().collect();
    let chunk = glyphs.len().div_ceil(12);
    let title = |i: usize| -> String {
        glyphs
            .chunks(chunk)
            .nth(i)
            .map_or_else(|| format!("Title {i}"), |c| c.iter().collect())
    };
    let game = |i: usize, store: &str, launcher: bool, icon: &str| crate::library::LibraryGame {
        id: format!("warm:{i}"),
        title: title(i),
        store: store.into(),
        launcher,
        icon: icon.into(),
        platform: Some("PC".into()),
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: i == 1,
        endable: false,
        install: None,
    };
    let stores = ["steam", "lutris", "gog", "epic", "custom", "heroic"];
    let mut out: Vec<_> = (0..12)
        .map(|i| game(i, stores[i % stores.len()], false, ""))
        .collect();
    out.push(game(12, "steam", true, "steam"));
    out.push(game(13, "desktop", true, "desktop"));
    out
}

/// The paired, online host the warm-up tours with.
fn stand_in_host() -> HostRow {
    HostRow {
        key: "warm".into(),
        name: "Stand-in".into(),
        addr: "127.0.0.1".into(),
        port: 9777,
        fp_hex: "00".into(),
        paired: true,
        saved: true,
        online: true,
        mgmt_port: 9778,
        os: "linux".into(),
        ..Default::default()
    }
}

/// A cover the warm-up draws: a raster with mips, as a decoded poster is.
fn stand_in_poster() -> Option<Image> {
    let mut surface = skia_safe::surfaces::raster_n32_premul((300, 450))?;
    surface.canvas().clear(Color4f::new(0.4, 0.3, 0.6, 1.0));
    let image = surface.image_snapshot();
    image.with_default_mipmaps().or(Some(image))
}

#[cfg(test)]
mod tests;
