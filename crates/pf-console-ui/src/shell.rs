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
use crate::library::{mesh_sksl, palette, LibraryShared};
use crate::model::{
    ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, SpeedPhase, SpeedStatus, WakeStatus,
};
use crate::platform::Platform;
#[cfg(test)]
use crate::pointer::DRAG_TICK_DP;
use crate::pointer::{Pointer, PointerKind, Touch};
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use crate::store::SettingsStore;
use anyhow::{anyhow, Result};
use pf_client_core::console::OverlayAction;
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::start;
use pf_client_core::trust;
use skia_safe::{Canvas, Color4f, Data, Paint, Rect, RuntimeEffect, Surface};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

mod overlays;
mod render;

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
const TOP_BAND: f64 = 64.0;
const BOTTOM_BAND: f64 = 86.0;

/// Long edge of the reduced backdrop's offscreen, px. The field is a pure function of
/// `xy/u_res`, so a small buffer holds the same picture — the per-pixel exp/sin/Bézier
/// work is exactly what a TV GPU cannot afford.
const FIELD_EDGE: f64 = 512.0;
/// Seconds between reduced-backdrop re-renders (~25 Hz). The field drifts on ~90–130 s
/// periods, so the step is invisible; a frozen (reduce-motion) field renders once.
const FIELD_STEP: f64 = 0.04;

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

/// How long an armed exit stays armed. Long enough to be a deliberate second press, short
/// enough that a Back pressed minutes later is a fresh accident rather than a confirmation.
const EXIT_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(4);

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

struct Connecting {
    title: String,
    appear: f64,
    /// Host is parked pending operator approval. Takeover title is
    /// "Waiting for approval", not "Connecting".
    request_access: bool,
}

/// Where the launch hold asks after its title: the shelf's host, on the
/// management lane the shelf already reads `/status` from.
struct LaunchHost {
    id: String,
    addr: String,
    mgmt: u16,
    fp_hex: String,
}

/// One screen from the press to the game: the cover leaves its shelf tile, and
/// holds — through the dial, then over the stream — until the game is up.
///
/// Every launch begins with the launcher's own window (Steam booting, a
/// desktop) — the first thing a player used to see of a game. The host tells
/// `launching` from `running` per lease (`punktfunk-host::gamelease`), and a
/// paired client may read it, so the hold polls that until it changes.
///
/// Raised at the press rather than at the first frame, because the shelf is
/// only on screen then — it is the one moment the cover has somewhere to fly
/// FROM — and holding from there means the launcher is never seen at all.
/// Replaces the [`Connecting`] card for a game launch; the two would otherwise
/// be two takeovers for one act.
struct Launching {
    host: LaunchHost,
    title: String,
    /// `PC · 2024 · Steam` — where the host filed it, in the order a player scans it.
    facts: String,
    /// Studio, and the genres joined; either may be empty, and an older host sends neither.
    developer: String,
    genres: String,
    /// Backdrop and copy fade, 0 → 1.
    appear: f64,
    /// The cover's flight out of its tile, 0 (tile) → 1 (settled).
    flight: Spring,
    /// The tile it leaves, in the shell's layout space. Empty = no tile to
    /// leave (a keyboard launch off a culled row), so it arrives in place.
    from: Rect,
    /// The dial landed. Before it, a press cancels the connect and there is no
    /// session to ask the host about; after it, a press shows the stream.
    connected: bool,
    since: f64,
    last_poll: f64,
    /// `status_gen` when the hold began; a state read before that describes
    /// an earlier launch of the same title and must not end this one.
    base_gen: u64,
    /// `status_gen` when the last poll went out — the next waits for it to move.
    poll_gen: u64,
    /// The game is up and the host is waiting for its window.
    window_wait: bool,
    /// Why the hold gave up, once it has. Latched: the hold holds the screen and says this
    /// instead of sliding away onto a desktop nobody asked for.
    failed: Option<String>,
}

/// Why the hold is giving up, in one sentence, or `None` while it should keep waiting.
///
/// `state` is the host's own `games[]` word for this title, `None` when the host lists nothing
/// for it at all — which is what a refused launch looks like from here. The touch shell's
/// `launchGaveUp` says the same three sentences, so a report quotes one line whichever shell
/// it came from. `running`, `untracked` and `grace` keep waiting or reveal: those launches worked.
fn launch_gave_up(title: &str, state: Option<&str>, elapsed: f64) -> Option<String> {
    match state {
        None if elapsed >= LAUNCH_NO_LEASE => Some(format!(
            "The host didn't start {title} — nothing is running for it."
        )),
        Some("launching") if elapsed >= LAUNCH_HOLD_MAX => {
            Some(format!("{title} is still starting after 2 minutes."))
        }
        Some("exited") => Some(format!("{title} closed right after starting.")),
        _ => None,
    }
}

/// Poll interval for the launch hold, and the retry when an answer never lands.
const LAUNCH_POLL: f64 = 1.0;
const LAUNCH_POLL_STALL: f64 = 5.0;
/// The host lists nothing for the title: the launch did not resolve
/// (no recipe, launcher missing). The host logs it and streams on; so do we.
const LAUNCH_NO_LEASE: f64 = 15.0;
/// A game still `launching`, or `running` without its window, this long is one
/// the player wants to see for themselves — a cold Steam boot with shader work
/// runs to minutes, and the host waits five for it.
const LAUNCH_HOLD_MAX: f64 = 120.0;

/// Host-supplied construction options.
pub struct ConsoleOptions {
    /// Hostname registered as the default pairing device name.
    pub device_name: String,
    /// Steam Deck: Steam's keyboard types; this shell never draws one.
    pub deck: bool,
    /// Host has another UI when the console is off (phone/tablet touch shell).
    /// False on desktop and Android TV — offering "off" would strand the user.
    pub fallback_ui: bool,
    /// This device's GPU decodes PyroWave. Answer from the same probe that gates the
    /// client's `CODEC_PYROWAVE` advertisement: a row that offers what the Hello never
    /// asks for is a setting that silently does nothing.
    pub pyrowave_ok: bool,
    /// This device decodes AV1 in hardware — the same answer that gates the client's
    /// `CODEC_AV1` advertisement (`pf_client_core::video::av1_hardware_decodable`). A host
    /// that learns it only once its GPU exists starts `true` and corrects it.
    pub av1_ok: bool,
    /// Settings and preset catalog. `None` uses the desktop file store
    /// (`pf_client_core::trust`); every other host must supply one.
    pub store: Option<Arc<dyn SettingsStore>>,
    /// Which settings rows exist and which platform-native screens may open.
    pub platform: Platform,
    /// Skia GPU resource-cache budget, bytes. Desktop default is
    /// [`DEFAULT_GPU_CACHE_BYTES`]; a memory-tight box may go down to
    /// [`MIN_GPU_CACHE_BYTES`] but never below it.
    pub gpu_cache_bytes: usize,
    /// This device's own screen, for the Aspect row. `None` where streams go to a window or a
    /// TV: only a panel of an unusual shape (a phone) changes what the row offers.
    pub screen: Option<DeviceScreen>,
}

/// A built-in screen in landscape pixels, whole and clear of its cutout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceScreen {
    pub full: (u32, u32),
    pub safe: (u32, u32),
}

impl ConsoleOptions {
    pub fn desktop(device_name: String, deck: bool) -> ConsoleOptions {
        ConsoleOptions {
            device_name,
            deck,
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

pub(crate) struct Shell {
    stack: Vec<Screen>,
    motion: Motion,
    console: ConsoleShared,
    library: LibraryShared,
    bus: ConsoleBus,
    actions: VecDeque<OverlayAction>,
    settings: trust::Settings,
    store: Arc<dyn SettingsStore>,
    pub(crate) platform: Platform,
    screen: Option<DeviceScreen>,
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    device_name: String,
    deck: bool,
    fallback_ui: bool,
    pyrowave_ok: bool,
    pub(crate) av1_ok: bool,
    pub(crate) in_stream: bool,
    connecting: Option<Connecting>,
    launching: Option<Launching>,
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
    toast: Option<Toast>,
    /// Fingerprint of a first pairing whose shelf has not opened yet. See
    /// [`Self::open_first_paired_library`].
    first_pair: Option<String>,
    /// When Back at the root was last pressed, where that press has to be repeated to exit.
    /// See [`EXIT_CONFIRM_WINDOW`].
    exit_armed: Option<std::time::Instant>,
    mesh: RuntimeEffect,
    /// Palette id baked into `mesh`. [`Self::sync`] recompiles when
    /// `settings.ui_palette` moves.
    mesh_palette: String,
    /// OS-theme revision baked into `mesh` while follow-system is on.
    /// `None` for a curated palette. Pair with `mesh_palette` so `sync`
    /// rebuilds only on the row step or a real theme change.
    mesh_os: Option<u64>,
    /// Palette ground × 0.4. `col*0.6 + lift` leaves the ground unchanged
    /// and pulls the bright pools down: form screens lose contrast, not colour.
    mesh_lift: [f32; 3],
    /// Backdrop scrim: rgb = vignette target (black on dark, white on pale),
    /// a = strength. Kept with the ink.
    mesh_scrim: [f32; 4],
    /// Text/accent/glass for this palette, published once per frame
    /// (see [`crate::theme::set_ink`]).
    ink: crate::theme::Ink,
    /// 0 = launcher aurora, 1 = form field. Chased so the backdrop settles
    /// with the screen transition.
    bg_mix: f64,
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
    /// The reduced backdrop's retained pass — [`Shell::draw_field_reduced`]. A cell
    /// because the takeover chain borrows overlay state while it draws, so `&mut self`
    /// never reaches here.
    field: RefCell<Option<FieldCache>>,
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
        let (mesh, mesh_lift, mesh_scrim, ink) = build_mesh(&settings.ui_palette)?;
        let bg_mix = match stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
        Ok(Shell {
            stack,
            motion: Motion::None,
            console,
            library,
            bus,
            actions: VecDeque::new(),
            mesh_palette: settings.ui_palette.clone(),
            mesh_os: None,
            settings,
            store,
            platform: opts.platform,
            screen: opts.screen,
            hosts: Vec::new(),
            hosts_gen: u64::MAX,
            device_name: opts.device_name,
            deck: opts.deck,
            fallback_ui: opts.fallback_ui,
            pyrowave_ok: opts.pyrowave_ok,
            av1_ok: opts.av1_ok,
            in_stream: false,
            connecting: None,
            launching: None,
            last_connect_title: None,
            wake: None,
            wake_optimistic: false,
            speed: None,
            toast: None,
            first_pair: None,
            exit_armed: None,
            mesh,
            mesh_lift,
            mesh_scrim,
            ink,
            bg_mix,
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
            field: RefCell::new(None),
        })
    }

    /// Live library model, for a host re-rooting via [`Self::replace_stack`].
    pub(crate) fn library(&self) -> &LibraryShared {
        &self.library
    }

    /// The screen on top of the stack.
    pub(crate) fn top(&self) -> Option<&Screen> {
        self.stack.last()
    }

    /// Replace the stack (deep link, return-to-shelf). Cut, no transition:
    /// this is re-entry, not navigation the user watched.
    pub(crate) fn replace_stack(&mut self, stack: Vec<Screen>) {
        if stack.is_empty() {
            return;
        }
        self.stack = stack;
        self.motion = Motion::None;
        self.bg_mix = match self.stack.last().expect("non-empty").background() {
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

    pub(crate) fn editing(&self) -> bool {
        !self.in_stream
            && self.connecting.is_none()
            && !self.holds_stream()
            && self.stack.last().is_some_and(Screen::editing)
    }

    /// What a screen reader should speak for the focused row. `None` while a takeover owns
    /// the input, or on a screen that names no focus.
    /// `&mut` only to hand `Ctx` the settings it wants by `&mut`; nothing on this
    /// path writes them. A host polls this once per frame, so cloning the settings
    /// to get an `&self` here would be ~9 string allocations per frame on a still
    /// screen — the per-frame-work-that-changes-nothing shape this shell has
    /// already paid to remove once.
    pub(crate) fn focus_announcement(&mut self) -> Option<String> {
        if self.in_stream
            || self.holds_stream()
            || self.connecting.is_some()
            || self.wake.is_some()
            || self.speed.is_some()
        {
            return None;
        }
        let t = self.t();
        let screen = self.stack.last()?;
        let ctx = Ctx {
            hosts: &self.hosts,
            library: &self.library,
            settings: &mut self.settings,
            store: &*self.store,
            platform: self.platform,
            screen: self.screen,
            pads: &self.pads,
            deck: self.deck,
            fallback_ui: self.fallback_ui,
            pyrowave_ok: self.pyrowave_ok,
            av1_ok: self.av1_ok,
            device_name: &self.device_name,
            t,
        };
        screen.announcement(&ctx)
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

    pub(crate) fn set_connecting(&mut self, title: Option<String>) {
        match title {
            Some(title) => {
                self.last_connect_title = Some(title.clone());
                self.connecting = Some(Connecting {
                    title,
                    appear: 0.0,
                    request_access: false,
                })
            }
            None => self.connecting = None,
        }
    }

    pub(crate) fn session_failed(&mut self, msg: &str) {
        self.connecting = None;
        self.launching = None;
        self.in_stream = false;
        self.show_toast_kind(format!("Couldn't connect — {msg}"), ToastKind::Error);
    }

    pub(crate) fn session_streaming(&mut self) {
        self.connecting = None;
        let t = self.t();
        let Some(l) = &mut self.launching else {
            self.in_stream = true;
            return;
        };
        l.connected = true;
        // The host has no lease to report until the session that launched the title
        // exists, so the "never listed it" clock only starts making sense here.
        l.since = t;
        self.in_stream = false;
    }

    /// The hold for a launched title, or `None` when there is nothing to wait for: a launcher
    /// tile (the host never tracks those), or a title the shelf no longer lists.
    ///
    /// Every platform, not just the desktop. The console is the launch screen wherever it is
    /// the launcher — a host that has a stream view of its own waits for
    /// [`OverlayAction::ShowStream`] before switching to it, rather than drawing a second
    /// launch screen of its own on top of this one.
    fn launch_hold(&self, host: LaunchHost, from: Rect) -> Option<Launching> {
        let snap = self.library.snapshot();
        let g = snap.games.iter().find(|g| g.id == host.id)?;
        if g.launcher {
            return None;
        }
        let year = g.year.map(|y| y.to_string());
        let facts = [
            g.platform.as_deref(),
            year.as_deref(),
            Some(crate::library::store_label(&g.store)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
        let t = self.t();
        let reads = self.library.status_gen();
        Some(Launching {
            host,
            title: g.title.clone(),
            facts,
            developer: g.developer.clone().unwrap_or_default(),
            genres: g.genres.join(" \u{b7} "),
            appear: 0.0,
            flight: Spring::rest(0.0),
            from,
            connected: false,
            since: t,
            // The first poll goes out on the next frame.
            last_poll: t - LAUNCH_POLL_STALL,
            base_gen: reads,
            poll_gen: reads,
            window_wait: false,
            failed: None,
        })
    }

    /// Drop the hold and let the stream through.
    fn reveal_stream(&mut self) {
        self.launching = None;
        self.in_stream = true;
        // Hosts that swap to a stream view of their own have been holding the session since
        // the dial landed; this is what releases it. A host that composites this console over
        // its stream ignores it.
        self.actions.push_back(OverlayAction::ShowStream);
    }

    /// One frame of the launch hold: reveal when the host has answered, or
    /// when it never will; else keep the poll going.
    fn tick_launch(&mut self) {
        let t = self.t();
        let reads = self.library.status_gen();
        let Some(l) = &self.launching else { return };
        // Nothing to ask about yet: the lease is the SESSION's, and a title that was
        // already up would otherwise read as "running" and reveal a stream that does
        // not exist.
        if !l.connected || l.failed.is_some() {
            return;
        }
        let state = (reads > l.base_gen)
            .then(|| self.library.launch_state(&l.host.id))
            .flatten();
        let elapsed = t - l.since;
        let window_wait = matches!(&state, Some((s, true)) if s == "running");
        let word = state.as_ref().map(|(s, _)| s.as_str());
        // A launch that produced no game ends with a sentence, not by sliding away: a bare
        // desktop reads the same whether the host refused it or the game is merely slow.
        if let Some(why) = launch_gave_up(&l.title, word, elapsed) {
            if let Some(l) = &mut self.launching {
                l.failed = Some(why);
            }
            return;
        }
        let done = match word {
            // Both handled above, once they run out of patience.
            Some("launching") => false,
            // A Proton prefix or a splash can sit behind a running process for a minute.
            Some("running") if window_wait => elapsed >= LAUNCH_HOLD_MAX,
            // window, running, untracked, grace: the host has said all it will.
            Some(_) => true,
            None => false,
        };
        if done {
            self.reveal_stream();
            return;
        }
        let waited = t - l.last_poll;
        if (reads != l.poll_gen && waited >= LAUNCH_POLL) || waited >= LAUNCH_POLL_STALL {
            let poll = ConsoleCmd::RefreshRunning {
                addr: l.host.addr.clone(),
                mgmt: l.host.mgmt,
                fp_hex: l.host.fp_hex.clone(),
            };
            if let Some(l) = &mut self.launching {
                l.last_poll = t;
                l.poll_gen = reads;
            }
            self.bus.send(poll);
        }
        if let Some(l) = &mut self.launching {
            l.window_wait = window_wait;
        }
    }

    pub(crate) fn session_ended(&mut self, reason: Option<&str>) {
        self.connecting = None;
        self.launching = None;
        self.in_stream = false;
        // Stack survives a stream, so nothing else refreshes the running set:
        // without this the Resume badge still names the title they just quit.
        // Catalog is left alone — a re-fetch would swap the shelf for a spinner.
        if let Some(Screen::Library(lib)) = self.stack.last() {
            self.bus.send(ConsoleCmd::RefreshRunning {
                addr: lib.host_addr().to_string(),
                mgmt: lib.host_mgmt_port(),
                fp_hex: lib.host_fp_hex().to_string(),
            });
        }
        if let Some(reason) = reason {
            self.show_toast(format!("Session ended — {reason}"));
        }
    }

    /// Client is redialing on its own (codec fallback). Raise the connecting
    /// modal: nothing sends `Launch` for this retry, so without it the shell
    /// is not streaming, not connecting, and a live pump is behind the
    /// console — A would launch a second session. Back → `CancelConnect`.
    ///
    /// `appear = 1.0`: the retry follows a live stream; fading in is a flash.
    pub(crate) fn session_reconnecting(&mut self, msg: &str) {
        self.in_stream = false;
        self.launching = None;
        self.connecting = Some(Connecting {
            // `None` only if the shell never raised the connect (`--connect`
            // has no console). Prefer a codec-change name over empty string.
            title: self
                .last_connect_title
                .clone()
                .unwrap_or_else(|| "the host".to_string()),
            appear: 1.0,
            request_access: false,
        });
        self.show_toast(msg.to_string());
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
        // Settings writes palette/follow-OS into `self.settings`; recompile
        // here so the backdrop re-colours live. A rejected compile keeps the
        // field that is drawing (never black) and still advances bookkeeping
        // so a broken build warns once, not once per frame.
        let (os_rev, os) = crate::os_theme::os_theme();
        let want_os = if self.settings.follow_os_theme {
            os
        } else {
            None
        };
        if let Some(t) = want_os {
            if self.mesh_os != Some(os_rev) {
                match build_mesh_os(&t) {
                    Ok((mesh, lift, scrim, ink)) => {
                        self.mesh = mesh;
                        self.mesh_lift = lift;
                        self.mesh_scrim = scrim;
                        self.ink = ink;
                    }
                    Err(e) => tracing::warn!("console: OS theme rejected: {e}"),
                }
                self.mesh_os = Some(os_rev);
            }
        } else if self.mesh_os.is_some() || self.settings.ui_palette != self.mesh_palette {
            match build_mesh(&self.settings.ui_palette) {
                Ok((mesh, lift, scrim, ink)) => {
                    self.mesh = mesh;
                    self.mesh_lift = lift;
                    self.mesh_scrim = scrim;
                    self.ink = ink;
                }
                Err(e) => {
                    tracing::warn!(
                        "console: {} palette rejected: {e}",
                        self.settings.ui_palette
                    );
                }
            }
            self.mesh_os = None;
            self.mesh_palette = self.settings.ui_palette.clone();
        }
        if self.console.hosts_gen() != self.hosts_gen {
            (self.hosts, self.hosts_gen) = self.console.hosts_snapshot();
        }

        if let Some(text) = self.console.take_notice() {
            self.show_toast(text);
        }

        let pair = self.console.pair();
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
                if matches!(self.stack.last(), Some(Screen::Pair(_))) {
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
                if let Some(Screen::Pair(p)) = self.stack.last_mut() {
                    p.apply_phase(phase);
                }
                if matches!(phase, PairPhase::Failed(_)) {
                    self.console.set_pair(PairPhase::Idle);
                }
            }
        }
        self.open_first_paired_library();

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
                        .map(|h| ConnectIntent {
                            addr: h.addr.clone(),
                            port: h.port,
                            fp_hex: h.fp_hex.clone(),
                            launch: None,
                            // Pinned-card wake carries the pin's preset.
                            title: match &h.pin {
                                Some(p) => format!("{} · {}", h.name, p.name),
                                None => h.name.clone(),
                            },
                            request_access: false,
                            preset: h.pin.as_ref().map(|p| p.id.clone()),
                        })
                });
                self.bus.send(ConsoleCmd::CancelWake);
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

        self.collections_handover();
        self.tick_launch();
    }

    /// Swap a library shelf for the collections screen once it holds more
    /// than one. Lives here: a screen cannot replace itself.
    ///
    /// Settled transitions only. Mid-flight the stack top is not what is
    /// on glass; swapping under a reversed push would land on a host the
    /// user already backed out of.
    fn collections_handover(&mut self) {
        if !matches!(self.motion, Motion::None) {
            return;
        }
        // Borrow, don't clone: this is every frame of the shelf's life and
        // `Settings` owns Strings. `stack` mut vs `library`/`settings` shared
        // are disjoint, so the shelf can read both while being held.
        let upgraded = match self.stack.last_mut() {
            Some(Screen::Library(shelf)) => {
                shelf.collections_upgrade(&self.library, &self.settings)
            }
            _ => None,
        };
        if let Some(screen) = upgraded {
            let n = self.stack.len();
            self.stack[n - 1] = Screen::Collections(screen);
        }
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
        self.bus.send(ConsoleCmd::FetchLibrary {
            addr: row.addr.clone(),
            mgmt: row.mgmt_port,
            fp_hex: row.fp_hex.clone(),
        });
        let epoch = self.library.fetch_epoch();
        self.apply_nav(Nav::Push(Box::new(Screen::Library(
            crate::screens::library::LibraryScreen::new(&row, epoch),
        ))));
    }

    pub(crate) fn start_connect(&mut self, intent: ConnectIntent) {
        // A game launch comes off a shelf, which knows both the host's management
        // port and where it just drew the tile.
        let launch = match (&intent.launch, self.stack.last()) {
            (Some(id), Some(Screen::Library(lib))) => Some((
                LaunchHost {
                    id: id.clone(),
                    addr: intent.addr.clone(),
                    mgmt: lib.host_mgmt_port(),
                    fp_hex: intent.fp_hex.clone(),
                },
                lib.tile_rect(id),
            )),
            _ => None,
        };
        self.launching = launch.and_then(|(host, from)| self.launch_hold(host, from));
        if self.launching.is_none() {
            self.set_connecting(Some(intent.title.clone()));
            if let Some(c) = &mut self.connecting {
                c.request_access = intent.request_access;
            }
        }
        self.actions.push_back(OverlayAction::Launch {
            addr: intent.addr,
            port: intent.port,
            fp_hex: intent.fp_hex,
            launch: intent.launch,
            title: intent.title,
            request_access: intent.request_access,
            preset: intent.preset,
        });
    }

    pub(crate) fn handle_menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        self.last_input = Instant::now();
        self.sync();
        // The launch hold owns the buttons while it is up: before the dial lands B
        // cancels it, as the connect card's B does; after, any press shows the stream.
        if let Some(l) = &self.launching {
            if l.connected {
                if matches!(ev, MenuEvent::Confirm | MenuEvent::Back) {
                    self.reveal_stream();
                    return Some(MenuPulse::Confirm);
                }
            } else if ev == MenuEvent::Back {
                self.launching = None;
                self.actions.push_back(OverlayAction::CancelConnect);
                return Some(MenuPulse::Confirm);
            }
            return None;
        }
        if self.connecting.is_some() {
            if ev == MenuEvent::Back {
                // Drop the takeover here, not on the next `session_phase`.
                // The dial is blocking on the host; a dropped cancel never
                // sends a phase. Cancel is local; `CancelConnect` still goes
                // out and hosts already handle a dial that lands after it.
                self.connecting = None;
                self.actions.push_back(OverlayAction::CancelConnect);
                return Some(MenuPulse::Confirm);
            }
            return None;
        }
        if let Some(w) = &self.wake {
            match ev {
                MenuEvent::Back => {
                    self.bus.send(ConsoleCmd::CancelWake);
                    self.wake = None;
                    self.wake_optimistic = false;
                    return Some(MenuPulse::Confirm);
                }
                MenuEvent::Confirm if w.timed_out => {
                    self.bus.send(ConsoleCmd::Wake {
                        key: w.key.clone(),
                        then_connect: w.then_connect,
                    });
                    return Some(MenuPulse::Confirm);
                }
                _ => return None,
            }
        }
        if self.speed.is_some() {
            match ev {
                // Dismissing mid-burst abandons the measurement, not the burst: the host
                // finishes it either way, and `advance_speed` drops the late report.
                MenuEvent::Back => {
                    self.close_speed();
                    return Some(MenuPulse::Confirm);
                }
                MenuEvent::Confirm => {
                    let kbps = self.speed_recommendation()?;
                    let text = self.apply_speed_bitrate(kbps);
                    self.close_speed();
                    self.show_toast(text);
                    return Some(MenuPulse::Confirm);
                }
                _ => return None,
            }
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

        let mut fx = Outbox::default();
        let pulse = {
            let mut ctx = Ctx {
                hosts: &self.hosts,
                library: &self.library,
                settings: &mut self.settings,
                store: &*self.store,
                platform: self.platform,
                screen: self.screen,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
                pyrowave_ok: self.pyrowave_ok,
                av1_ok: self.av1_ok,
                device_name: &self.device_name,
                t: self.t0.elapsed().as_secs_f64(),
            };
            self.stack
                .last_mut()
                .expect("non-empty stack")
                .menu(ev, &mut ctx, &mut fx)
        };
        self.apply(fx);
        pulse
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
        // Right button is B, including on modal cards. Exception: B at the
        // root quits, and a right-click is too easy to fire by accident —
        // quit stays the legend's clickable "Quit".
        if let Some(l) = &self.launching {
            let connected = l.connected;
            if connected && (p.press() || p.kind == PointerKind::Back) {
                self.reveal_stream();
            } else if !connected && p.kind == PointerKind::Back {
                self.launching = None;
                self.actions.push_back(OverlayAction::CancelConnect);
            }
            return true;
        }
        if p.kind == PointerKind::Back {
            if self.stack.len() > 1
                || self.connecting.is_some()
                || self.wake.is_some()
                || self.speed.is_some()
            {
                self.handle_menu(MenuEvent::Back);
            }
            return true;
        }
        // Clicking through a connect takeover onto the library would start
        // a second session. Same early return as the menu path.
        if self.connecting.is_some() || self.wake.is_some() || self.speed.is_some() {
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
            // A screen that does not pan scrolls a drag by ticks.
            PointerKind::PanStart { .. } | PointerKind::Pan { .. } | PointerKind::Fling { .. }
                if !self.stack.last().is_some_and(Screen::pans) =>
            {
                return false;
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
                    // Home carousel: Up is "open this tile's menu", not nav.
                    // Without this the host-link copy path is pad-only.
                    crate::glyphs::HintKey::Up => Some(MenuEvent::Move(MenuDir::Up)),
                    // Home carousel: Down is "open Settings", not nav.
                    crate::glyphs::HintKey::Down => Some(MenuEvent::Move(MenuDir::Down)),
                    _ => None,
                };
                if let Some(ev) = ev {
                    self.handle_menu(ev);
                }
                return true;
            }
        }
        self.screen_pointer(p)
    }

    /// The top screen's turn at a pointer already in safe-area space.
    fn screen_pointer(&mut self, p: Pointer) -> bool {
        let mut fx = Outbox::default();
        let consumed = {
            let mut ctx = Ctx {
                hosts: &self.hosts,
                library: &self.library,
                settings: &mut self.settings,
                store: &*self.store,
                platform: self.platform,
                screen: self.screen,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
                pyrowave_ok: self.pyrowave_ok,
                av1_ok: self.av1_ok,
                device_name: &self.device_name,
                t: self.t0.elapsed().as_secs_f64(),
            };
            self.stack
                .last_mut()
                .expect("non-empty stack")
                .pointer(p, &mut ctx, &mut fx)
        };
        self.apply(fx);
        consumed
    }

    /// Record what is producing menu events. The hint legend follows it.
    pub(crate) fn note_input_source(&mut self, source: crate::console::InputSource) {
        self.input_source = Some(source);
    }

    /// Keyboard fallback. Arrows and Enter/Esc are menu events; Y/X mirror
    /// Secondary/Tertiary (suppressed while editing — those keys are text).
    /// `shift` only affects Tab.
    pub(crate) fn key(&mut self, key: crate::input::Key, shift: bool, repeat: bool) -> bool {
        use crate::input::Key as S;
        self.last_input = Instant::now();
        self.input_source = Some(crate::console::InputSource::Keys);
        if self.editing() {
            let mut ctx = Ctx {
                hosts: &self.hosts,
                library: &self.library,
                settings: &mut self.settings,
                store: &*self.store,
                platform: self.platform,
                screen: self.screen,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
                pyrowave_ok: self.pyrowave_ok,
                av1_ok: self.av1_ok,
                device_name: &self.device_name,
                t: self.t0.elapsed().as_secs_f64(),
            };
            if let Some(top) = self.stack.last_mut() {
                if top.edit_key(key, &mut ctx) {
                    return true;
                }
            }
            // Editing consumed nothing: arrows still drive the OSK grid.
        }
        let editing = self.stack.last().is_some_and(Screen::editing);
        let ev = match key {
            S::Left => MenuEvent::Move(MenuDir::Left),
            S::Right => MenuEvent::Move(MenuDir::Right),
            S::Up => MenuEvent::Move(MenuDir::Up),
            S::Down => MenuEvent::Move(MenuDir::Down),
            S::Return | S::Space if !repeat => MenuEvent::Confirm,
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
            top.text_input(text);
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
        let ceiling = crate::screens::settings::bitrate_ceiling_kbps(self.platform);
        self.settings.bitrate_kbps = kbps.min(ceiling);
        self.store.save(&self.settings);
        format!(
            "{} Mb/s set as the default",
            self.settings.bitrate_kbps / 1_000
        )
    }

    fn apply(&mut self, fx: Outbox) {
        for cmd in fx.cmds {
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
                self.console.set_speed(Some(SpeedStatus {
                    key: key.clone(),
                    name: host_name.clone(),
                    phase: SpeedPhase::Connecting,
                }));
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
        if let Some(nav) = fx.nav {
            self.apply_nav(nav);
        }
    }

    /// Spring for the next push/pop. Sampled at nav time so a mid-flight
    /// settings change cannot retune the in-progress transition.
    /// Reduced motion stays a spring (`REDUCED_NAV`); `render.rs` flattens
    /// geometry into the crossfade the setting promises.
    fn nav_spec(&self) -> crate::anim::SpringSpec {
        if self.settings.reduce_motion {
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
                } else if self.exit_needs_confirming() {
                    // B at home is the app's exit, and on a TV remote it is the same button
                    // the user has been backing out of screens with — so it lands by accident.
                    // Armed once, fired on the repeat, in the shell's own press-again idiom
                    // rather than a modal it has no other use for.
                    self.exit_armed = Some(std::time::Instant::now());
                    self.show_toast("Press Back again to exit".to_string());
                } else {
                    self.exit_armed = None;
                    self.actions.push_back(OverlayAction::Quit);
                }
            }
        }
    }

    /// Whether Back at the root should arm rather than quit.
    ///
    /// webOS only for now: there the shell IS the app, and Back is the same key used to leave
    /// every screen, so one press too many closes it. A Deck's B at the root is a deliberate
    /// exit to Gaming Mode and stays immediate — widening this is a line here.
    fn exit_needs_confirming(&self) -> bool {
        if self.platform != Platform::WebOS {
            return false;
        }
        self.exit_armed
            .is_none_or(|t| t.elapsed() >= EXIT_CONFIRM_WINDOW)
    }

    /// In-flight spring position. `1.0` when there is no transition, so
    /// callers that compare against `NAV_INPUT_OPENS` treat idle as seated.
    fn nav_pos(&self) -> f64 {
        match &self.motion {
            Motion::None => 1.0,
            Motion::Nav { spring, .. } => spring.pos,
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
            // the entering screen at 0. Refused at the root: B there is quit.
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
        if self.settings.reduce_motion {
            0.0
        } else {
            t
        }
    }

    /// The field as a paint for an `w`×`h` target — `u_res` is the TARGET's pixels,
    /// the shader's `xy/u_res` normalises everything, so the reduced pass's small
    /// offscreen renders the same picture the full surface would.
    fn aurora_paint(&self, w: f64, h: f64, t: f64, calm: f64) -> Option<Paint> {
        // Matches the SkSL block: u_res, u_tc, u_lift, u_scrim (each float2/4).
        let uniforms: [f32; 12] = [
            w as f32,
            h as f32,
            t as f32,
            calm as f32,
            self.mesh_lift[0],
            self.mesh_lift[1],
            self.mesh_lift[2],
            0.0,
            self.mesh_scrim[0],
            self.mesh_scrim[1],
            self.mesh_scrim[2],
            self.mesh_scrim[3],
        ];
        let words = uniforms.map(f32::to_ne_bytes);
        let bytes = words.as_flattened();
        self.mesh
            .make_shader(Data::new_copy(bytes), &[], None)
            .map(|shader| {
                let mut paint = crate::theme::shaded();
                paint.set_shader(shader);
                paint
            })
    }

    fn draw_aurora(&self, canvas: &Canvas, w: f64, h: f64, t: f64, calm: f64) {
        // One clock read: the takeover's `draw_aurora` inherits it.
        let t = self.field_clock(t);
        let reduced = crate::screens::settings::reduce_ui_res(
            &self.settings,
            self.platform,
            self.fallback_ui,
        );
        let mut cache = self.field.borrow_mut();
        if !reduced {
            // Hand the offscreen back while the full-rate path runs — it is dead weight
            // under the resource cache until the switch comes back on.
            cache.take();
            match self.aurora_paint(w, h, t, calm) {
                Some(paint) => {
                    canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
                }
                None => {
                    canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
                }
            }
            return;
        }
        self.draw_field_reduced(canvas, &mut cache, w, h, t, calm);
    }

    /// The reduced-interface pass: the field into a ≤[`FIELD_EDGE`]-px offscreen, blitted
    /// up with bilinear sampling. Re-rendered only when an input moved — size, palette,
    /// calm, or the clock past [`FIELD_STEP`]. The takeover's `calm = 0` and the base
    /// field's share one slot: when both differ each gets a small re-render a frame,
    /// still a fraction of the full-surface cost.
    fn draw_field_reduced(
        &self,
        canvas: &Canvas,
        cache: &mut Option<FieldCache>,
        w: f64,
        h: f64,
        t: f64,
        calm: f64,
    ) {
        let scale = (FIELD_EDGE / w.max(h)).min(1.0);
        let size = ((w * scale).ceil() as i32, (h * scale).ceil() as i32);
        // `t < c.t` is the test clock rewinding, not a direction the field moves.
        let stale = cache.as_ref().is_none_or(|c| {
            c.size != size
                || c.calm != calm
                || c.mesh.0 != self.mesh_palette
                || c.mesh.1 != self.mesh_os
                || t - c.t >= FIELD_STEP
                || t < c.t
        });
        if stale {
            if let Some(mut surface) = field_surface(canvas, size) {
                // u_res is the offscreen's own pixels — `aurora_paint` is resolution-free.
                if let Some(paint) = self.aurora_paint(size.0 as f64, size.1 as f64, t, calm) {
                    surface
                        .canvas()
                        .draw_rect(Rect::from_wh(size.0 as f32, size.1 as f32), &paint);
                    *cache = Some(FieldCache {
                        surface,
                        size,
                        t,
                        calm,
                        mesh: (self.mesh_palette.clone(), self.mesh_os),
                    });
                }
                // A rejected shader keeps whatever the cache held: a stale field beats black.
            } else {
                // No offscreen (context teardown): the full-rate draw is the fallback,
                // never a black frame — the same stance the unreduced path takes.
                match self.aurora_paint(w, h, t, calm) {
                    Some(paint) => {
                        canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
                    }
                    None => {
                        canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
                    }
                }
                return;
            }
        }
        match cache {
            Some(c) => {
                canvas.draw_image_rect_with_sampling_options(
                    c.surface.image_snapshot(),
                    None,
                    Rect::from_wh(w as f32, h as f32),
                    skia_safe::SamplingOptions::new(
                        skia_safe::FilterMode::Linear,
                        skia_safe::MipmapMode::None,
                    ),
                    &crate::theme::shaded(),
                );
            }
            // Stale with nothing cached means the shader rejected — the direct path's
            // own answer.
            None => {
                canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
            }
        }
    }
}

/// The reduced backdrop's retained pass: the offscreen and the inputs it was rendered
/// from — anything that moves one of them is what a re-render keys on.
struct FieldCache {
    surface: Surface,
    /// The offscreen's pixel size (`FIELD_EDGE`-scaled from the surface it blits to).
    size: (i32, i32),
    /// Clock and calm mix baked into the current contents.
    t: f64,
    calm: f64,
    /// `mesh`'s provenance (palette id, OS-theme revision) — a palette change must
    /// re-render even with the clock frozen.
    mesh: (String, Option<u64>),
}

/// The reduced backdrop's offscreen: a GPU render target on `canvas`'s own context
/// where one exists, a raster surface where none does (tests, a software host) — the
/// cheap pass still applies there.
#[cfg(any(feature = "gl", feature = "vulkan-overlay"))]
fn field_surface(canvas: &Canvas, size: (i32, i32)) -> Option<Surface> {
    use skia_safe::gpu;
    let info = skia_safe::ImageInfo::new_n32_premul(size, None);
    canvas
        .recording_context()
        .and_then(|mut rc| {
            gpu::surfaces::render_target(
                &mut rc,
                gpu::Budgeted::Yes,
                &info,
                None,
                gpu::SurfaceOrigin::TopLeft,
                None,
                false,
                None,
            )
        })
        .or_else(|| skia_safe::surfaces::raster(&info, None, None))
}

/// The reduced backdrop's offscreen where the build has no GPU backend: a raster
/// surface — same pass, just CPU-painted.
#[cfg(not(any(feature = "gl", feature = "vulkan-overlay")))]
fn field_surface(_canvas: &Canvas, size: (i32, i32)) -> Option<Surface> {
    skia_safe::surfaces::raster(
        &skia_safe::ImageInfo::new_n32_premul(size, None),
        None,
        None,
    )
}

/// Compile the mesh for a palette and the lift, scrim, and ink it decides.
/// `uniform_size` is checked: [`Shell::draw_aurora`] hand-packs the buffer
/// and a silent layout change would feed the field garbage.
type MeshLook = (RuntimeEffect, [f32; 3], [f32; 4], crate::theme::Ink);

fn build_mesh(palette_id: &str) -> Result<MeshLook> {
    let p = palette(palette_id);
    compile_mesh(&p.mesh_colors(), crate::theme::Ink::of(p), p.ground)
}

/// Follow-system field: a quiet ramp from the theme's own colours, not the
/// curated hue arcs. The desk colour is the point.
fn build_mesh_os(t: &crate::os_theme::OsTheme) -> Result<MeshLook> {
    use crate::os_theme::mix;
    let (bg, fg, ac) = (t.background, t.foreground, t.accent);
    let stops: [(f64, f64, f64); 5] = if t.light {
        // Pale field shades toward its text colour, not black: darkening a
        // pastel strands dark ink on it (see `theme::Ink` scrim).
        [
            mix(bg, fg, 0.10),
            bg,
            bg,
            mix(bg, ac, 0.08),
            mix(bg, ac, 0.18),
        ]
    } else {
        [
            mix(bg, (0.0, 0.0, 0.0), 0.35),
            bg,
            bg,
            mix(bg, ac, 0.12),
            mix(bg, ac, 0.30),
        ]
    };
    compile_mesh(
        &crate::library::mesh_colors_of(&stops),
        crate::theme::Ink::of_os(t),
        bg,
    )
}

fn compile_mesh(
    colors: &[(f64, f64, f64); 16],
    ink: crate::theme::Ink,
    ground: (f64, f64, f64),
) -> Result<MeshLook> {
    let effect = RuntimeEffect::make_for_shader(mesh_sksl(colors), None)
        .map_err(|e| anyhow!("mesh-gradient SkSL: {e}"))?;
    anyhow::ensure!(
        effect.uniform_size() == 48,
        "mesh uniform block is {} bytes, expected 48 (u_res, u_tc, u_lift, u_scrim)",
        effect.uniform_size()
    );
    let g = ground;
    Ok((
        effect,
        [(g.0 * 0.4) as f32, (g.1 * 0.4) as f32, (g.2 * 0.4) as f32],
        [ink.scrim.r, ink.scrim.g, ink.scrim.b, ink.scrim.a],
        ink,
    ))
}

#[cfg(test)]
mod tests;
