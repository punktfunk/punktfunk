//! Session lifecycle: one SDL context on the caller's main thread drives the window,
//! Vulkan presenter, input capture, pumped gamepad service, and the session pump's
//! event/frame channels.
//!
//! Two modes, one loop. **single** (`run_session`) is one `--connect` stream and
//! exits when it ends. **browse** (`run_browse`) idles the console library between
//! streams; overlay actions launch, session end returns to the library.
//!
//! Stdout is the machine interface: `{"ready":true}` after the first presented frame, then
//! once per window while the overlay tier is not Off: `stats: …` (the Advanced Detailed
//! text, lines joined by ` | `) and `stats-json: …` (the snapshot). Logs go to stderr.
//!
//! In-stream chords share Ctrl+Alt+Shift: Q release/engage, M mouse model, D
//! disconnect, S stats tier, V microphone mute.

use crate::input::{Capture, FingerPhase};
use crate::overlay::{
    FrameCtx, Overlay, OverlayAction, OverlayFrame, PointerButton, PointerInput, RingCommand,
    RingFacts, RingInput, SessionPhase,
};
use crate::present_pace::{
    Cadence, CadenceProbe, FrameStore, LatchClock, PresentGate, SourcePacer, MARGIN_MAX_NS,
    MARGIN_STEP_NS,
};
use crate::touch::{Abs, Act};
use crate::vk::{FrameInput, Presenter};
use anyhow::{Context as _, Result};
use pf_client_core::gamepad::{GamepadService, SelectChord};
use pf_client_core::session::{self, DecodeFacts, SessionEvent, SessionHandle, SessionParams};
use pf_client_core::trust::{MouseMode, PresentPriority, StatsVerbosity, TouchMode};
use pf_client_core::video::VulkanDecodeDevice;
use pf_client_core::video::{DecodeHealth, DecodedFrame, DecodedImage};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::hud::{self, HudLine, StatsSnapshot};
use punktfunk_core::quic::HdrMeta;
use punktfunk_core::video_fit::{self, VideoFit};
use sdl3::event::{DisplayEvent, Event, WindowEvent};
use sdl3::keyboard::Mod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// [`SessionOpts::on_connected`]: host fingerprint, then Welcome's management-API
/// port (`0` = none advertised).
pub type ConnectedFn = Box<dyn FnMut([u8; 32], u16)>;

pub struct SessionOpts {
    pub window_title: String,
    pub fullscreen: bool,
    /// Desktop top-left; `None` = primary-display center. Shells pass their own window so
    /// the stream opens on the same monitor (fullscreen follows that display).
    pub window_pos: Option<(i32, i32)>,
    /// OSD tier at start; also gates stdout `stats:` lines. Ctrl+Alt+Shift+S cycles live.
    pub stats_verbosity: StatsVerbosity,
    /// Latched per session. A mouse-only client leaves the default and never sees a finger.
    pub touch_mode: TouchMode,
    /// `Capture` (pointer lock + relative) or `Desktop` (uncaptured absolute). Ctrl+Alt+Shift+M
    /// flips it live; hosts without absolute injection (gamescope) stay captured.
    pub mouse_mode: MouseMode,
    pub invert_scroll: bool,
    /// Send system chords (Alt+Tab, Super) to the host while captured. Off keeps them local.
    /// Applies in both mouse models; desktop mode's unlocked pointer clicking another window
    /// is the way back. See [`apply_capture`].
    pub inhibit_shortcuts: bool,
    /// Quick-action ring blob; empty = the platform default ring.
    pub overlay_actions: String,
    /// `Latency` = newest-wins arrival pacing; `Smooth { buffer }` = FIFO one frame per latch
    /// slot. `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
    pub present_priority: PresentPriority,
    /// Tear-free present (default on). Off asks for a tearing mode; the mode that took is
    /// named in the stats line.
    pub vsync: bool,
    /// Prefer a present mode that drives VRR when the session starts fullscreen.
    pub allow_vrr: bool,
    pub json_status: bool,
    /// Once on `Connected`: host fingerprint and Welcome's management-API port (`0` = none).
    /// This loop stays store-agnostic. The port is the one moment a client has it without mDNS.
    pub on_connected: Option<ConnectedFn>,
    /// `None` is the Skia-free build (stats stay stdout-only). Init failure degrades to `None`
    /// with a warning rather than killing the session. Browse mode requires one.
    pub overlay: Option<Box<dyn Overlay>>,
    /// Starting logical size; `None` = 1280×720. Match-window passes the persisted last size
    /// so the first connect's mode already matches the glass.
    pub window_size: Option<(u32, u32)>,
    /// `Some` = stream mode follows the window: start params use physical pixels, a mid-session
    /// resize sends a debounced `Reconfigure`. The callback gets logical size at each resize-end
    /// for persist. `None` = never auto-resize.
    pub match_window: Option<Box<dyn FnMut(u32, u32)>>,
    /// Multiplier on the window pixel size under Match-window. `> 1` supersamples; `1.0` is
    /// native pixels. See [`punktfunk_core::render_scale`].
    pub render_scale: f64,
    /// Codec per-axis ceiling for the render-scale clamp (4096 for H.264, else 8192).
    pub render_scale_max_dim: u32,
    /// How a frame of another aspect fills the window. The blit and every absolute input
    /// map through the same [`video_fit::place`].
    pub video_fit: VideoFit,
}

pub enum Outcome {
    /// `None` = user quit; `Some` = the reason the pump reported.
    Ended(Option<String>),
    ConnectFailed {
        msg: String,
        trust_rejected: bool,
    },
}

/// Browse-mode overlay action result.
pub enum ActionOutcome {
    Handled,
    /// Launch. Boxed because SessionParams is large next to the unit variants.
    Start(Box<SessionParams>),
    Quit,
}

/// One `--connect` stream; returns when it ends.
pub fn run_session<F>(opts: SessionOpts, build_params: F) -> Result<Outcome>
where
    F: FnOnce(
        &GamepadService,
        Mode,
        Option<HdrMeta>,
        Arc<AtomicBool>,
        Option<VulkanDecodeDevice>,
    ) -> SessionParams,
{
    let mut build = Some(build_params);
    run_inner(
        opts,
        ModeCtl::Single(Box::new(move |gp, native, hdr, fs, vk| {
            (build.take().expect("single build runs once"))(gp, native, hdr, fs, vk)
        })),
    )
    .map(|o| o.expect("single mode always yields an outcome"))
}

/// Console library idles between streams. `on_action` gets every overlay action plus what
/// a launch needs: gamepad service, native display mode, the window display's HDR volume
/// ([`window_display_hdr`]), a fresh `force_software` flag.
pub fn run_browse<F>(opts: SessionOpts, on_action: F) -> Result<()>
where
    F: FnMut(
        OverlayAction,
        &GamepadService,
        Mode,
        Option<HdrMeta>,
        Arc<AtomicBool>,
        Option<VulkanDecodeDevice>,
    ) -> ActionOutcome,
{
    anyhow::ensure!(
        opts.overlay.is_some(),
        "--browse needs the console UI (a build with the `ui` feature)"
    );
    run_inner(opts, ModeCtl::Browse(Box::new(on_action))).map(|_| ())
}

/// Params builder for the one single-mode session (called once, after setup).
type BuildParams<'a> = Box<
    dyn FnMut(
            &GamepadService,
            Mode,
            Option<HdrMeta>,
            Arc<AtomicBool>,
            Option<VulkanDecodeDevice>,
        ) -> SessionParams
        + 'a,
>;
type OnAction<'a> = Box<
    dyn FnMut(
            OverlayAction,
            &GamepadService,
            Mode,
            Option<HdrMeta>,
            Arc<AtomicBool>,
            Option<VulkanDecodeDevice>,
        ) -> ActionOutcome
        + 'a,
>;

/// The two run modes, type-erased so one loop serves both.
enum ModeCtl<'a> {
    Single(BuildParams<'a>),
    Browse(OnAction<'a>),
}

/// Custom SDL event a decoded frame's arrival pushes (see [`StreamState::new`]).
/// Pure wake-up: the loop drains the frame channel regardless of why it woke.
struct FrameWake;

/// Decoded frame plus when the source cadence says it is due on glass. Due time is
/// from the arrival process the store saw, not from whatever survived it.
struct Paced {
    frame: DecodedFrame,
    /// `session::now_ns` domain (`DecodedFrame::decoded_ns` is the same clock). `0`
    /// under the latency intent, which never asks.
    due_ns: i64,
}

/// One stream session's live state. Created at start, dropped at end; browse cycles
/// several per process.
struct StreamState {
    handle: SessionHandle,
    /// Decoded frames, re-queued by the wake forwarder (newest-wins, like the pump).
    /// The loop drains this, never `handle.frames` — the forwarder is that channel's
    /// one consumer.
    frames: async_channel::Receiver<DecodedFrame>,
    connector: Option<Arc<NativeClient>>,
    capture: Option<Capture>,
    force_software: Arc<AtomicBool>,
    /// User canceled this connect: skip capture/attach on a late `Connected` and
    /// route its end back silently.
    canceled: bool,
    ready_announced: bool,
    mode_line: String,
    /// Settings preset this session resolved; `None` = global defaults, nothing shown.
    preset: Option<String>,
    /// Latch grid the pump's PhaseReports read, written by the 1 Hz present-timing fold.
    /// `None` = the session did not advertise phase lock.
    latch_grid: Option<Arc<session::LatchGrid>>,
    /// Host↔client clock offset (`None` until Connected). Loaded per present so a
    /// mid-stream re-sync keeps e2e honest after an NTP step.
    clock_offset: Option<Arc<std::sync::atomic::AtomicI64>>,
    /// Video-leg e2e in ns, published on every presented frame for the audio plane.
    video_e2e: Option<Arc<std::sync::atomic::AtomicU64>>,
    hdr: bool,
    /// OSD `HDR→SDR (raw)`: this lane showed PQ with no tone-map. Nothing sets it
    /// today — every lane goes through planar CSC. Kept so a future bypass can say so.
    hdr_untonemapped: bool,
    /// Per present: D3D11 import lookup (0 off that lane) and `vkQueueSubmit` wall time,
    /// for the presenter window line.
    win_import_us: Vec<u32>,
    win_submit_us: Vec<u32>,
    /// Per present: the in-flight fence wait, `vkAcquireNextImageKHR`, `vkQueuePresentKHR`.
    win_fence_us: Vec<u32>,
    win_acquire_us: Vec<u32>,
    win_present_us: Vec<u32>,
    /// The overlay window (`NativeClient::hud`) closes here once a second.
    win_start: Instant,
    /// Last closed window, so a tier cycle re-renders at once rather than up to 1 s later.
    last_snap: Option<StatsSnapshot>,
    /// Latest decoder facts from the pump, and the integrity counters as of the last window.
    facts: DecodeFacts,
    health_seen: Option<DecodeHealth>,
    /// Glass-gate force-opens in the last window. The VRR probe trusts only healthy windows.
    last_forced: u32,
    /// Newest-wins under latency, smoothing FIFO under smoothness. A smoothing store
    /// holds decoder-pool frames up to `buffer` deep on top of the depth-2 wake
    /// channels — headroom for 1..=3; deeper must revisit pool sizing.
    store: FrameStore<Paced>,
    /// Panel latch grid (present-wait glass stamps; submit-anchored fallback). Smoothness
    /// slot clock, and the values published to the host-facing `latch_grid`.
    clock: LatchClock,
    /// Plays smoothness frames on the source's cadence, not on arrival. Inert under
    /// latency, which never folds a frame into it.
    pacer: SourcePacer,
    /// Source's nominal frame interval: the negotiated stream mode's refresh, never
    /// the panel's. A 120 fps stream on a 60 Hz panel would otherwise license twice the hold.
    source_interval_ns: i64,
    /// FIFO glass budget (one undisplayed present in flight). Inert off FIFO modes or
    /// without present timing.
    gate: PresentGate,
    /// Variable refresh actually live? Measured from on-glass stamps (no portable query).
    cadence: CadenceProbe,
    /// Display mode's refresh period — the vblank grid presents quantize to when VRR is
    /// off. Not the learned period (see the probe's call site).
    mode_period_ns: u64,
    /// Smoothness slot-pick margin: starts 0 (a fixed lead is display tax), widens
    /// +500 µs per >2-miss window toward 2.5 ms.
    margin_ns: u64,
    /// This window's latch misses (glass later than one panel period past submit plus
    /// the applied lead). Adaptive margin's error signal.
    win_misses: u32,
    win_out_max: usize,
    /// One-shot log latch: smoothness was requested but PyroWave collapsed the store
    /// to latency (plane-ring retirement assumes newest-wins).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_latency_forced: bool,
    /// Hardware-path health: a failure streak (or no import support) demotes the
    /// decoder to software via the shared flag — once per session.
    dmabuf_demoted: bool,
    /// PyroWave present has no demote rung. Warn on the first of a streak; stay quiet
    /// until a present succeeds.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_present_warned: bool,
    /// Same latch for the software lane: last rung, so a present failure has nothing
    /// left to demote to.
    cpu_present_warned: bool,
    hw_fails: u32,
    osd: Vec<HudLine>,
    /// Last resize event's stamp. `Some` = pending; the tick fires once ~400 ms pass
    /// with no further size events (never per drag-frame — each switch rebuilds the host).
    resize_pending: Option<Instant>,
    /// When the last `Reconfigure` was sent — ≥ 1 s between requests. The accept ack
    /// round-trips in milliseconds, so this also keeps at most ~one request outstanding.
    resize_sent_at: Option<Instant>,
    /// Last size actually requested. Each distinct size at most once: a rejected size
    /// is not re-asked until it changes, and a host-side rollback cannot loop forever.
    resize_requested: Option<(u32, u32)>,
    /// Connector mode last shown in the HUD/title — a change refreshes both.
    shown_mode: Option<Mode>,
    /// Scrim + spinner. Armed by [`resize_tick`] when it requests a switch; cleared when
    /// a decoded frame reaches the target (or on timeout).
    resize_overlay: ResizeIndicator,
    /// Last presented frame's video dimensions. Touch passthrough maps a finger into
    /// this letterboxed rect; `None` until the first frame, and touches before then drop.
    last_video: Option<(u32, u32)>,
    /// Created with the connector; inert when the host did not negotiate the channel.
    cursor_chan: Option<crate::cursor::CursorChannel>,
    /// Auto-flip fires on changes only, so it never fights a user who chorded away.
    last_hint: Option<bool>,
    /// When `last_hint` last changed; the flip waits out [`HINT_SETTLE`] from here.
    hint_since: std::time::Instant,
    /// When the user last moved the mouse; the local cursor follows host-driven motion only
    /// after [`FOLLOW_HOST_AFTER`] of stillness.
    last_user_motion: std::time::Instant,
    /// Motion events before this are the echo of a follow-warp.
    warp_echo_until: std::time::Instant,
    /// User flipped the model manually. The standing hint stops driving until the
    /// host's intent next changes (a fresh hint edge clears this and applies).
    hint_override: bool,
    /// Last `client_draws` told to the host; `None` = nothing sent yet. Edge-detected
    /// from the live mouse model so chord, auto-flip, and engage/release share one path.
    sent_client_draws: Option<bool>,
    /// Welcome advert, then every mid-session `AccessUpdate` (latest wins). Default is
    /// full control, permanent — what a host that never sent access decodes to.
    access: pf_client_core::access::SessionAccess,
    /// Transient access toast and when it went up — cleared after [`ACCESS_NOTICE_S`].
    /// An access change outranks "click to capture" for a few seconds.
    session_notice: Option<(String, Instant)>,
    /// Gaming Mode touch-as-mouse: drops leaked Steam Input positions sent as deltas, once.
    touch_mouse: crate::touch::SteamTouchMouse,
    /// Host's pinned fingerprint once connected — the key the pre-fetched host-actions cache uses.
    fp_hex: String,
    native_mode: (u32, u32, u32),
    /// Launch params, kept for codec-fallback re-dial. Clone is at start, so a mid-session
    /// accepted mode switch is not in here — the retry re-reads it from the connector.
    /// The latch grid rides by `Arc` (it is the presenter's). `force_software` does not:
    /// it is a per-session demote latch, and the retry replaces it.
    params: SessionParams,
}

impl StreamState {
    /// `wake` pushes a [`FrameWake`] as each decoded frame lands, via a forwarder that
    /// owns the pump's frame channel. The run loop can block in `wait_event_timeout`
    /// and still present the instant a frame arrives. The forwarder exits when the
    /// pump drops its sender.
    fn new(
        params: SessionParams,
        force_software: Arc<AtomicBool>,
        wake: sdl3::event::EventSender,
        priority: PresentPriority,
        native_refresh_hz: u32,
    ) -> StreamState {
        let preset = params.preset.clone();
        // Rate we asked for, until Welcome resolves it. No frames flow before that,
        // so this only has to be sane, not right.
        let source_interval_ns = frame_interval_ns(params.mode.refresh_hz, native_refresh_hz);
        // Presenter's half of phase-locked capture: keep the Arc before the params move.
        // `None` when the session did not advertise the cap — the 1 Hz fold then skips it.
        let latch_grid = params.phase_lock.then(|| params.latch_grid.clone());
        let retry_params = params.clone();
        let handle = session::start(params);
        let (wake_tx, wake_rx) = async_channel::bounded(2);
        let pump_rx = handle.frames.clone();
        let _ = std::thread::Builder::new()
            .name("pf-frame-wake".into())
            .spawn(move || {
                pf_client_core::audio_rt::boost_and_log("frame-wake");
                while let Ok(f) = pump_rx.recv_blocking() {
                    let _ = wake_tx.force_send(f); // newest wins, like the pump's queue
                    let _ = wake.push_custom_event(FrameWake);
                }
            });
        StreamState {
            handle,
            frames: wake_rx,
            connector: None,
            capture: None,
            cursor_chan: None,
            access: pf_client_core::access::SessionAccess::default(),
            session_notice: None,
            touch_mouse: crate::touch::SteamTouchMouse::new(in_gamescope()),
            last_hint: None,
            hint_since: std::time::Instant::now(),
            last_user_motion: std::time::Instant::now(),
            warp_echo_until: std::time::Instant::now(),
            hint_override: false,
            sent_client_draws: None,
            force_software,
            canceled: false,
            ready_announced: false,
            mode_line: String::new(),
            fp_hex: String::new(),
            native_mode: (0, 0, 0),
            preset,
            latch_grid,
            clock_offset: None,
            video_e2e: None,
            hdr: false,
            hdr_untonemapped: false,
            win_import_us: Vec::with_capacity(256),
            win_submit_us: Vec::with_capacity(256),
            win_fence_us: Vec::with_capacity(256),
            win_acquire_us: Vec::with_capacity(256),
            win_present_us: Vec::with_capacity(256),
            win_start: Instant::now(),
            last_snap: None,
            facts: DecodeFacts::default(),
            health_seen: None,
            last_forced: 0,
            store: FrameStore::new(usize::from(priority.fifo_capacity())),
            clock: LatchClock::new(native_refresh_hz),
            pacer: SourcePacer::new(),
            source_interval_ns,
            gate: PresentGate::default(),
            cadence: CadenceProbe::new(),
            mode_period_ns: 1_000_000_000 / u64::from(native_refresh_hz.max(1)),
            margin_ns: 0,
            win_misses: 0,
            win_out_max: 0,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            pyro_latency_forced: false,
            dmabuf_demoted: false,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            pyro_present_warned: false,
            cpu_present_warned: false,
            hw_fails: 0,
            osd: Vec::new(),
            resize_pending: None,
            resize_sent_at: None,
            resize_requested: None,
            shown_mode: None,
            resize_overlay: ResizeIndicator::default(),
            last_video: None,
            params: retry_params,
        }
    }

    /// Stop the pump and join its thread before any device-wide idle: the pump submits
    /// decode work to the shared device. It notices `stop` within its 20 ms receive
    /// timeout; on a normal end it is already returning.
    fn shutdown(mut self) {
        self.handle.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.handle.thread.take() {
            let _ = t.join();
        }
    }

    /// User exit: release capture, close with QUIT_CLOSE_CODE so the host tears down
    /// instead of lingering, stop the pump. The pump then emits `Ended(None)`.
    fn request_quit(&mut self) {
        if let Some(cap) = &mut self.capture {
            cap.release(true);
        }
        if let Some(c) = &self.connector {
            c.disconnect_quit();
        }
        self.handle.stop.store(true, Ordering::SeqCst);
    }

    /// Smoothness with a frame in hand sleeps only to the pass that can still serve it;
    /// everything else uses a 15 ms housekeeping tick. Must stay the present decision's
    /// mirror — a rule changed on one side oversleeps a smooth stream past its due time.
    fn wake_timeout(&self) -> Duration {
        const TICK: Duration = Duration::from_millis(15);
        if !self.store.is_smoothing() {
            return TICK;
        }
        let Some(p) = self.store.front() else {
            return TICK;
        };
        // Free-running presents at the due time. Snapping presents once the aimed slot
        // is the next one still reachable (one period minus submit lead). Before the
        // first on-glass stamp there is no grid; `next_slot_after` answers "one period
        // from now" — mirror that or opening frames wait a refresh they never owed.
        let lead_ns = self.clock.period_ns() as i64 + self.margin_ns as i64;
        let wake_ns = if self.pacer.free_running() {
            p.due_ns
        } else if self.clock.anchor_ns() == 0 {
            p.due_ns - lead_ns
        } else {
            self.clock.next_slot_after(p.due_ns.max(0) as u64) as i64 - lead_ns
        };
        Duration::from_nanos(wake_ns.saturating_sub(session::now_ns() as i64).max(0) as u64)
            .clamp(Duration::from_millis(1), TICK)
    }
}

/// One frame at `refresh_hz`, in ns — the source's nominal interval, and the cadence
/// cushion's ceiling.
///
/// The negotiated stream mode's refresh is the only source-rate signal a client has.
impl StreamState {
    /// Re-seed the latch grid, VRR verdict and pacer anchor from the window's current
    /// display mode. Re-anchoring costs one frame; measured jitter survives, because
    /// that describes the link.
    fn relearn_grid(&mut self, window: &sdl3::video::Window) {
        let hz = window
            .get_display()
            .and_then(|d| d.get_mode())
            .map(|m| m.refresh_rate.round().max(0.0) as u32)
            .unwrap_or(0);
        if hz > 0 {
            self.clock = LatchClock::new(hz);
            self.mode_period_ns = 1_000_000_000 / u64::from(hz);
        }
        self.cadence.reset();
        self.pacer.reset();
        // The slot margin was sized by the old panel's misses.
        self.margin_ns = 0;
        self.win_misses = 0;
        tracing::info!(
            refresh_hz = hz,
            "display changed — relearning the latch grid"
        );
    }
}

/// Measured fps sags when the transport is struggling, which is when a ceiling
/// derived from it would license a bigger hold.
///
/// `0` = "native", which the host resolves to this client's reported display rate.
/// Neither known falls back to 60 Hz, the same last resort [`native_mode`]'s caller takes.
fn frame_interval_ns(refresh_hz: u32, fallback_hz: u32) -> i64 {
    let hz = match (refresh_hz, fallback_hz) {
        (0, 0) => 60,
        (0, f) => f,
        (r, _) => r,
    };
    1_000_000_000 / i64::from(hz)
}

fn ui_wants_pad_mask(focus_lost: bool, overlay_open: bool, capture_active: Option<bool>) -> bool {
    focus_lost || overlay_open || capture_active == Some(false)
}

/// Whether a present error is `VK_ERROR_DEVICE_LOST` in its chain. A lost device is
/// unrecoverable by spec — every object on it is dead, and demote-to-software would
/// rebuild the decoder against that same dead device. Fail the session and let the
/// shell relaunch.
fn device_lost(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<ash::vk::Result>() == Some(&ash::vk::Result::ERROR_DEVICE_LOST))
}

fn run_inner(mut opts: SessionOpts, mut mode: ModeCtl) -> Result<Option<Outcome>> {
    // Before any window exists: unpackaged runs adopt the shell's AppUserModelID so
    // shell⇄session windows group as one taskbar app (MSIX identity wins).
    #[cfg(windows)]
    crate::win32::set_app_user_model_id();
    // This thread presents and forwards input; a late wake is a missed refresh.
    pf_client_core::audio_rt::boost_and_log("presenter");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // Hold Valve HIDAPI off before SDL_Init: the Deck driver clears digital mappings
    // at enumeration. A hint set after `sdl.gamepad()` only detaches a driver that
    // already killed the trackpad-mouse. They are still enabled for an attached session.
    pf_client_core::gamepad::preinit_disable_valve_hidapi();
    // Touch is forwarded as real touch below. Left on, SDL's mouse-from-touch synthesis
    // warps a synthetic mouse; under relative lock that is a large positive delta that
    // walks the host cursor into the corner.
    sdl3::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
    // Wayland `app_id` (and X11 WM_CLASS) so compositors match io.unom.Punktfunk.desktop.
    // Without it SDL uses a generic identity and the session window gets the default icon.
    sdl3::hint::set("SDL_APP_ID", "io.unom.Punktfunk");
    // `PUNKTFUNK_DRM_CARD=<n>` → SDL's KMSDRM device index. SDL takes the first card
    // it can open, often the wrong one on a multi-GPU box. Detecting "already mastered"
    // needs the ioctl that taking master is, so this stays an explicit operator choice.
    if let Ok(card) = std::env::var("PUNKTFUNK_DRM_CARD") {
        if card.chars().all(|c| c.is_ascii_digit()) && !card.is_empty() {
            tracing::info!(
                card,
                "PUNKTFUNK_DRM_CARD: pinning SDL's KMSDRM device index"
            );
            sdl3::hint::set("SDL_KMSDRM_DEVICE_INDEX", &card);
        } else {
            tracing::warn!(
                card,
                "PUNKTFUNK_DRM_CARD must be a card NUMBER (e.g. 0) — ignoring"
            );
        }
    }
    let sdl = sdl3::init().context("SDL init")?;
    let video = sdl.video().context("SDL video")?;
    let events = sdl.event().context("SDL events")?;
    events
        .register_custom_event::<FrameWake>()
        .map_err(|e| anyhow::anyhow!("register FrameWake event: {e}"))?;
    let mut window = {
        // Match-window: open at the persisted last size so the first connect's mode
        // matches the glass. 1280×720 is the fallback.
        let (ww, wh) = opts.window_size.unwrap_or((1280, 720));
        let mut b = video.window(&opts.window_title, ww.max(320), wh.max(200));
        match opts.window_pos {
            Some((x, y)) => b.position(x, y),
            None => b.position_centered(),
        };
        // HIGH_PIXEL_DENSITY: backbuffer in the panel's real pixels. Without it SDL
        // leaves a Wayland surface at buffer scale 1, so a fractionally scaled output
        // builds the swapchain in points. The flag only widens `size_in_pixels()`;
        // `size()` stays logical (persisted size and SDL mouse coords).
        b.resizable().vulkan().high_pixel_density();
        if opts.fullscreen {
            b.fullscreen();
        }
        b.build().context("SDL window")?
    };
    // SDL wheel input remains the fallback when native Wayland capture is unavailable.
    let mut scroll_routing = crate::scroll_routing::ScrollRouting::new(&window);
    // Exe-embedded icon onto the title bar/taskbar; a no-op for exes that embed none.
    #[cfg(windows)]
    crate::win32::stamp_window_icon(&window);
    let instance_exts = window
        .vulkan_instance_extensions()
        .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
    let mut presenter = Presenter::new(
        &window,
        &instance_exts,
        crate::vk::PresentPref {
            vsync: opts.vsync,
            allow_vrr: opts.allow_vrr,
            fullscreen: opts.fullscreen,
            // `vrr_fifo_opt_in` and `fifo_latest_ready` are resolved inside `Presenter::new`.
            // `..Default` keeps this site from breaking when the struct learns another field.
            ..Default::default()
        },
    )
    .context("vulkan presenter")?;
    presenter.set_video_fit(opts.video_fit);
    // A valid black frame immediately — the window is honest while the connect runs.
    presenter.present(&window, FrameInput::Redraw, None)?;

    // `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
    let arrival_override = std::env::var("PUNKTFUNK_PRESENTER").ok().as_deref() == Some("arrival");
    let present_priority = if arrival_override {
        tracing::info!("PUNKTFUNK_PRESENTER=arrival — presentation pacing disabled");
        PresentPriority::Latency
    } else {
        opts.present_priority
    };
    let pacing_active = !arrival_override;
    let present_debug = std::env::var_os("PUNKTFUNK_PRESENT_DEBUG").is_some();
    // Present completions wake the loop like decoded frames: a glass-gate reopen or a
    // smoothness slot must not wait out the event timeout.
    {
        let sender = events.event_sender();
        presenter.set_present_wake(Box::new(move || {
            let _ = sender.push_custom_event(FrameWake);
        }));
    }
    // Browse is "ready" the moment the library window presents — there may never be a
    // stream. Single mode announces on the first video frame instead.
    if opts.json_status && matches!(mode, ModeCtl::Browse(_)) {
        println!("{{\"ready\":true}}");
    }

    // Operator preference on top of the display DPI. Read once (a preference, not
    // session state); the DPI part is re-read per frame.
    let osd_scale_pref = std::env::var("PUNKTFUNK_OSD_SCALE")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);

    let mut overlay = opts.overlay.take();
    if let Some(o) = overlay.as_mut() {
        if let Err(e) = o.init(&presenter.shared_device()) {
            if matches!(mode, ModeCtl::Browse(_)) {
                return Err(e).context("console UI init (required for --browse)");
            }
            tracing::warn!(error = %format!("{e:#}"),
                "console-UI overlay init failed — continuing without it");
            overlay = None;
        }
    }

    let gamepad_subsystem = sdl.gamepad().context("SDL gamepad")?;
    let (gamepad, mut pump) = GamepadService::pumped(gamepad_subsystem);
    let escape_rx = gamepad.escape_events();
    let chord_rx = gamepad.chord_events();
    // A Select chord eats the button pressed with Select, so it is only worth claiming where
    // the overlay it drives exists — a build without the console UI leaves A and X to the game.
    gamepad.set_chords_live(overlay.is_some());
    // Ring pad ownership, edge-tracked: open masks the pads (a held trigger is released
    // on the host) and polls them into menu events; close re-adopts them.
    let mut ring_was_open = false;
    // The pad whose Select+A opened the ring; `None` for a keyboard, touch or closed ring.
    let mut ring_opener: Option<u8> = None;
    // Last audio-mute mask drawn and when it changed: a local mute's badge is timed off it.
    let mut audio_mute_seen: u8 = 0;
    let mut audio_mute_at = Instant::now();
    let disconnect_rx = gamepad.disconnect_events();
    let menu_rx = gamepad.menu_events();
    if matches!(mode, ModeCtl::Browse(_)) {
        // Menu mode for the launcher's lifetime (an attached session supersedes translation).
        gamepad.set_menu_mode(true);
    }
    // Gaming Mode's Steam menu / QAM drive the same physical pad we forward, and
    // gamescope never takes our X focus away, so SDL's background-input gate cannot
    // fire there. `None` everywhere else, where window focus is the signal.
    #[cfg(target_os = "linux")]
    let overlay_focus = pf_client_core::overlay_focus::OverlayFocus::start();
    // Window focus and the gamescope overlay OR into one mask pushed on an edge.
    // Kept as separate inputs: either would otherwise clear the other's mask.
    let mut focus_lost = false;
    let mut mask_applied = false;

    // Native display mode — the `0 = native` fallback for the requested stream mode.
    let native = window
        .get_display()
        .and_then(|d| d.get_mode())
        .map(|m| native_mode(m.w, m.h, m.pixel_density, m.refresh_rate))
        .ok()
        // A zero-sized mode is as useless as no mode. Without this filter a display
        // that reports 0×0 streams a 0×0 request.
        .filter(|m: &Mode| m.width > 0 && m.height > 0)
        .unwrap_or(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });

    let mut stream: Option<StreamState> = match &mut mode {
        ModeCtl::Single(build) => {
            let force_software = Arc::new(AtomicBool::new(false));
            let mut params = build(
                &gamepad,
                native,
                window_display_hdr(&window),
                force_software.clone(),
                presenter.vulkan_decode(),
            );
            if opts.match_window.is_some() {
                apply_match_window(
                    &mut params,
                    &window,
                    opts.render_scale,
                    opts.render_scale_max_dim,
                );
            }
            Some(StreamState::new(
                params,
                force_software,
                events.event_sender(),
                present_priority,
                native.refresh_hz,
            ))
        }
        ModeCtl::Browse(_) => None,
    };

    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
    let mouse = sdl.mouse();

    let mut fullscreen = opts.fullscreen;
    // Latched for the loop's life: `opts` is borrowed mutably for its callbacks at
    // several `apply_capture` sites.
    let inhibit_shortcuts = opts.inhibit_shortcuts;
    let mut stats_verbosity = opts.stats_verbosity;
    let mut overlay_frame: Option<OverlayFrame> = None;
    // SDL text input tracks overlay editing (IME / Steam OSK). Toggled edge-wise —
    // start/stop are not free on Wayland.
    let mut text_input_on = false;
    // Ring Keyboard slot: hold text input on, which summons Steam's OSK under gamescope.
    let mut ring_keyboard = false;
    let mut overlay_damage = OverlayDamage::default();

    let outcome = 'main: loop {
        // Block in SDL's wait: input/window events and decoded frames (FrameWake) all
        // land in this queue. The timeout only bounds stop-flag/pump-tick latency.
        // Smoothness tightens it to the next latch-slot deadline.
        let timeout = stream
            .as_ref()
            .map_or(Duration::from_millis(15), |st| st.wake_timeout());
        let first = event_pump.wait_event_timeout(timeout);
        let mut queued: Vec<Event> = Vec::new();
        if let Some(e) = first {
            queued.push(e);
        }
        while let Some(e) = event_pump.poll_event() {
            queued.push(e);
        }
        scroll_routing.begin(
            stream.as_ref().and_then(|s| s.capture.as_ref()),
            overlay.as_deref(),
        );
        for event in queued {
            // Console UI sees input first: a consumed event never reaches capture/forwarding.
            if let Some(o) = overlay.as_mut() {
                if o.handle_event(&event) {
                    scroll_routing.consumed(&event);
                    continue;
                }
                // Mouse/touch: console hit-tests in its own pixel space. Consumed while
                // the console is up; ignored while streaming (those belong to `Capture`).
                if let Some(input) = overlay_pointer(&event, &window) {
                    if o.handle_pointer(input) {
                        scroll_routing.consumed(&event);
                        continue;
                    }
                }
            }
            match event {
                Event::Quit { .. } => {
                    if let Some(st) = &mut stream {
                        st.request_quit();
                    }
                    break 'main Some(Outcome::Ended(None));
                }
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::FocusLost => {
                        scroll_routing.focus_lost();
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.release(false) {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                                tracing::info!("focus lost — input released");
                            }
                        }
                        // Controllers go with keyboard and mouse. SDL already stops
                        // delivering presses here, but nothing zeroed what the host still
                        // believes is held — masking flushes it neutral.
                        focus_lost = true;
                    }
                    WindowEvent::FocusGained => {
                        // Unlike capture, the controller mask has no "the user meant it"
                        // variant — it only mirrors who owns the pad — so regaining focus
                        // always lifts its half.
                        focus_lost = false;
                        // An auto-release (Alt-Tab) undoes itself; a chord release stays
                        // until the user opts in. With the ring up the grab waits for its close.
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.should_reengage() && cap.engage() && !ring_was_open {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                                tracing::info!("focus gained — input recaptured");
                            }
                        }
                    }
                    WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                        // A driver that refuses the new size must not end the session.
                        // A refused fullscreen swapchain costs the fullscreen, not the
                        // stream: fall back to the geometry that was already working.
                        // A windowed failure still propagates — no smaller state to fall back to.
                        if let Err(e) = presenter.recreate_swapchain(&window) {
                            if !fullscreen {
                                return Err(e);
                            }
                            tracing::warn!(
                                error = format!("{e:#}"),
                                "swapchain recreate failed — leaving fullscreen"
                            );
                            fullscreen = false;
                            if let Err(e) = window.set_fullscreen(false) {
                                tracing::warn!(error = %e, "fullscreen exit failed");
                            }
                            continue;
                        }
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
                        // Match-window: restamp the debounce. The request fires once
                        // ~400 ms pass with no further size events, never per drag-frame.
                        if opts.match_window.is_some() {
                            if let Some(st) = stream.as_mut() {
                                st.resize_pending = Some(Instant::now());
                            }
                        }
                    }
                    // Dragged to another monitor: latch grid and VRR verdict belong to
                    // the old panel. A 60 Hz-seeded clock must not keep pacing a 144 Hz panel.
                    WindowEvent::DisplayChanged(..) => {
                        if let Some(st) = stream.as_mut() {
                            st.relearn_grid(&window);
                        }
                    }
                    WindowEvent::Exposed => {
                        presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
                    }
                    _ => {}
                },
                // The panel's rate changed under the window (60 ↔ 165 Hz in the OS
                // settings): no window event fires, and the grid describes the old rate.
                Event::Display {
                    display_event:
                        DisplayEvent::CurrentModeChanged | DisplayEvent::DesktopModeChanged,
                    display,
                    ..
                } if window.get_display().is_ok_and(|d| d == display) => {
                    if let Some(st) = stream.as_mut() {
                        st.relearn_grid(&window);
                    }
                }
                // Windows never auto-repeats injected input, so a held key needs these.
                // Chords and toggles below fire on the first press only.
                Event::KeyDown {
                    scancode: Some(sc),
                    repeat: true,
                    ..
                } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_repeat(sc);
                    }
                }
                Event::KeyDown {
                    keycode,
                    scancode: Some(sc),
                    keymod,
                    repeat: false,
                    ..
                } => {
                    let chord = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                        && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD)
                        && keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                    use sdl3::keyboard::{Keycode, Scancode};
                    // The letter the layout prints on the key (AZERTY's Q sits on Scancode::A),
                    // or the physical position for a layout that has no such letter.
                    let key = |k: Keycode, s: Scancode| keycode == Some(k) || sc == s;
                    if chord && key(Keycode::Q, Scancode::Q) {
                        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                            if cap.captured() {
                                cap.release(true);
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                            } else if cap.engage() {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            }
                            tracing::info!(captured = cap.captured(), "chord: release/engage");
                        }
                        continue;
                    }
                    // Mouse model flip. Applies immediately when engaged; a released
                    // stream just changes what the next engage does.
                    if chord && key(Keycode::M, Scancode::M) {
                        if let Some(st) = stream.as_mut() {
                            let mut flipped = false;
                            if let Some(cap) = st.capture.as_mut() {
                                match cap.toggle_desktop() {
                                    Some(desktop) => {
                                        if cap.captured() {
                                            apply_capture(
                                                &mut window,
                                                &mouse,
                                                true,
                                                desktop,
                                                inhibit_shortcuts,
                                                cap.grants(),
                                            );
                                        }
                                        flipped = true;
                                        tracing::info!(desktop, "chord: mouse mode");
                                    }
                                    None => tracing::info!(
                                        "chord: mouse mode — host has no absolute pointer \
                                         (gamescope), staying captured"
                                    ),
                                }
                            }
                            // A manual flip outranks the standing hint until the host's
                            // intent next changes (the hint edge clears this).
                            if flipped {
                                st.hint_override = true;
                            }
                        }
                        continue;
                    }
                    if chord && key(Keycode::D, Scancode::D) {
                        if let Some(st) = &mut stream {
                            tracing::info!("chord: disconnect");
                            st.request_quit();
                            apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                        }
                        continue;
                    }
                    if chord && key(Keycode::S, Scancode::S) {
                        bump_stats_tier(&mut stats_verbosity, &mut stream);
                        tracing::info!(tier = ?stats_verbosity, "chord: stats verbosity");
                        continue;
                    }
                    // Quick-action ring at the window centre (a locked pointer has no
                    // position worth opening at).
                    if chord && key(Keycode::O, Scancode::O) {
                        if let (Some(o), true) = (overlay.as_mut(), stream.is_some()) {
                            let (pw, ph) = window.size_in_pixels();
                            o.ring_input(RingInput::Toggle {
                                x: pw as f32 / 2.0,
                                y: ph as f32 / 2.0,
                            });
                        }
                        continue;
                    }
                    // Mic mute — per session, never persisted. The uplink keeps running;
                    // only sending stops. A session with no mic says so instead of
                    // swallowing the chord.
                    if chord && key(Keycode::V, Scancode::V) {
                        if let Some(st) = &stream {
                            match st.handle.mic.toggle() {
                                Some(muted) => tracing::info!(muted, "chord: microphone mute"),
                                None => tracing::info!(
                                    "chord: microphone mute — this session streams no \
                                     microphone (turn it on in Settings)"
                                ),
                            }
                        }
                        continue;
                    }
                    // F11 or Alt+Enter (some Fn layers send a media key for plain F11).
                    let alt_enter =
                        sc == Scancode::Return && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    if sc == Scancode::F11 || alt_enter {
                        fullscreen = !fullscreen;
                        tracing::debug!(fullscreen, "fullscreen toggle");
                        if let Err(e) = window.set_fullscreen(fullscreen) {
                            tracing::warn!(error = %e, fullscreen, "fullscreen toggle failed");
                        }
                        continue;
                    }
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_down(sc);
                    }
                }
                Event::KeyUp {
                    scancode: Some(sc), ..
                } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_key_up(sc);
                    }
                }
                Event::MouseMotion {
                    x, y, xrel, yrel, ..
                } => {
                    if let Some(st) = stream.as_mut() {
                        let video = st.last_video;
                        // The echo of our own follow-warp is not the user moving.
                        if Instant::now() >= st.warp_echo_until {
                            st.last_user_motion = Instant::now();
                        }
                        if let Some(cap) = st.capture.as_mut() {
                            if cap.desktop() {
                                // Desktop model: window position through the placement.
                                // Before the first decoded frame there is nothing to map
                                // onto — dropped, like touch.
                                if let Some(video) = video {
                                    let (lw, lh) = window.size();
                                    let nx = x / lw.max(1) as f32;
                                    let ny = y / lh.max(1) as f32;
                                    cap.on_motion_abs(finger_to_frame(
                                        opts.video_fit,
                                        window.size_in_pixels(),
                                        video,
                                        nx,
                                        ny,
                                    ));
                                }
                            } else if st.touch_mouse.leaks(xrel, yrel) {
                                // Gaming Mode touch-as-mouse: a leaked position, not a
                                // delta — dropped, and said once.
                                if st.touch_mouse.take_notice() {
                                    tracing::warn!(
                                        xrel,
                                        yrel,
                                        "Steam Input is replaying the touchscreen as a mouse — \
                                         dropping the leaked positions"
                                    );
                                    st.session_notice = Some((
                                        "Steam Input is sending the touchscreen as a mouse — \
                                         pick the Punktfunk controller layout for touch"
                                            .into(),
                                        Instant::now(),
                                    ));
                                }
                            } else {
                                cap.on_motion(xrel, yrel);
                            }
                        }
                    }
                }
                Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        if !cap.captured() {
                            // The engaging click is not forwarded. `engage` refuses when
                            // access covers neither pointer nor keyboard — the click then
                            // does nothing.
                            if cap.engage() {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            }
                        } else {
                            cap.on_button_down(mouse_btn);
                        }
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                        cap.on_button_up(mouse_btn);
                    }
                }
                Event::MouseWheel { x, y, .. } => {
                    // The overlay consumes SDL wheels before native/fallback routing.
                    scroll_routing.wheel(stream.as_mut().and_then(|s| s.capture.as_mut()), x, y);
                }
                // Touchscreen fingers → the session's touch model. `x`/`y` are
                // window-normalized; only DIRECT devices (an INDIRECT trackpad drives
                // the mouse). A three-finger tap returns `cycle` → bump the stats tier.
                Event::FingerDown {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        if let Some(st) = stream.as_mut() {
                            if !st.touch_mouse.fingers_seen {
                                tracing::info!(
                                    touch_id,
                                    "first touchscreen finger: direct touch reaches the client"
                                );
                            }
                            st.touch_mouse.fingers_seen = true;
                        }
                        if ring_finger(&mut overlay, &window, FingerPhase::Down, x, y) {
                            continue;
                        }
                        for act in dispatch_finger(
                            FingerPhase::Down,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    } else if let Some(st) = stream.as_mut() {
                        // A finger from a device SDL does not call a touchscreen: ignored,
                        // and said once — otherwise "touch arrived and was thrown away"
                        // is indistinguishable from "no touch arrived".
                        if !st.touch_mouse.indirect_seen {
                            st.touch_mouse.indirect_seen = true;
                            tracing::info!(
                                touch_id,
                                "finger from a non-direct touch device — ignored (a trackpad \
                                 drives the mouse)"
                            );
                        }
                    }
                }
                Event::FingerMotion {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        if ring_finger(&mut overlay, &window, FingerPhase::Move, x, y) {
                            continue;
                        }
                        for act in dispatch_finger(
                            FingerPhase::Move,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    }
                }
                Event::FingerUp {
                    touch_id,
                    finger_id,
                    x,
                    y,
                    timestamp,
                    ..
                } => {
                    if is_direct_touch(touch_id) {
                        // The lift also reaches the engine, so it never keeps a finger
                        // that is gone.
                        ring_finger(&mut overlay, &window, FingerPhase::Up, x, y);
                        for act in dispatch_finger(
                            FingerPhase::Up,
                            &window,
                            &mut stream,
                            finger_id,
                            x,
                            y,
                            timestamp,
                            opts.video_fit,
                        ) {
                            on_touch_act(act, &mut stats_verbosity, &mut stream, &mut overlay);
                        }
                    }
                }
                // FrameWake (and any other user event): pure wake-up — the frame drain
                // runs this iteration either way.
                Event::User { .. } => {}
                other => pump.handle_event(other),
            }
        }
        // Native events forward only when capture owns the entire SDL batch.
        scroll_routing.finish(
            stream.as_mut().and_then(|s| s.capture.as_mut()),
            overlay.as_deref(),
        );
        // Who owns the pad: capture, window focus, and Gaming Mode's overlay signal.
        // Edge-triggered so an open QAM does not re-flush the pads every iteration.
        #[cfg(target_os = "linux")]
        let overlay_now = overlay_focus.as_ref().is_some_and(|of| of.is_open());
        #[cfg(not(target_os = "linux"))]
        let overlay_now = false;
        // Remembered, not applied: the ring is a third owner of this same mask and is only
        // known further down. Applying here too gave one boolean two latches, and whichever
        // fell last unmasked the pads while the other still wanted them masked.
        let capture_active = stream
            .as_ref()
            .and_then(|st| st.capture.as_ref())
            .map(Capture::captured);
        let want_mask_ui = ui_wants_pad_mask(focus_lost, overlay_now, capture_active);
        pump.tick();
        // One coalesced MouseMove per iteration — pure motion must reach the host
        // without waiting for a click/key to flush it.
        if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
            cap.flush_motion();
        }
        // Drain forwarded cursor shape/state and drive the local OS cursor — only
        // meaningful in the desktop mouse model (capture's relative lock hides it).
        if let Some(st) = stream.as_mut() {
            // Host-framebuffer px → cursor-surface px: the placement scale (physical px per
            // host px) over what the backend scales a cursor by itself, so the pointer is the
            // size it has in the picture, as on Apple. Stretch keeps the shape undistorted.
            let cursor_scale = st.last_video.map_or(1.0, |video| {
                let p = video_fit::place(opts.video_fit, window.size_in_pixels(), video);
                p.scale_x.min(p.scale_y) as f32 / cursor_surface_scale(&window)
            });
            if let (Some(chan), Some(c)) = (st.cursor_chan.as_mut(), st.connector.as_ref()) {
                let desktop_active = st
                    .capture
                    .as_ref()
                    .is_some_and(|cap| cap.captured() && cap.desktop());
                chan.pump(c, &mouse, desktop_active, cursor_scale);
                // We draw the pointer while released (a released cursor over a composited one is
                // a frozen twin), in desktop mode, or relative on the host's hint: only the state
                // it keeps forwarding can clear the hint. Without the pointer grant the host
                // pointer is someone else's, so the host draws it.
                let hint_relative = !st.hint_override && st.last_hint == Some(true);
                let client_draws = match st.capture.as_ref() {
                    Some(cap) => {
                        (!cap.captured() || cap.desktop() || hint_relative)
                            && cap.grants() & punktfunk_core::quic::GRANT_POINTER != 0
                    }
                    None => true,
                };
                if chan.negotiated() && st.sent_client_draws != Some(client_draws) {
                    st.sent_client_draws = Some(client_draws);
                    let _ = c.set_cursor_render(client_draws);
                }
            }
            // Host-driven mode flip: `relative_hint` set = run captured relative; clear
            // = return to absolute. Edge-triggered so a manual chord is not fought: the
            // override latch holds until the host's intent next changes. The hint must hold
            // [`HINT_SETTLE`] with no button down (Windows hides the pointer for a click or a
            // keystroke), and a grab needs the pointer over this window.
            let hint_state = st.cursor_chan.as_ref().and_then(|ch| ch.state());
            if let Some(hs) = hint_state {
                let hint = hs.relative_hint();
                if st.last_hint != Some(hint) {
                    st.last_hint = Some(hint);
                    st.hint_override = false;
                    st.hint_since = std::time::Instant::now();
                }
                if !st.hint_override && st.hint_since.elapsed() >= HINT_SETTLE {
                    let video = st.last_video;
                    let over_us = !hint || mouse.focused_window_id() == Some(window.id());
                    if let Some(cap) = st.capture.as_mut() {
                        if cap.captured()
                            && !cap.buttons_held()
                            && over_us
                            && !ring_was_open
                            && cap.set_desktop(!hint)
                        {
                            apply_capture(
                                &mut window,
                                &mouse,
                                true,
                                cap.desktop(),
                                inhibit_shortcuts,
                                cap.grants(),
                            );
                            if cap.desktop() {
                                // Reappear where the host last had the pointer so the
                                // hand-back is seamless.
                                if let Some(video) = video {
                                    let (wx, wy) = content_to_window(
                                        opts.video_fit,
                                        window.size(),
                                        window.size_in_pixels(),
                                        video,
                                        hs.x,
                                        hs.y,
                                    );
                                    mouse.warp_mouse_in_window(&window, wx, wy);
                                }
                            }
                            tracing::info!(
                                desktop = cap.desktop(),
                                "host cursor hint: mouse model flipped"
                            );
                        }
                    }
                }
                // Something else moved the pointer we draw (controller mouse, a trackpad
                // gesture, an app warping it): once the user's own mouse has been still for
                // longer than a round trip, the local cursor goes where the host put it.
                let over_us = mouse.focused_window_id() == Some(window.id());
                let still = st.last_user_motion.elapsed() >= FOLLOW_HOST_AFTER;
                if let (Some(cap), Some(video)) = (st.capture.as_mut(), st.last_video) {
                    let drifted = cap.last_abs().is_some_and(|(x, y)| {
                        (x - hs.x).abs() > FOLLOW_SLACK_PX || (y - hs.y).abs() > FOLLOW_SLACK_PX
                    });
                    if cap.captured()
                        && cap.desktop()
                        && hs.visible()
                        && over_us
                        && still
                        && drifted
                    {
                        let (wx, wy) = content_to_window(
                            opts.video_fit,
                            window.size(),
                            window.size_in_pixels(),
                            video,
                            hs.x,
                            hs.y,
                        );
                        st.warp_echo_until = Instant::now() + WARP_ECHO;
                        mouse.warp_mouse_in_window(&window, wx, wy);
                        cap.followed_host((hs.x, hs.y));
                    }
                }
            }
        }

        let want_text = overlay.as_ref().is_some_and(|o| o.text_input_active());
        if want_text != text_input_on {
            text_input_on = want_text;
            let ti = video.text_input();
            if want_text {
                ti.start(&window);
            } else {
                ti.stop(&window);
            }
        }

        // Select chords on a pad. `Select+A` puts the ring at the window centre, where its
        // highlight starts on the centre so `Select+A` then `A` opens the sheet; `Select+X`
        // steps the stats tier, the same move the keyboard chord and the dial's own slot make.
        while let Ok((pad, chord)) = chord_rx.try_recv() {
            if overlay.is_none() || stream.is_none() {
                continue;
            }
            match chord {
                SelectChord::Ring => {
                    if let Some(o) = overlay.as_mut() {
                        ring_opener = Some(pad);
                        let (pw, ph) = window.size_in_pixels();
                        o.ring_input(RingInput::Toggle {
                            x: pw as f32 / 2.0,
                            y: ph as f32 / 2.0,
                        });
                    }
                }
                SelectChord::Stats => {
                    bump_stats_tier(&mut stats_verbosity, &mut stream);
                    tracing::info!(tier = ?stats_verbosity, "chord: stats verbosity");
                }
            }
        }
        // While the ring is up, or the console holds a launch over the live stream, the
        // pad belongs to the overlay: masked off the wire, polled into menu events. The
        // three gates that keep pad input off client UI flip together.
        let ring_open = stream.is_some()
            && overlay
                .as_ref()
                .is_some_and(|o| o.ring_open() || o.holds_stream());
        if ring_open != ring_was_open {
            ring_was_open = ring_open;
            gamepad.set_ring_nav(ring_open);
            if !ring_open {
                ring_opener = None;
            }
            // The ring eats every pointer event, so a button already down would stay pressed
            // on the host without a flush. It also needs a pointer to aim with: under a lock
            // the cursor is hidden and every event carries the position the lock froze, so
            // capture hands the local one back while it is up and takes the window on close.
            if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                if ring_open {
                    cap.flush_held();
                }
                if cap.captured() {
                    let (on, desktop, grants) = if ring_open {
                        (false, false, 0)
                    } else {
                        (true, cap.desktop(), cap.grants())
                    };
                    apply_capture(&mut window, &mouse, on, desktop, inhibit_shortcuts, grants);
                }
            }
        }
        // One owner for the mask: any gate that wants the pads keeps them masked.
        let want_mask = want_mask_ui || ring_open;
        if want_mask != mask_applied {
            mask_applied = want_mask;
            gamepad.set_masked(want_mask);
        }
        if ring_open {
            while let Ok(ev) = menu_rx.try_recv() {
                if let Some(o) = overlay.as_mut() {
                    o.handle_menu(ev);
                }
            }
        }

        // Controller escape chord: release capture (+ leave fullscreen on desktop —
        // under a `--fullscreen` gamescope launch there is nothing to release into).
        while escape_rx.try_recv().is_ok() {
            if let Some(cap) = stream.as_mut().and_then(|s| s.capture.as_mut()) {
                if cap.release(true) {
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                }
            }
            if fullscreen && !opts.fullscreen {
                fullscreen = false;
                let _ = window.set_fullscreen(false);
            }
        }
        // Escape chord held past the threshold: the controller's disconnect.
        if disconnect_rx.try_recv().is_ok() {
            if let Some(st) = &mut stream {
                tracing::info!("controller chord: disconnect");
                st.request_quit();
                apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
            }
        }

        if let ModeCtl::Browse(on_action) = &mut mode {
            // Menu events flow while no stream is engaged — including a connect in
            // flight, so B can cancel the dial. Once attached, the worker forwards raw
            // input instead.
            if stream.as_ref().is_none_or(|s| s.connector.is_none()) {
                while let Ok(ev) = menu_rx.try_recv() {
                    if let Some(o) = overlay.as_mut() {
                        if let Some(pulse) = o.handle_menu(ev) {
                            gamepad.menu_rumble(pulse);
                        }
                    }
                }
            }
            if let Some(action) = overlay.as_mut().and_then(|o| o.take_action()) {
                match action {
                    OverlayAction::CancelConnect => {
                        if let Some(st) = &mut stream {
                            if st.connector.is_none() && !st.canceled {
                                tracing::info!("connect canceled from the console");
                                st.canceled = true;
                                st.handle.stop.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    // The console already toasted "Link copied"; a clipboard SDL refuses
                    // is a log line, not a contradiction of the toast.
                    OverlayAction::CopyText(text) => {
                        if let Err(e) = video.clipboard().set_clipboard_text(&text) {
                            tracing::warn!(error = %e, "copying to the clipboard");
                        }
                    }
                    action => {
                        let force_software = Arc::new(AtomicBool::new(false));
                        match on_action(
                            action,
                            &gamepad,
                            native,
                            window_display_hdr(&window),
                            force_software.clone(),
                            presenter.vulkan_decode(),
                        ) {
                            ActionOutcome::Handled => {}
                            ActionOutcome::Start(mut params) => {
                                if opts.match_window.is_some() {
                                    apply_match_window(
                                        &mut params,
                                        &window,
                                        opts.render_scale,
                                        opts.render_scale_max_dim,
                                    );
                                }
                                // Adopt the tier this launch resolved. The console outlives
                                // every stream. Not in `StreamState::new`: a codec-fallback
                                // retry rebuilds from a clone of these params and would snap
                                // the overlay back, undoing a chord the user had just made.
                                stats_verbosity = params.stats_verbosity;
                                // A live pump here would be detached by the assignment —
                                // `StreamState` has no `Drop`, so its thread would keep
                                // decoding onto the shared Vulkan device. Every other
                                // replacement site takes-and-shuts-down; so does this one.
                                if let Some(prev) = stream.take() {
                                    tracing::warn!(
                                        "launch while a session was still attached — \
                                         stopping it first"
                                    );
                                    prev.shutdown();
                                }
                                stream = Some(StreamState::new(
                                    *params,
                                    force_software,
                                    events.event_sender(),
                                    present_priority,
                                    native.refresh_hz,
                                ));
                                if let Some(o) = overlay.as_mut() {
                                    o.session_phase(SessionPhase::Connecting);
                                }
                            }
                            ActionOutcome::Quit => break Some(Outcome::Ended(None)),
                        }
                    }
                }
            }
        }

        // `stream` may become None mid-drain (browse-mode session end) — re-borrow each
        // event and stop draining on the terminal ones.
        while let Some(st) = stream.as_mut() {
            let Ok(ev) = st.handle.events.try_recv() else {
                break;
            };
            match ev {
                SessionEvent::Connected {
                    connector: c,
                    mode: m,
                    fingerprint,
                } => {
                    if st.canceled {
                        // The dial won the race against the cancel: quit-close the host
                        // now; the stop flag (already set) ends the pump without engaging.
                        c.disconnect_quit();
                        continue;
                    }
                    st.mode_line = format!("{}×{}@{}", m.width, m.height, m.refresh_hz);
                    st.native_mode = (m.width, m.height, m.refresh_hz);
                    st.fp_hex = pf_client_core::trust::hex(&fingerprint);
                    // Pre-fetch the ring's host-action slots here, never when it opens.
                    let host_addr = st.params.host.clone();
                    pf_client_core::host_actions::refresh(&host_addr, c.mgmt_port(), &st.fp_hex);
                    // The resolved rate — a `0 = native` request becomes a real number
                    // here, last moment before frames start arriving.
                    st.source_interval_ns = frame_interval_ns(m.refresh_hz, native.refresh_hz);
                    tracing::info!(mode = %st.mode_line, "connected");
                    // Which touch devices SDL sees. Under gamescope this is the tell
                    // for whether Steam Input hands the touchscreen through as touch:
                    // no DIRECT device, no twist can arrive.
                    tracing::info!(
                        devices = ?touch_devices(),
                        gamescope = in_gamescope(),
                        "touch devices"
                    );
                    window
                        .set_title(&format!("{} · {}", opts.window_title, st.mode_line))
                        .ok();
                    gamepad.attach(c.clone());
                    st.clock_offset = Some(c.clock_offset_shared());
                    st.video_e2e = Some(c.video_e2e_shared());
                    // gamescope's EIS grants only a relative pointer — absolute would be
                    // dropped, so desktop mode is pinned off. Auto (a host that never
                    // said) stays allowed.
                    let abs_ok = c.resolved_compositor != CompositorPref::Gamescope;
                    if opts.mouse_mode == MouseMode::Desktop && !abs_ok {
                        tracing::info!(
                            "desktop mouse mode unavailable on a gamescope host \
                             (relative-only input) — using capture"
                        );
                    }
                    // Access off the Welcome. The pump's Access event lands in this drain,
                    // but capture below must be built gated, not re-gated a beat later.
                    st.access = pf_client_core::access::SessionAccess::from_connector(&c);
                    // Passthrough needs a host that injects touch. Without the bit every
                    // contact would vanish with no error, so the session runs the trackpad
                    // model and the notice says so.
                    let touch_mode = if opts.touch_mode == TouchMode::Touch
                        && c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH == 0
                    {
                        st.session_notice = Some((
                            "This host does not accept touch — using the trackpad model".into(),
                            Instant::now(),
                        ));
                        TouchMode::Trackpad
                    } else {
                        opts.touch_mode
                    };
                    let mut cap = Capture::new(
                        c.clone(),
                        touch_mode,
                        opts.invert_scroll,
                        opts.mouse_mode,
                        abs_ok,
                        st.access.grants,
                    );
                    // Capture engages when the stream starts unless access covers neither
                    // pointer nor keyboard, where `engage` refuses and the pointer stays free.
                    if cap.engage() {
                        apply_capture(
                            &mut window,
                            &mouse,
                            true,
                            cap.desktop(),
                            inhibit_shortcuts,
                            cap.grants(),
                        );
                    }
                    st.capture = Some(cap);
                    st.cursor_chan = Some(crate::cursor::CursorChannel::new(&c));
                    // Read the mgmt port before `c` is moved into `st` — the Welcome's
                    // library address, which the binary persists so it survives without mDNS.
                    let mgmt_port = c.mgmt_port();
                    st.connector = Some(c);
                    if let Some(f) = opts.on_connected.as_mut() {
                        f(fingerprint, mgmt_port);
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.session_phase(SessionPhase::Streaming);
                    }
                }
                SessionEvent::DecodeFacts(f) => st.facts = f,
                // Welcome advert first, then every mid-session AccessUpdate. Re-gate live
                // capture: a removed POINTER/KEYBOARD bit releases the lock it backed;
                // with neither class left the capture drops (auto-release, so a later
                // re-grant re-engages on click).
                SessionEvent::Notice(n) => {
                    st.session_notice = Some((n, Instant::now()));
                }
                SessionEvent::Access { access, notice } => {
                    st.access = access;
                    if let Some(n) = notice {
                        tracing::info!(notice = %n, "session access changed");
                        st.session_notice = Some((n, Instant::now()));
                    }
                    if let Some(cap) = st.capture.as_mut() {
                        cap.set_grants(access.grants);
                        if cap.captured() {
                            // With the ring up the pointer stays the ring's; its close re-applies.
                            if cap.can_capture() && !ring_was_open {
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    true,
                                    cap.desktop(),
                                    inhibit_shortcuts,
                                    cap.grants(),
                                );
                            } else if !cap.can_capture() {
                                cap.release(false);
                                apply_capture(
                                    &mut window,
                                    &mouse,
                                    false,
                                    false,
                                    inhibit_shortcuts,
                                    0,
                                );
                            }
                        }
                    }
                }
                SessionEvent::Failed {
                    msg,
                    trust_rejected,
                } => match &mode {
                    ModeCtl::Single(_) => {
                        break 'main Some(Outcome::ConnectFailed {
                            msg,
                            trust_rejected,
                        })
                    }
                    ModeCtl::Browse(_) => {
                        tracing::warn!(%msg, "connect failed — back to the console");
                        let canceled = st.canceled;
                        if let Some(st) = stream.take() {
                            st.shutdown();
                        }
                        apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                        if let Some(o) = overlay.as_mut() {
                            if canceled {
                                o.session_phase(SessionPhase::Ended(None));
                            } else {
                                o.session_phase(SessionPhase::Failed(&msg));
                            }
                        }
                        break;
                    }
                },
                SessionEvent::Ended(reason) => {
                    gamepad.detach();
                    if let Some(cap) = &mut st.capture {
                        cap.release(true);
                    }
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                    match &mode {
                        ModeCtl::Single(_) => break 'main Some(Outcome::Ended(reason)),
                        ModeCtl::Browse(_) => {
                            window.set_title(&opts.window_title).ok();
                            let canceled = st.canceled;
                            if let Some(st) = stream.take() {
                                st.shutdown();
                            }
                            if let Some(o) = overlay.as_mut() {
                                o.session_phase(SessionPhase::Ended(if canceled {
                                    None
                                } else {
                                    reason.as_deref()
                                }));
                            }
                            break;
                        }
                    }
                }
                // The negotiated codec ran out of decode rungs: re-dial the same host
                // with that codec removed from advertised caps. The pump left nothing of
                // its own running before sending this, so this is a clean start, not an
                // overlap. Applies in both modes — single has no console to fall back to.
                SessionEvent::CodecFallback {
                    exclude_codecs,
                    retry_caps,
                    msg,
                } => {
                    tracing::warn!(
                        %msg,
                        exclude_codecs,
                        retry_caps,
                        "decode ladder exhausted — reconnecting with reduced codec caps"
                    );
                    gamepad.detach();
                    if let Some(cap) = &mut st.capture {
                        cap.release(true);
                    }
                    apply_capture(&mut window, &mouse, false, false, inhibit_shortcuts, 0);
                    // Widen the exclusion rather than replace it: a second fallback must
                    // not re-offer what the first already ruled out.
                    let mut params = st.params.clone();
                    params.exclude_codecs |= exclude_codecs;
                    // The mode this session ended on, not the one it dialled with: a
                    // mid-session `Reconfigure` lives only in the connector, and
                    // `st.params` is a launch clone.
                    if let Some(c) = &st.connector {
                        params.mode = c.mode();
                    }
                    // Then the window follower on top, so a retry lands on the size the
                    // window is now.
                    if opts.match_window.is_some() {
                        apply_match_window(
                            &mut params,
                            &window,
                            opts.render_scale,
                            opts.render_scale_max_dim,
                        );
                    }
                    // A fresh demote flag, like `ActionOutcome::Start` — never the old
                    // session's. Inheriting it would open a software decoder on good hardware.
                    let force_software = Arc::new(AtomicBool::new(false));
                    params.force_software = force_software.clone();
                    // `params.launch` rides along verbatim. Dropping it would miss
                    // `pf-vdisplay`'s reuse key (it includes the launch command) and
                    // orphan the running game inside the lingering display. A `gog:`/
                    // `custom:` target may start a second copy; Steam/Epic URIs dedupe.
                    if let Some(st) = stream.take() {
                        st.shutdown();
                    }
                    if let Some(o) = overlay.as_mut() {
                        o.session_phase(SessionPhase::Reconnecting(&msg));
                    }
                    stream = Some(StreamState::new(
                        params,
                        force_software,
                        events.event_sender(),
                        present_priority,
                        native.refresh_hz,
                    ));
                    break;
                }
            }
        }

        // HUD/title follow the live mode slot on any accepted switch — also when the
        // match-window follower is off (another trigger, or a host-side rollback).
        if let Some(st) = stream.as_mut() {
            hud_mode_tick(st, &mut window, &opts.window_title);
        }
        if let Some(persist) = opts.match_window.as_mut() {
            if let Some(st) = stream.as_mut() {
                resize_tick(
                    st,
                    &mut window,
                    persist.as_mut(),
                    opts.render_scale,
                    opts.render_scale_max_dim,
                );
            }
        }
        // A switch the host rejected/capped never delivers the exact target frame —
        // drop the scrim so it cannot linger.
        if let Some(st) = stream.as_mut() {
            st.resize_overlay.tick(Instant::now());
        }
        // Touch long-press: a still finger raises no SDL event, so the gesture engine
        // needs the clock — SDL ticks, the millisecond base the finger timestamps use.
        if let Some(cap) = stream.as_mut().and_then(|st| st.capture.as_mut()) {
            cap.tick(sdl3::timer::ticks() as f64);
        }
        let mut ring_cmds = Vec::new();
        if let (Some(o), true) = (overlay.as_mut(), stream.is_some()) {
            while let Some(cmd) = o.take_ring_command() {
                ring_cmds.push(cmd);
            }
        }
        for cmd in ring_cmds {
            tracing::info!(?cmd, "ring");
            match cmd {
                RingCommand::CycleStats => {
                    bump_stats_tier(&mut stats_verbosity, &mut stream);
                }
                RingCommand::Keyboard => ring_keyboard = !ring_keyboard,
                RingCommand::TogglePadMouse => {
                    if let Some(c) = stream.as_ref().and_then(|st| st.connector.as_ref()) {
                        toggle_pad_mouse(c, ring_opener);
                    }
                }
                RingCommand::ToggleStreamMute => {
                    if let Some(c) = stream.as_ref().and_then(|st| st.connector.as_ref()) {
                        let on = c.audio_mute() & punktfunk_core::client::AUDIO_MUTE_LOCAL != 0;
                        c.set_audio_muted(!on);
                    }
                }
                // The pad worker owns the wire index and the owed release, so this one is
                // the service's, not `ring_command`'s.
                RingCommand::TapButton(bit) => gamepad.tap_button(bit),
                other => {
                    if let Some(st) = stream.as_mut() {
                        ring_command(other, st, &mut window, &mouse, inhibit_shortcuts);
                    }
                }
            }
        }

        if let Some(st) = stream.as_mut() {
            if st
                .session_notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(ACCESS_NOTICE_S))
            {
                st.session_notice = None;
            }
        }

        if let Some(o) = overlay.as_mut() {
            let (pw, ph) = window.size_in_pixels();
            let (stats, hint) = match &stream {
                Some(st) if st.connector.is_some() => {
                    // No "click to capture" over a session with nothing to capture for.
                    let hint = match &st.capture {
                        Some(cap) if !cap.captured() && cap.can_capture() => {
                            Some(if gamepad.active().is_some() {
                                HINT_WITH_PAD
                            } else {
                                HINT_KEYBOARD
                            })
                        }
                        _ => None,
                    };
                    (
                        (stats_verbosity != StatsVerbosity::Off && !st.osd.is_empty())
                            .then_some(st.osd.as_slice()),
                        hint,
                    )
                }
                _ => (None, None),
            };
            // Access chip: a standing pill in the stats overlay family. A pill that never
            // goes away is chrome, so it rides the stats tier. `None` for a full-control
            // permanent session — what a host that never sent access looks like.
            let access_chip = match &stream {
                Some(st) if st.connector.is_some() && stats_verbosity != StatsVerbosity::Off => {
                    st.access.chip_text(Instant::now())
                }
                _ => None,
            };
            let session_notice = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .and_then(|st| st.session_notice.as_ref().map(|(n, _)| n.as_str()));
            let pad = gamepad.active();
            let pads = gamepad.pads();
            let resizing = stream
                .as_ref()
                .is_some_and(|st| st.connector.is_some() && st.resize_overlay.active());
            // Read live from the session's control rather than mirrored into StreamState:
            // the pump knows whether an uplink exists, and a mirrored copy would go stale
            // at session end.
            let mic_muted = stream.as_ref().is_some_and(|st| st.handle.mic.muted());
            // The badge clears itself once a local mute has been read; the mask's own clock
            // lives here because only the frame loop knows when it last moved.
            let audio_mute = stream
                .as_ref()
                .and_then(|st| st.connector.as_ref())
                .and_then(|c| {
                    let mask = c.audio_mute();
                    if mask != audio_mute_seen {
                        audio_mute_seen = mask;
                        audio_mute_at = Instant::now();
                    }
                    punktfunk_core::client::audio_mute_notice(mask, audio_mute_at.elapsed())
                });
            let ring_facts = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .map(|st| ring_facts(st, &opts, stats_verbosity, mic_muted, ring_opener));
            let ctx = FrameCtx {
                width: pw,
                height: ph,
                ten_bit: presenter.ten_bit(),
                // Re-read per frame: dragging to a second monitor with a different scale
                // updates this.
                scale: overlay_scale(window.display_scale(), osd_scale_pref),
                stats,
                hint,
                access: access_chip.as_deref(),
                notice: session_notice,
                mic_muted,
                audio_mute,
                resizing,
                pad: pad.as_ref().map(|p| p.name.as_str()),
                pad_pref: pad.as_ref().map(|p| p.pref),
                pads: &pads,
                ring: ring_facts.as_ref(),
            };
            match o.frame(&ctx) {
                Ok(f) => overlay_frame = f,
                Err(e) => {
                    if matches!(mode, ModeCtl::Browse(_)) {
                        return Err(e).context("console UI frame (required for --browse)");
                    }
                    tracing::warn!(error = %format!("{e:#}"),
                        "overlay frame failed — disabling the console UI");
                    overlay = None;
                    overlay_frame = None;
                }
            }
        }
        overlay_damage.rendered(overlay_frame.as_ref().map(|f| f.image));

        let mut presented_video = false;
        if let Some(st) = &mut stream {
            // Mastering metadata (0xCE) → the presentation engine, ahead of the frame
            // that needs it.
            if let Some(c) = &st.connector {
                while let Ok(m) = c.next_hdr_meta(Duration::ZERO) {
                    presenter.set_hdr_metadata(m);
                }
            }
            // Present-wait completions drive the latch clock, the glass gate, and the
            // host-facing grid — drained every pass (a 1 Hz batch would starve all three).
            if presenter.present_timing_active() {
                let samples = presenter.take_presented_samples();
                if !samples.is_empty() {
                    let clock_offset_ns = st
                        .clock_offset
                        .as_ref()
                        .map_or(0, |o| o.load(Ordering::Relaxed));
                    let period = st.clock.period_ns();
                    let mut stamps = Vec::with_capacity(samples.len());
                    for s in &samples {
                        let e2e = (s.displayed_ns as i128 + clock_offset_ns as i128
                            - s.pts_ns as i128)
                            .max(0) as u64;
                        // Hand the audio plane the figure it has to hit: the on-glass branch.
                        if e2e > 0 && e2e < 10_000_000_000 {
                            if let Some(c) = st.video_e2e.as_ref() {
                                c.store(e2e, Ordering::Relaxed);
                            }
                        }
                        if let Some(c) = &st.connector {
                            c.hud().note_displayed(
                                s.pts_ns,
                                s.decoded_ns,
                                s.submitted_ns,
                                s.displayed_ns,
                            );
                        }
                        // Latch miss: glass later than one panel period past submit plus
                        // the lead we already applied. Store evictions happen whenever
                        // the stream out-runs the panel and say nothing about the latch.
                        if st.store.is_smoothing()
                            && s.displayed_ns.saturating_sub(s.submitted_ns) > period + st.margin_ns
                        {
                            st.win_misses += 1;
                        }
                        stamps.push(s.displayed_ns);
                    }
                    st.clock.note_batch(&stamps);
                    // VRR probe: healthy-window stamps only. Use the display mode's period
                    // (not the learned one — a slow stream makes the learner adopt our
                    // cadence as "the grid"). FIFO-family only: MAILBOX/IMMEDIATE never
                    // wait for vblank, so they would look like VRR. Else Unknown.
                    let healthy = st.last_forced == 0;
                    if presenter.vblank_locked() {
                        st.cadence.note(&stamps, st.mode_period_ns, healthy);
                    }
                    // Phase-locked capture, the presenter's half: publish the grid the
                    // local clock just learned, so the report and the scheduler cannot
                    // disagree.
                    if let Some(grid) = &st.latch_grid {
                        grid.period_ns
                            .store(st.clock.period_ns(), Ordering::Relaxed);
                        grid.anchor_ns
                            .store(st.clock.anchor_ns(), Ordering::Relaxed);
                    }
                }
            }

            // Intake into the intent store. PyroWave collapses smoothness to latency:
            // its plane-ring retirement assumes the newest-wins hand-off, and all-intra
            // frames make buffering moot.
            while let Ok(f) = st.frames.try_recv() {
                #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                if st.store.is_smoothing() && matches!(f.image, DecodedImage::PyroWave(_)) {
                    st.store.force_latency();
                    if !st.pyro_latency_forced {
                        st.pyro_latency_forced = true;
                        tracing::info!(
                            "PyroWave stream — smoothness buffering does not apply \
                             (latency pacing)"
                        );
                    }
                }
                // Intent after any PyroWave collapse above, so a wavelet stream folds
                // nothing into a loop it will never consult.
                let smoothing = st.store.is_smoothing();
                let due_ns = st
                    .pacer
                    .due_ns(smoothing, f.pts_ns, f.decoded_ns, st.source_interval_ns)
                    .unwrap_or(0);
                st.store.submit(Paced { frame: f, due_ns });
            }

            // One frame out: latency takes the newest whenever the glass gate allows;
            // smoothness serves the frame whose due time has come.
            let now_ns = session::now_ns();
            st.pacer.follow(st.cadence.verdict());
            let mut to_present = if st.store.is_smoothing() {
                if st.pacer.free_running() {
                    // Variable refresh, measured: the panel refreshes when we present, so
                    // there is no grid to aim at and the due time is the target.
                    st.store.take(|p| p.due_ns <= now_ns as i64)
                } else {
                    // The first latch still reachable from here, given the submit lead. A
                    // frame due before it cannot be shown sooner by waiting; one due after
                    // it would land a slot early (`next_slot_after` is monotone).
                    let slot = st
                        .clock
                        .next_slot_after(now_ns.saturating_add(st.margin_ns));
                    st.store.take(|p| p.due_ns < slot as i64)
                }
            } else {
                st.store.take(|_| true)
            };
            // FIFO glass budget: one undisplayed present in flight, so the swapchain's
            // own FIFO can never become a standing queue. Only FIFO modes queue and only
            // present timing can count; everywhere else this stays inert.
            if pacing_active && presenter.needs_glass_gate() && presenter.present_timing_active() {
                if let Some(f) = to_present.take() {
                    if st.gate.open(presenter.presents_outstanding(), now_ns) {
                        to_present = Some(f);
                    } else {
                        // Parked: a newest-wins store replaces it if a fresher frame
                        // lands; the waiter's wake (or the 100 ms stale force-open) retries.
                        st.store.put_back(f);
                    }
                }
            }
            if let Some(Paced { frame: f, .. }) = to_present {
                // Resize end: a frame at the steered target size means the new-mode
                // picture is here.
                let (fw, fh) = f.image.dimensions();
                st.resize_overlay.decoded(fw, fh);
                st.last_video = Some((fw, fh));
                let DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    image,
                } = f;
                let did_present = match image {
                    // PyroWave: already on the presenter's device and fence-complete — a
                    // present failure has no demote rung; only device loss ends the session.
                    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                    DecodedImage::PyroWave(f) => {
                        // Wavelet stream carries negotiated ColorInfo (no VUI): a PQ
                        // session presents through the HDR10 path like the H.26x codecs.
                        st.hdr = f.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::PyroWave(f),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.pyro_present_warned = false;
                                p
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                if !st.pyro_present_warned {
                                    st.pyro_present_warned = true;
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "pyrowave present failed — suppressing repeats until it recovers"
                                    );
                                }
                                false
                            }
                        }
                    }
                    DecodedImage::Cpu(c) => {
                        st.hdr = c.color.is_pq();
                        // Software lane uploads planes into the same planar CSC pass as
                        // hardware, so PQ is tone-mapped there too.
                        st.hdr_untonemapped = false;
                        // Last rung: a present failure has nothing left to demote to.
                        // Drop the frame and keep the session; only a lost device ends it.
                        match presenter.present(
                            &window,
                            FrameInput::Cpu(&c),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.cpu_present_warned = false;
                                p
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                if !st.cpu_present_warned {
                                    st.cpu_present_warned = true;
                                    tracing::warn!(
                                        error = %format!("{e:#}"),
                                        "software present failed — suppressing repeats until it recovers"
                                    );
                                }
                                false
                            }
                        }
                    }
                    // VAAPI output: dmabuf fds plus a plane layout. Import and failure-
                    // streak demotion are the same contract as the other hardware arms.
                    #[cfg(target_os = "linux")]
                    DecodedImage::NativeDmabuf(d)
                        if presenter.supports_dmabuf() && !st.dmabuf_demoted =>
                    {
                        st.hdr = d.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::Dmabuf(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            // Import/CSC failure is survivable — a streak means this box
                            // cannot do the hw path: demote the decoder to software. A lost
                            // device is not survivable and must not demote.
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "hardware present failed");
                                if st.hw_fails >= 3 && !st.dmabuf_demoted {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    #[cfg(target_os = "linux")]
                    DecodedImage::NativeDmabuf(_) => {
                        // No import extensions (or already demoted) — the pump rebuilds
                        // the decoder as software.
                        if !st.dmabuf_demoted {
                            st.dmabuf_demoted = true;
                            tracing::warn!(
                                "no dmabuf import support on this device — demoting the \
                                 decoder to software"
                            );
                            st.force_software.store(true, Ordering::Relaxed);
                        }
                        false
                    }
                    // D3D11VA: shared-texture import, same gate + failure-streak demotion
                    // as dmabuf.
                    #[cfg(windows)]
                    DecodedImage::D3d11(d) if presenter.supports_d3d11() && !st.dmabuf_demoted => {
                        st.hdr = d.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::D3d11(d),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "hardware present failed");
                                if st.hw_fails >= 3 && !st.dmabuf_demoted {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    #[cfg(windows)]
                    DecodedImage::D3d11(_) => {
                        // No import extensions (or already demoted) — the pump rebuilds
                        // the decoder as software.
                        if !st.dmabuf_demoted {
                            st.dmabuf_demoted = true;
                            tracing::warn!(
                                "no win32 external-memory import on this device — demoting \
                                 the decoder to software"
                            );
                            st.force_software.store(true, Ordering::Relaxed);
                        }
                        false
                    }
                    // Native Vulkan Video: decoded on the presenter's own device —
                    // present is views + CSC, no import step. Same failure-streak demotion.
                    // A drained/demoted frame drops through the arm below — its guard
                    // still returns the decoder's slot.
                    DecodedImage::NativeVk(v) if !st.dmabuf_demoted => {
                        st.hdr = v.color.is_pq();
                        st.hdr_untonemapped = false;
                        match presenter.present(
                            &window,
                            FrameInput::NativeVk(v),
                            overlay_frame.as_ref(),
                        ) {
                            Ok(p) => {
                                st.hw_fails = 0;
                                p
                            }
                            Err(e) => {
                                if device_lost(&e) {
                                    return Err(e).context("GPU device lost");
                                }
                                st.hw_fails += 1;
                                tracing::warn!(error = %format!("{e:#}"), fails = st.hw_fails,
                                    "native vulkan present failed");
                                if st.hw_fails >= 3 {
                                    st.dmabuf_demoted = true;
                                    tracing::warn!("demoting the decoder to software");
                                    st.force_software.store(true, Ordering::Relaxed);
                                }
                                false
                            }
                        }
                    }
                    DecodedImage::NativeVk(_) => false, // demoted — drain until rebuild
                };
                if did_present {
                    presented_video = true;
                    overlay_damage.video_presented(Instant::now());
                    let (import_us, submit_us) = presenter.last_timings();
                    st.win_import_us.push(import_us);
                    st.win_submit_us.push(submit_us);
                    let (fence_us, acquire_us, present_us) = presenter.last_waits();
                    st.win_fence_us.push(fence_us);
                    st.win_acquire_us.push(acquire_us);
                    st.win_present_us.push(present_us);
                    if opts.json_status && !st.ready_announced {
                        st.ready_announced = true;
                        println!("{{\"ready\":true}}");
                    }
                    if presenter.present_timing_active() {
                        // Hand the frame's stamps to the present-wait waiter — e2e/display
                        // samples arrive via `take_presented_samples` with a true on-glass stamp.
                        presenter.note_presented(pts_ns, decoded_ns);
                        st.gate.note_present(now_ns);
                        st.win_out_max = st.win_out_max.max(presenter.presents_outstanding());
                    } else {
                        let displayed_ns = session::now_ns();
                        let clock_offset_ns = st
                            .clock_offset
                            .as_ref()
                            .map_or(0, |o| o.load(Ordering::Relaxed));
                        let e2e = (displayed_ns as i128 + clock_offset_ns as i128 - pts_ns as i128)
                            .max(0) as u64;
                        // Same hand-off as the glass-stamped branch. Anchored on submit, so it
                        // understates the video leg by up to a refresh: inside the audio deadband.
                        if e2e > 0 && e2e < 10_000_000_000 {
                            if let Some(c) = st.video_e2e.as_ref() {
                                c.store(e2e, Ordering::Relaxed);
                            }
                        }
                        if let Some(c) = &st.connector {
                            c.hud().note_displayed(pts_ns, decoded_ns, 0, displayed_ns);
                        }
                        // No glass stamps: the submit instant anchors an approximate grid
                        // on the mode's refresh period, so smoothness still drains one
                        // frame per (approximate) slot.
                        st.clock.note_batch(&[displayed_ns]);
                    }
                }
            }

            // Close the overlay window once per second.
            if st.win_start.elapsed() >= Duration::from_secs(1) {
                let import = punktfunk_core::hud::Summary::of(&mut st.win_import_us);
                let submit = punktfunk_core::hud::Summary::of(&mut st.win_submit_us);
                let fence = punktfunk_core::hud::Summary::of(&mut st.win_fence_us);
                let acquire = punktfunk_core::hud::Summary::of(&mut st.win_acquire_us);
                let queue_present = punktfunk_core::hud::Summary::of(&mut st.win_present_us);
                // Drained once per window and shared by the HUD and the log line — a
                // second `take_counters` would read zeros.
                let (replaced, q_drop, q_dry) = st.store.take_counters();
                let (gated, forced) = st.gate.take_counters();
                st.last_forced = forced;
                let present = PresentCounters {
                    mode: presenter.present_mode_name(),
                    vrr: st.cadence.verdict(),
                    smoothing: st.store.is_smoothing(),
                    q_drop,
                    q_dry,
                    gated,
                    forced,
                };
                st.win_import_us.clear();
                st.win_submit_us.clear();
                st.win_fence_us.clear();
                st.win_acquire_us.clear();
                st.win_present_us.clear();
                let (pace_ms, latch_ms) =
                    close_window(st, &presenter, &present, replaced, stats_verbosity);
                st.win_start = Instant::now();
                // Adaptive slot margin: start at 0 — a fixed lead is display tax — and
                // widen one step per window whose measured latch misses demand it.
                // One-way per stream.
                if st.store.is_smoothing() && st.win_misses > 2 && st.margin_ns < MARGIN_MAX_NS {
                    st.margin_ns = (st.margin_ns + MARGIN_STEP_NS).min(MARGIN_MAX_NS);
                    tracing::info!(
                        margin_us = st.margin_ns / 1000,
                        misses = st.win_misses,
                        "smoothness slot margin widened (measured latch misses)"
                    );
                }
                // The 1 Hz presenter line: emitted when anything moved, or always under
                // PUNKTFUNK_PRESENT_DEBUG=1.
                if pacing_active && (present_debug || q_drop + q_dry + gated + forced > 0) {
                    let cadence_health = st.pacer.health();
                    tracing::info!(
                        smoothing = present.smoothing,
                        mode = present.mode,
                        vrr = present.vrr.label(),
                        replaced,
                        q_drop,
                        q_dry,
                        gated,
                        forced,
                        misses = st.win_misses,
                        out_max = st.win_out_max,
                        pace_ms,
                        latch_ms,
                        import_us = import.p50_us,
                        submit_us = submit.p50_us,
                        submit_max_us = submit.max_us,
                        fence_us = fence.p50_us,
                        fence_max_us = fence.max_us,
                        acquire_us = acquire.p50_us,
                        acquire_max_us = acquire.max_us,
                        present_us = queue_present.p50_us,
                        period_us = st.clock.period_ns() / 1000,
                        margin_us = st.margin_ns / 1000,
                        // Cadence loop's current hold and the jitter it is sized from,
                        // plus frames whose due time had already passed when they arrived.
                        // Cumulative/instantaneous, not window sums like the counters above.
                        cushion_us = cadence_health.cushion_ns / 1000,
                        jitter_us = cadence_health.jitter_ns / 1000,
                        late = cadence_health.late,
                        "presenter window"
                    );
                }
                st.win_misses = 0;
                st.win_out_max = 0;
            }
        }

        // Present the overlay alone when no video frame carried it: every pass across a
        // resize scrim (the host's rebuild gap), and once per change while browsing or
        // after a mid-stream picture has gone still. An idle console hands back the same
        // image, so browsing presents only what the overlay re-rendered.
        let resize_scrim = stream.as_ref().is_some_and(|s| s.resize_overlay.active());
        let browse_idle = matches!(mode, ModeCtl::Browse(_))
            && stream.as_ref().is_none_or(|s| s.connector.is_none());
        let still_picture = stream.as_ref().is_some_and(|s| s.last_video.is_some())
            && overlay_damage.take_due(Instant::now());
        let browse_changed = browse_idle && overlay_damage.take_dirty();
        if !presented_video && (resize_scrim || browse_changed || still_picture) {
            // The UI owns the screen: hand the swapchain back to SDR. A finished PQ stream
            // leaves HDR10 live, and UI presents carry no frame. Not applied to
            // `resize_scrim`: that gap is still an HDR session, and flipping would rebuild
            // the swapchain twice.
            if browse_idle {
                presenter.leave_hdr(&window)?;
            }
            presenter.present(&window, FrameInput::Redraw, overlay_frame.as_ref())?;
        }
    };

    // Every loop exit converges here, so gamepad teardown belongs here, not on the
    // individual breaks. `detach` only queues; the close (flush, GamepadRemove, rumble
    // stop) runs when the pump drains it. Breaking immediately after detach leaves
    // pads unflushed and, if rumbling, still buzzing.
    pump.shutdown();
    // Join the pump before the device-wide idle: its decode submissions would race
    // vkDeviceWaitIdle otherwise.
    if let Some(st) = stream.take() {
        st.shutdown();
    }
    // Overlay resources live on the presenter's device: quiesce the queue first, drop
    // the overlay, then the presenter tears down.
    presenter.wait_idle();
    drop(overlay);
    Ok(outcome)
}

/// An `SDL_DisplayMode` as the panel's real pixels — the `0 = native` stream mode.
///
/// SDL3 reports a display mode in screen coordinates and hands the ratio separately as
/// `pixel_density`. On X11 and Windows that ratio is 1.0, so this is a no-op. Under
/// Wayland fractional scaling, taking `m.w`/`m.h` raw negotiates the point size and
/// streams a blurry image. The density is the exact `pixels / points` ratio, so the
/// multiplication recovers the panel size to the pixel.
fn native_mode(w: i32, h: i32, pixel_density: f32, refresh_rate: f32) -> Mode {
    // A non-finite or non-positive density is SDL telling us nothing useful; 1×
    // preserves the reported size instead of collapsing the mode to zero.
    let density = if pixel_density.is_finite() && pixel_density > 0.0 {
        pixel_density
    } else {
        1.0
    };
    let px = |v: i32| (v.max(0) as f32 * density).round().max(0.0) as u32;
    Mode {
        width: px(w),
        height: px(h),
        refresh_hz: refresh_rate.round().max(0.0) as u32,
    }
}

/// The HDR volume of the display the window is on now, read per launch, so a console
/// moved to a TV asks for the TV's HDR. `None` for an SDR display, and off Windows.
fn window_display_hdr(window: &sdl3::video::Window) -> Option<HdrMeta> {
    #[cfg(windows)]
    return pf_client_core::video_d3d11::display_hdr_volume(crate::win32::window_monitor(window));
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// Replace the params' requested w/h with the window's physical pixel size —
/// even-floored (the host's `validate_dimensions` rejects odd) and clamped to a
/// sane minimum — keeping the resolved refresh. Fullscreen, the window is the display.
fn apply_match_window(
    params: &mut SessionParams,
    window: &sdl3::video::Window,
    render_scale: f64,
    max_dim: u32,
) {
    let (pw, ph) = window.size_in_pixels();
    // × the render scale (even + codec-clamped) so match-window supersamples like the
    // fixed-mode path; 1.0 leaves the window's native pixels.
    let (w, h) = punktfunk_core::render_scale::apply(pw, ph, render_scale, max_dim);
    params.mode.width = w;
    params.mode.height = h;
    tracing::info!(
        w,
        h,
        "match-window: requesting the scaled window pixel size"
    );
}

/// Follow the live mode slot (any accepted ack — follower, another trigger, or rollback).
fn hud_mode_tick(st: &mut StreamState, window: &mut sdl3::video::Window, title_base: &str) {
    let Some(c) = &st.connector else {
        return;
    };
    let m = c.mode();
    if st.shown_mode.is_some_and(|prev| prev != m) {
        st.mode_line = format!("{}×{}@{}", m.width, m.height, m.refresh_hz);
        tracing::info!(mode = %st.mode_line, "stream mode switched");
        let _ = window.set_title(&format!("{title_base} · {}", st.mode_line));
        // A switch is a full host-side rebuild: the interval the cushion is bounded by
        // can change, and the gap must re-anchor the cadence estimate rather than slew over.
        st.source_interval_ns = frame_interval_ns(m.refresh_hz, 0);
        st.pacer.reset();
    }
    st.shown_mode = Some(m);
}

/// Fire the debounced `Reconfigure` once ~400 ms pass with no further resize events.
/// Physical pixels, even-floored, clamped ≥ 320×200; ≥ 1 s between requests (the accept
/// ack round-trips in milliseconds, so the spacing also keeps ~one request outstanding);
/// each distinct size requested at most once (rejected sizes and host-side rollbacks).
fn resize_tick(
    st: &mut StreamState,
    window: &mut sdl3::video::Window,
    persist: &mut dyn FnMut(u32, u32),
    render_scale: f64,
    max_dim: u32,
) {
    let Some(c) = &st.connector else {
        return; // not connected yet — the pending stamp survives until we are
    };
    let m = c.mode();
    // × the render scale so a resize under Match-window targets the same supersampled
    // space the live mode is in. resize_decision re-normalizes idempotently.
    let (pw, ph) = window.size_in_pixels();
    let pixel_size = punktfunk_core::render_scale::apply(pw, ph, render_scale, max_dim);
    match resize_decision(
        Instant::now(),
        &mut st.resize_pending,
        st.resize_sent_at,
        st.resize_requested,
        (m.width, m.height),
        pixel_size,
    ) {
        ResizeAction::Wait => {}
        ResizeAction::Settled(target) => {
            // Persist the window's logical size for the next launch even when no request
            // goes out (e.g. resized back to the streamed size).
            let (lw, lh) = window.size();
            persist(lw, lh);
            let Some((w, h)) = target else { return };
            tracing::info!(w, h, "window resized — requesting mode switch");
            if c.request_mode(Mode {
                width: w,
                height: h,
                refresh_hz: m.refresh_hz,
            })
            .is_err()
            {
                tracing::warn!("mode-switch request dropped — control channel closed");
            }
            st.resize_requested = Some((w, h));
            st.resize_sent_at = Some(Instant::now());
            // Scrim + spinner until a frame at this size lands: the live drag stays
            // sharp; only the host's rebuild gap is covered.
            st.resize_overlay.steering(w, h, Instant::now());
        }
    }
}

/// What one [`resize_decision`] tick decided.
#[derive(Debug, PartialEq, Eq)]
enum ResizeAction {
    /// Nothing to do yet — the pending stamp is kept so a later tick retries.
    Wait,
    /// Debounce settled (caller persists the window size). `None` when the size needs no
    /// switch (equal to the streamed mode, or this exact size was already requested).
    Settled(Option<(u32, u32)>),
}

/// Debounce to resize-end, ≥ 1 s between requests, physical pixels even-floored and
/// clamped ≥ 320×200, skip when equal to the streamed mode, each distinct size at most
/// once (covers rejected sizes and host-side rollbacks).
fn resize_decision(
    now: Instant,
    pending: &mut Option<Instant>,
    sent_at: Option<Instant>,
    requested: Option<(u32, u32)>,
    current: (u32, u32),
    pixel_size: (u32, u32),
) -> ResizeAction {
    const DEBOUNCE: Duration = Duration::from_millis(400);
    const SPACING: Duration = Duration::from_secs(1);
    let Some(since) = *pending else {
        return ResizeAction::Wait;
    };
    if now.duration_since(since) < DEBOUNCE {
        return ResizeAction::Wait;
    }
    if sent_at.is_some_and(|at| now.duration_since(at) < SPACING) {
        return ResizeAction::Wait; // keep the pending stamp — a later tick retries
    }
    *pending = None;
    let target = ((pixel_size.0 & !1).max(320), (pixel_size.1 & !1).max(200));
    if current == target || requested == Some(target) {
        return ResizeAction::Settled(None);
    }
    ResizeAction::Settled(Some(target))
}

/// Overlay changes no present has carried to the glass yet. A still host desktop sends
/// no frames, so an opened ring or a new OSD line would stay invisible while the ring
/// holds the pad, and the menu would read as frozen.
#[derive(Default)]
struct OverlayDamage {
    image: Option<ash::vk::Image>,
    dirty: bool,
    video_at: Option<Instant>,
}

impl OverlayDamage {
    /// Video quiet this long hands the overlay its own presents: longer than a live
    /// stream's frame gap, short enough that the ring opens without a visible lag.
    const VIDEO_QUIET: Duration = Duration::from_millis(100);

    /// Once per pass, after `Overlay::frame`. A re-render lands in the other ring slot.
    fn rendered(&mut self, image: Option<ash::vk::Image>) {
        self.dirty |= image != self.image;
        self.image = image;
    }

    /// A video present composited the current overlay.
    fn video_presented(&mut self, now: Instant) {
        self.dirty = false;
        self.video_at = Some(now);
    }

    /// Browsing: the overlay changed since the last present.
    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The overlay changed over a picture that has gone still: present it once.
    fn take_due(&mut self, now: Instant) -> bool {
        let still = self
            .video_at
            .is_some_and(|t| now.duration_since(t) >= Self::VIDEO_QUIET);
        let due = self.dirty && still;
        self.dirty &= !due;
        due
    }
}

/// Resize-in-progress overlay. A mid-stream Match-window switch takes the host a rebuild
/// of virtual display + encoder; the first new-mode frame is an IDR the decoder re-inits
/// on. A scrim + spinner from request until the sharp new-resolution frame is on screen.
///
/// Driven by signals the presenter already has (no new protocol):
/// * START — [`resize_tick`] reports the size it just requested (`steering`).
/// * END — decode reports each frame's dimensions; when they reach the target
///   (`decoded`). The accepted-switch ack alone cannot end it: the ack round-trips in
///   milliseconds, ahead of the host's rebuild.
/// * TIMEOUT — a switch that never delivers the exact target (reject, cap, or
///   corrective ack); `tick` clears it after [`ResizeIndicator::TIMEOUT`].
///
/// Pure + clock-injected so the transition logic is unit-tested without a live session.
#[derive(Default)]
struct ResizeIndicator {
    /// Size the follower is steering toward — `Some` ⇔ show the scrim + spinner.
    target: Option<(u32, u32)>,
    /// When the current active span began — the timeout is measured from here.
    since: Option<Instant>,
}

impl ResizeIndicator {
    /// How long to keep the overlay up if the target frame never arrives.
    const TIMEOUT: Duration = Duration::from_millis(2500);

    fn active(&self) -> bool {
        self.target.is_some()
    }

    /// A switch to `w`×`h` was just requested. The timeout re-arms only when the target
    /// actually changes, so a drag through several sizes never trips it mid-gesture.
    fn steering(&mut self, w: u32, h: u32, now: Instant) {
        if self.target != Some((w, h)) {
            self.since = Some(now);
        }
        self.target = Some((w, h));
    }

    /// A decoded frame at `w`×`h`. Clears once it matches the steered target.
    fn decoded(&mut self, w: u32, h: u32) {
        if self.target == Some((w, h)) {
            self.target = None;
            self.since = None;
        }
    }

    /// Stop showing once [`TIMEOUT`](Self::TIMEOUT) has elapsed with no matching frame.
    fn tick(&mut self, now: Instant) {
        if self
            .since
            .is_some_and(|s| now.duration_since(s) >= Self::TIMEOUT)
        {
            self.target = None;
            self.since = None;
        }
    }
}

/// Apply capture to the window: pointer lock (relative mouse + hidden cursor) and a
/// keyboard grab so system chords reach the host while captured. SDL implements the
/// grab per platform (low-level hook / shortcuts-inhibit / XGrabKeyboard).
///
/// `inhibit` is [`Settings::inhibit_shortcuts`] — off leaves system chords with the
/// local shell. It only ever *removes* a grab: releasing input always hands chords back.
///
/// The `desktop` mouse model never locks: the pointer roams freely and the local cursor
/// is hidden over the window. The keyboard grab follows `inhibit` in both models —
/// desktop mode's unlocked pointer clicking another window is the way back.
/// `desktop` only matters while `on`.
///
/// `grants`: no pointer lock without POINTER, no keyboard grab without KEYBOARD.
/// On-sites pass `Capture::grants()`; off-sites pass `0`.
fn apply_capture(
    window: &mut sdl3::video::Window,
    mouse: &sdl3::mouse::MouseUtil,
    on: bool,
    desktop: bool,
    inhibit: bool,
    grants: u32,
) {
    use punktfunk_core::quic::{GRANT_KEYBOARD, GRANT_POINTER};
    let pointer = grants & GRANT_POINTER != 0;
    mouse.set_relative_mouse_mode(window, on && !desktop && pointer);
    // The local cursor hides only while the host's cursor stands in for it — without
    // POINTER no send lands, so hiding it would leave a keyboard-only session with no cursor.
    mouse.show_cursor(!(on && pointer));
    let grab = on && inhibit && grants & GRANT_KEYBOARD != 0;
    if !window.set_keyboard_grab(grab) && grab {
        // The one refusal SDL reports is a missing mechanism. Said once per process: the
        // answer never changes mid-session. Under gamescope that is expected (it has no
        // shortcuts of its own) so it stays at debug rather than warning once per stream.
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            let err = sdl3::get_error();
            if pf_client_core::gamescope::under_gamescope() {
                tracing::debug!(error = %err, "no keyboard grab under gamescope — chords already ours");
            } else {
                tracing::warn!(
                    error = %err,
                    "capture system shortcuts is on, but this compositor offers no way to grab \
                     the keyboard — system chords stay with the local shell"
                );
            }
        }
    }
}

/// One SDL mouse/touch event as the overlay wants it: swapchain pixels. `None` for
/// events the console cannot use.
///
/// Two conversions: mouse positions are window (logical) coordinates; fingers arrive
/// window-normalized (0..1). Mixing them puts every click off by the display scale.
/// Only DIRECT touch devices; an indirect trackpad already drives the mouse.
fn overlay_pointer(event: &Event, window: &sdl3::video::Window) -> Option<PointerInput> {
    // SDL's mouse id on mouse events synthesized from a touch (`SDL_TOUCH_MOUSEID`,
    // not re-exported by the sdl3 crate). The finger arms already forward the real
    // touch; the synthesized twin would land every tap twice.
    const TOUCH_MOUSEID: u32 = u32::MAX;
    let (pw, ph) = window.size_in_pixels();
    let (lw, lh) = window.size();
    // Logical → physical. A zero-sized window (minimized) would divide by zero.
    let sx = pw as f32 / lw.max(1) as f32;
    let sy = ph as f32 / lh.max(1) as f32;
    let button = |b: sdl3::mouse::MouseButton| match b {
        sdl3::mouse::MouseButton::Left => Some(PointerButton::Primary),
        sdl3::mouse::MouseButton::Right => Some(PointerButton::Secondary),
        _ => None,
    };
    Some(match event {
        Event::MouseMotion { which, x, y, .. } if *which != TOUCH_MOUSEID => PointerInput::Move {
            x: x * sx,
            y: y * sy,
        },
        Event::MouseButtonDown {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Down {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
            touch: false,
        },
        Event::MouseButtonUp {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Up {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
        },
        Event::MouseWheel {
            y,
            mouse_x,
            mouse_y,
            ..
        } => PointerInput::Wheel {
            x: mouse_x * sx,
            y: mouse_y * sy,
            dy: *y,
        },
        Event::FingerDown { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Down {
                x: x * pw as f32,
                y: y * ph as f32,
                button: PointerButton::Primary,
                touch: true,
            }
        }
        Event::FingerMotion { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Move {
                x: x * pw as f32,
                y: y * ph as f32,
            }
        }
        Event::FingerUp { touch_id, x, y, .. } if is_direct_touch(*touch_id) => PointerInput::Up {
            x: x * pw as f32,
            y: y * ph as f32,
            button: PointerButton::Primary,
        },
        // The pointer left the window mid-press: drop the press rather than let a release
        // that never comes leave a widget armed forever.
        Event::Window {
            win_event: WindowEvent::MouseLeave,
            ..
        } => PointerInput::Cancel,
        _ => return None,
    })
}

/// Inside a gamescope session? `overlay_focus` exists only on Linux; elsewhere, no.
fn in_gamescope() -> bool {
    #[cfg(target_os = "linux")]
    {
        pf_client_core::overlay_focus::gamescope_session()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Every touch device SDL sees, as `(id, kind, name)` — logged at connect. Under
/// gamescope this is the tell for whether Steam Input hands the touchscreen through.
fn touch_devices() -> Vec<(u64, &'static str, String)> {
    use sdl3::sys::stdinc::SDL_free;
    use sdl3::sys::touch::{
        SDL_GetTouchDeviceName, SDL_GetTouchDeviceType, SDL_GetTouchDevices, SDL_TouchDeviceType,
    };
    let kind = |t: SDL_TouchDeviceType| {
        if t == SDL_TouchDeviceType::DIRECT {
            "direct"
        } else if t == SDL_TouchDeviceType::INDIRECT_ABSOLUTE {
            "indirect-absolute"
        } else if t == SDL_TouchDeviceType::INDIRECT_RELATIVE {
            "indirect-relative"
        } else {
            "invalid"
        }
    };
    let mut n: std::ffi::c_int = 0;
    // SAFETY: SDL hands back an array it owns (freed here once read, and never touched
    // after) and names it owns (copied out before the free, never kept); a null array or
    // name is checked before use.
    unsafe {
        let ids = SDL_GetTouchDevices(&mut n);
        if ids.is_null() {
            return Vec::new();
        }
        let out = std::slice::from_raw_parts(ids, usize::try_from(n).unwrap_or(0))
            .iter()
            .map(|id| {
                let name = SDL_GetTouchDeviceName(*id);
                let name = if name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned()
                };
                (id.0, kind(SDL_GetTouchDeviceType(*id)), name)
            })
            .collect();
        SDL_free(ids.cast());
        out
    }
}

/// Is this SDL touch device a real touchscreen (DIRECT, window-relative)? Trackpads
/// report INDIRECT and drive the mouse — their finger events must not be forwarded
/// as touch passthrough. An unknown/invalid id reads as not-direct.
fn is_direct_touch(touch_id: u64) -> bool {
    use sdl3::sys::touch::{SDL_GetTouchDeviceType, SDL_TouchDeviceType, SDL_TouchID};
    // SAFETY: `SDL_GetTouchDeviceType` is a query on an id SDL issued; the TouchID
    // wrapper is a newtype over that id and does not take ownership of any handle.
    unsafe { SDL_GetTouchDeviceType(SDL_TouchID(touch_id)) == SDL_TouchDeviceType::DIRECT }
}

/// Route one SDL touchscreen finger into the session's [`Capture`]. SDL delivers
/// window-normalized `x`/`y` (0..1); the dispatcher hands physical window pixels
/// (trackpad ballistics) and the frame position under `fit` (pointer + passthrough).
/// Down/Move before the first decoded frame are dropped; an Up always dispatches so
/// a lift can release a held contact.
#[allow(clippy::too_many_arguments)]
fn dispatch_finger(
    phase: FingerPhase,
    window: &sdl3::video::Window,
    stream: &mut Option<StreamState>,
    finger_id: u64,
    x: f32,
    y: f32,
    timestamp: u64,
    fit: VideoFit,
) -> Vec<Act> {
    let Some(st) = stream.as_mut() else {
        return Vec::new();
    };
    let (pw, ph) = window.size_in_pixels();
    let (wx, wy) = (x * pw as f32, y * ph as f32);
    let abs = match st.last_video {
        Some(video) => finger_to_frame(fit, (pw, ph), video, x, y),
        None if phase == FingerPhase::Up => Abs {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        None => return Vec::new(),
    };
    let Some(cap) = st.capture.as_mut() else {
        return Vec::new();
    };
    // `wx`/`wy` are physical px; the gesture engine prices scroll in DIP.
    cap.set_touch_density(window.display_scale());
    cap.dispatch_finger(
        phase,
        finger_id,
        wx,
        wy,
        abs,
        timestamp as f64 / 1_000_000.0,
    )
}

/// Three-finger tap bumps the stats tier; a two-finger twist turns the quick-action ring.
fn on_touch_act(
    act: Act,
    verbosity: &mut StatsVerbosity,
    stream: &mut Option<StreamState>,
    overlay: &mut Option<Box<dyn Overlay>>,
) {
    let input = match act {
        Act::CycleStats => return bump_stats_tier(verbosity, stream),
        Act::Dial {
            progress,
            clockwise,
            x,
            y,
        } => RingInput::Turn {
            progress,
            clockwise,
            x,
            y,
        },
        Act::DialCommit => RingInput::Commit,
        Act::DialCancel => RingInput::Cancel,
        _ => return,
    };
    if let Some(o) = overlay.as_mut() {
        o.ring_input(input);
    }
}

/// A touchscreen finger while the ring is up goes to the ring as a pointer, not the
/// gesture engine. Returns true when the ring took it.
fn ring_finger(
    overlay: &mut Option<Box<dyn Overlay>>,
    window: &sdl3::video::Window,
    phase: FingerPhase,
    x: f32,
    y: f32,
) -> bool {
    let Some(o) = overlay.as_mut().filter(|o| o.ring_open()) else {
        return false;
    };
    let (pw, ph) = window.size_in_pixels();
    let (x, y) = (x * pw as f32, y * ph as f32);
    let input = match phase {
        FingerPhase::Down => PointerInput::Down {
            x,
            y,
            button: PointerButton::Primary,
            touch: true,
        },
        FingerPhase::Move => PointerInput::Move { x, y },
        FingerPhase::Up => PointerInput::Up {
            x,
            y,
            button: PointerButton::Primary,
        },
    };
    o.handle_pointer(input);
    true
}

/// The ring's session facts for this frame.
fn ring_facts(
    st: &StreamState,
    opts: &SessionOpts,
    stats: StatsVerbosity,
    mic_muted: bool,
    ring_opener: Option<u8>,
) -> RingFacts {
    let c = st.connector.as_ref().expect("filtered on connector");
    let m = c.mode();
    let target = pad_mouse_target(c, ring_opener);
    RingFacts {
        overlay_actions: opts.overlay_actions.clone(),
        touch_mode: st
            .capture
            .as_ref()
            .map_or(TouchMode::Trackpad, Capture::touch_mode)
            .as_name()
            .into(),
        invert_scroll: c.invert_scroll(),
        host_accepts_touch: c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH != 0,
        stats_tier: stats.label().into(),
        // The mic control answers `toggle` with `None` when no uplink runs; a session
        // with a mic is one whose settings asked for it.
        mic_available: st.params.mic_enabled,
        mic_muted,
        pad_mouse_target: target,
        pad_mouse_on: target != 0 && c.pad_mouse() & target == target,
        audio_mute: c.audio_mute(),
        pointer_granted: c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0,
        mode: (m.width, m.height, m.refresh_hz),
        native_mode: st.native_mode,
        addr: st.params.host.clone(),
        mgmt_port: c.mgmt_port(),
        fp_hex: st.fp_hex.clone(),
        host_name: opts.window_title.clone(),
    }
}

/// Pads the controller-mouse toggle acts on: the pad that opened the ring, else every live pad.
fn pad_mouse_target(c: &NativeClient, ring_opener: Option<u8>) -> u16 {
    ring_opener.map_or_else(
        || c.live_pads(),
        |pad| 1u16.checked_shl(pad.into()).unwrap_or(0),
    )
}

/// All target pads in controller mouse go back to passthrough; otherwise they all switch.
fn toggle_pad_mouse(c: &NativeClient, ring_opener: Option<u8>) {
    let target = pad_mouse_target(c, ring_opener);
    let on = c.pad_mouse();
    let next = if on & target == target {
        on & !target
    } else {
        on | target
    };
    if let Err(e) = c.set_pad_mouse(next) {
        tracing::warn!(error = %e, "ring: controller mouse");
    }
}

/// Run one ring command against the live session (stats tier, keyboard, system buttons and
/// controller mouse are the loop's own and are handled at the call site).
fn ring_command(
    cmd: RingCommand,
    st: &mut StreamState,
    window: &mut sdl3::video::Window,
    mouse: &sdl3::mouse::MouseUtil,
    inhibit_shortcuts: bool,
) {
    match cmd {
        RingCommand::EndStream => {
            st.request_quit();
            apply_capture(window, mouse, false, false, inhibit_shortcuts, 0);
        }
        RingCommand::DisconnectLinger => {
            // Leave without the quit close code: the host lingers for a reconnect.
            if let Some(cap) = &mut st.capture {
                cap.release(true);
            }
            st.handle.stop.store(true, Ordering::SeqCst);
            apply_capture(window, mouse, false, false, inhibit_shortcuts, 0);
        }
        RingCommand::ToggleMic => {
            st.handle.mic.toggle();
        }
        RingCommand::ToggleScrollInvert => {
            if let Some(c) = st
                .connector
                .as_ref()
                .filter(|c| c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0)
            {
                c.set_invert_scroll(!c.invert_scroll());
            }
        }
        RingCommand::CycleTouchMode => {
            let accepts_touch = st
                .connector
                .as_ref()
                .is_some_and(|c| c.host_caps2() & punktfunk_core::quic::HOST_CAP2_TOUCH != 0);
            if let Some(cap) = &mut st.capture {
                let next = match (cap.touch_mode(), accepts_touch) {
                    (TouchMode::Trackpad, _) => TouchMode::Pointer,
                    (TouchMode::Pointer, true) => TouchMode::Touch,
                    (TouchMode::Pointer, false) | (TouchMode::Touch, _) => TouchMode::Off,
                    (TouchMode::Off, _) => TouchMode::Trackpad,
                };
                cap.set_touch_mode(next);
            }
        }
        RingCommand::RequestMode {
            width,
            height,
            refresh_hz,
        } => {
            if let Some(c) = &st.connector {
                if let Err(e) = c.request_mode(punktfunk_core::config::Mode {
                    width,
                    height,
                    refresh_hz,
                }) {
                    tracing::warn!(error = %e, "ring: mode request");
                }
            }
        }
        RingCommand::Shortcut(keys) => {
            let vks: Vec<u8> = keys
                .iter()
                .filter_map(|k| pf_client_core::overlay_actions::key_vk(k))
                .collect();
            if vks.len() == keys.len() {
                if let Some(cap) = &mut st.capture {
                    cap.send_chord(&vks);
                }
            }
        }
        RingCommand::CycleStats
        | RingCommand::Keyboard
        | RingCommand::TapButton(_)
        | RingCommand::TogglePadMouse
        | RingCommand::ToggleStreamMute => {}
    }
}

/// Advance the stats-overlay tier and re-render the OSD immediately from the last
/// window (waiting for the next Stats event would lag the trigger by up to 1 s).
fn bump_stats_tier(verbosity: &mut StatsVerbosity, stream: &mut Option<StreamState>) {
    *verbosity = verbosity.next();
    if let Some(st) = stream {
        render_osd(st, *verbosity);
    }
}

/// Window-normalized position → frame pixel, with the frame size as the wire extent.
/// Same placement as the blit; a finger in a bar or on a cropped edge clamps onto the
/// visible frame.
fn finger_to_frame(fit: VideoFit, surface: (u32, u32), video: (u32, u32), x: f32, y: f32) -> Abs {
    let p = video_fit::place(fit, surface, video);
    let (fx, fy) = p.to_frame(
        f64::from(x) * f64::from(surface.0),
        f64::from(y) * f64::from(surface.1),
    );
    Abs {
        x: fx.round() as i32,
        y: fy.round() as i32,
        w: video.0,
        h: video.1,
    }
}

/// Inverse of [`finger_to_frame`] for the reappear warp: a host-frame pixel → logical
/// window coordinates (what `warp_mouse_in_window` takes). Coordinates outside the
/// visible frame clamp onto it.
fn content_to_window(
    fit: VideoFit,
    logical: (u32, u32),
    surface: (u32, u32),
    video: (u32, u32),
    x: i32,
    y: i32,
) -> (f32, f32) {
    let p = video_fit::place(fit, surface, video);
    let (px, py) = p.to_view(f64::from(x), f64::from(y));
    // Physical → logical (HiDPI): the window's logical size over its pixel size.
    let lx = px * f64::from(logical.0) / f64::from(surface.0.max(1));
    let ly = py * f64::from(logical.1) / f64::from(surface.1.max(1));
    (lx as f32, ly as f32)
}

/// The scale the backend applies to a custom cursor surface on its own. Wayland applies the
/// display scale: SDL hands the compositor the bitmap's pixel size as a surface-local viewport
/// destination. X11 and Windows show the surface at 1:1 physical pixels.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; dividing by 0
/// would blow the cursor up to nothing usable.
fn cursor_surface_scale(window: &sdl3::video::Window) -> f32 {
    if window.subsystem().current_video_driver() != "wayland" {
        return 1.0;
    }
    let scale = window.display_scale();
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Overlay chrome UI scale: SDL's window display scale times `PUNKTFUNK_OSD_SCALE`.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; a 0
/// multiplier would collapse the OSD to an invisible panel. The 4× ceiling keeps a
/// bogus scale from covering the stream.
fn overlay_scale(display_scale: f32, pref: f32) -> f32 {
    let base = if display_scale.is_finite() && display_scale > 0.0 {
        display_scale
    } else {
        1.0
    };
    let pref = if pref.is_finite() && pref > 0.0 {
        pref
    } else {
        1.0
    };
    (base * pref).clamp(0.5, 4.0)
}

/// How long an access toast holds the pill slot. The chip keeps the standing truth.
const ACCESS_NOTICE_S: u64 = 6;

/// How long the host's relative hint must hold before the mouse model follows it: longer
/// than the hide Windows does for a click, short against a game grabbing the pointer.
const HINT_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Mouse stillness before the local cursor follows host-driven motion: past a round trip, so
/// the host's echo of the user's own motion is never mistaken for someone else's.
const FOLLOW_HOST_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// How long motion events after a follow-warp are its echo, not the user.
const WARP_ECHO: std::time::Duration = std::time::Duration::from_millis(50);

/// Rounding between window and frame pixels; a smaller difference is the same spot.
const FOLLOW_SLACK_PX: i32 = 2;

/// Capture hints (`ui_stream` parity — the words the user reads while released).
const HINT_KEYBOARD: &str = "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+M mouse mode · Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats";
const HINT_WITH_PAD: &str = "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+D disconnects · hold L1 + R1 + Start + Select to leave";

/// The presenter's own counters for one window: what the `present:` line reports.
struct PresentCounters {
    mode: &'static str,
    vrr: Cadence,
    smoothing: bool,
    q_drop: u32,
    q_dry: u32,
    gated: u32,
    forced: u32,
}

/// Close the overlay window: the connector's snapshot plus what only the presenter knows,
/// then the OSD and the stdout lines. Returns the display split's p50s (ms) for the log.
fn close_window(
    st: &mut StreamState,
    presenter: &Presenter,
    present: &PresentCounters,
    replaced: u32,
    tier: StatsVerbosity,
) -> (f32, f32) {
    let Some(c) = st.connector.clone() else {
        return (0.0, 0.0);
    };
    // Replaced before display, or dropped from a full smoothing queue: decoded, never shown.
    c.hud()
        .note_skipped(replaced.saturating_add(present.q_drop), 0);
    let mut snap = c.hud_snapshot();
    snap.decoder = st.facts.decoder.to_string();
    snap.hdr = hdr_shown(st.hdr, presenter.hdr_active(), st.hdr_untonemapped);
    snap.asked_444 = st.params.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
    snap.preset = st.preset.clone();
    snap.on_glass = presenter.present_timing_active();
    let prev = std::mem::replace(&mut st.health_seen, st.facts.health);
    snap.extras = desktop_extras(present, st.facts.health, prev, session::codec_fallbacks());
    tracing::debug!(
        e2e_p50_us = snap.e2e.p50_us,
        e2e_p95_us = snap.e2e.p95_us,
        host_p50_us = snap.host.p50_us,
        host_p95_us = snap.host.p95_us,
        net_p50_us = snap.net.p50_us,
        net_p95_us = snap.net.p95_us,
        decode_p50_us = snap.decode.p50_us,
        decode_p95_us = snap.decode.p95_us,
        display_p50_us = snap.display.p50_us,
        display_p95_us = snap.display.p95_us,
        rtt_us = snap.rtt_us.unwrap_or(0),
        lost = snap.lost,
        received = snap.received,
        "stream window"
    );
    if tier != StatsVerbosity::Off {
        print_stats(&snap);
    }
    let split = (
        snap.pace.p50_us as f32 / 1000.0,
        snap.latch.p50_us as f32 / 1000.0,
    );
    st.last_snap = Some(snap);
    render_osd(st, tier);
    split
}

/// Re-render the OSD from the last closed window at `tier`.
fn render_osd(st: &mut StreamState, tier: StatsVerbosity) {
    st.osd = match &st.last_snap {
        Some(s) => hud::format(s, tier, st.params.advanced_stats),
        None => Vec::new(),
    };
}

/// The stdout machine interface: the Advanced Detailed text for a person reading a log, and
/// the snapshot for a program. Both are additive; readers skip what they do not know.
fn print_stats(snap: &StatsSnapshot) {
    use std::io::Write as _;
    let text = hud::join(&hud::format(snap, StatsVerbosity::Detailed, true), " | ");
    let json = serde_json::to_string(snap).unwrap_or_default();
    // Not `println!`: it panics on EPIPE, and the reader (the shell) can exit mid-stream.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "stats: {text}");
    let _ = writeln!(out, "stats-json: {json}");
}

/// How the stream reaches the screen. No present arm sets `untonemapped` today; the tag
/// stays so a lane that bypasses CSC can say so rather than claim a tone-map.
fn hdr_shown(stream_hdr: bool, display_hdr: bool, untonemapped: bool) -> hud::Hdr {
    match (stream_hdr, display_hdr, untonemapped) {
        (false, ..) => hud::Hdr::Sdr,
        (true, true, _) => hud::Hdr::Hdr,
        (true, false, true) => hud::Hdr::Untonemapped,
        (true, false, false) => hud::Hdr::ToneMapped,
    }
}

/// Lines only the desktop measures: the live present path, decode integrity over this window
/// (`health` against `prev`), and this process's codec fallbacks.
fn desktop_extras(
    p: &PresentCounters,
    health: Option<DecodeHealth>,
    prev: Option<DecodeHealth>,
    codec_fallbacks: u64,
) -> Vec<hud::Extra> {
    let mut out = Vec::new();
    if !p.mode.is_empty() {
        let mut t = format!("present: {}", p.mode);
        // Only once measured: an unproven "vrr no" would be a claim, not a reading.
        if p.vrr != Cadence::Unknown {
            t.push_str(&format!(" · vrr {}", p.vrr.label()));
        }
        if p.smoothing {
            t.push_str(" · smoothing");
        }
        for (name, n) in [
            ("qdrop", p.q_drop),
            ("qdry", p.q_dry),
            ("gated", p.gated),
            ("forced", p.forced),
        ] {
            if n > 0 {
                t.push_str(&format!(" · {name} {n}"));
            }
        }
        out.push(hud::Extra {
            text: t,
            tier: StatsVerbosity::Detailed,
            advanced_only: false,
            role: hud::Role::Muted,
        });
    }
    // A lane that cannot see damage says nothing; one that looked and saw none also says
    // nothing; one with half its detectors says so every window.
    if let Some(h) = health {
        let base = prev.unwrap_or_default();
        let damaged = h.damaged.saturating_sub(base.damaged);
        let refused = h.refused.saturating_sub(base.refused);
        let failed = h.failed.saturating_sub(base.failed);
        let mut parts = Vec::new();
        if damaged > 0 {
            parts.push(format!("damaged {damaged}"));
        }
        if refused > 0 {
            parts.push(format!("refused {refused}"));
        }
        if failed > 0 {
            parts.push(format!("driver-failed {failed}"));
        }
        if h.run > 0 {
            parts.push(format!("run {}", h.run));
        }
        // Session-cumulative: a 1 Hz sample of `run` misses the worst moment.
        if h.worst_run > h.run {
            parts.push(format!("worst run {}", h.worst_run));
        }
        if !h.status_queries {
            parts.push("no driver status".into());
        }
        if !parts.is_empty() {
            let hurt = damaged + refused + failed > 0 || h.run > 0;
            out.push(hud::Extra {
                text: format!("integrity: {}", parts.join(" · ")),
                tier: StatsVerbosity::Detailed,
                advanced_only: true,
                role: if hurt {
                    hud::Role::Warn
                } else {
                    hud::Role::Muted
                },
            });
        }
    }
    if codec_fallbacks > 0 {
        out.push(hud::Extra::detail(format!(
            "codec_fallbacks {codec_fallbacks}"
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_capture_masks_pads_until_capture_returns() {
        assert!(ui_wants_pad_mask(false, false, Some(false)));
        assert!(!ui_wants_pad_mask(false, false, Some(true)));
        assert!(!ui_wants_pad_mask(false, false, None));
        assert!(ui_wants_pad_mask(true, false, Some(true)));
        assert!(ui_wants_pad_mask(false, true, Some(true)));
    }

    #[test]
    fn overlay_damage_presents_a_changed_overlay_only_over_a_still_picture() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let quiet = OverlayDamage::VIDEO_QUIET;
        let t0 = Instant::now();
        let mut d = OverlayDamage::default();
        // No video yet: nothing to present over.
        d.rendered(Some(a));
        assert!(!d.take_due(t0 + quiet));
        // Live video carries the change.
        d.video_presented(t0);
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet / 2));
        // The picture went still: the change presents once.
        assert!(d.take_due(t0 + quiet));
        assert!(!d.take_due(t0 + quiet * 2));
        // An unchanged overlay does not; closing it does.
        d.rendered(Some(b));
        assert!(!d.take_due(t0 + quiet * 2));
        d.rendered(None);
        assert!(d.take_due(t0 + quiet * 2));
    }

    /// Browsing presents each overlay change once; an idle console hands back the same image.
    #[test]
    fn overlay_damage_browse_presents_only_a_changed_overlay() {
        use ash::vk::Handle as _;
        let (a, b) = (ash::vk::Image::from_raw(1), ash::vk::Image::from_raw(2));
        let mut d = OverlayDamage::default();
        d.rendered(Some(a));
        assert!(d.take_dirty());
        d.rendered(Some(a));
        assert!(!d.take_dirty());
        d.rendered(Some(b));
        assert!(d.take_dirty());
    }

    /// KDE fractional scaling advertises points; "Native" must recover the panel pixels.
    #[test]
    fn native_is_the_panels_pixels_under_fractional_wayland_scaling() {
        let density = 2560.0 / 1707.0;
        let m = native_mode(1707, 1067, density, 165.0);
        assert_eq!((m.width, m.height, m.refresh_hz), (2560, 1600, 165));
        // Survives the even-floor `validate_dimensions` forces (1707×1067 lost its odd
        // pixel and became 1706×1066).
        assert_eq!(
            punktfunk_core::render_scale::apply(m.width, m.height, 1.0, 8192),
            (2560, 1600)
        );
        assert_eq!(
            punktfunk_core::render_scale::apply(1707, 1067, 1.0, 8192),
            (1706, 1066),
            "the pre-fix mode, kept here so the regression is legible"
        );
    }

    #[test]
    fn native_is_unchanged_where_the_density_is_one() {
        // X11, Windows, and Wayland at 100 % all report 1.0 — density 1.0 is a no-op.
        let m = native_mode(2560, 1600, 1.0, 165.0);
        assert_eq!((m.width, m.height, m.refresh_hz), (2560, 1600, 165));
        // Integer scaling (a 200 % 4K panel reported as 1920×1080 points) doubles cleanly.
        let m = native_mode(1920, 1080, 2.0, 60.0);
        assert_eq!((m.width, m.height), (3840, 2160));
    }

    #[test]
    fn a_nonsense_density_falls_back_to_one_rather_than_zeroing_the_mode() {
        // SDL normalizes an unset density to 1.0, but this must not be the one place a
        // driver quirk can hand the host a 0×0 mode request.
        for bogus in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let m = native_mode(2560, 1600, bogus, 60.0);
            assert_eq!((m.width, m.height), (2560, 1600), "density {bogus}");
        }
        // A negative mode size is clamped, not wrapped into a huge u32.
        let m = native_mode(-1, -1, 1.5, 60.0);
        assert_eq!((m.width, m.height), (0, 0));
    }

    /// Cadence cushion is bounded by the source's frame interval, not the panel's.
    #[test]
    fn the_cadence_interval_comes_from_the_stream_mode_not_the_panel() {
        assert_eq!(frame_interval_ns(120, 60), 8_333_333);
        assert_eq!(frame_interval_ns(60, 165), 16_666_666);
        // A `0 = native` request is resolved by the host to the display this client
        // reported, so that display's rate is what it will produce.
        assert_eq!(frame_interval_ns(0, 165), 6_060_606);
        // Neither known: 60 Hz, never an unbounded ceiling.
        assert_eq!(frame_interval_ns(0, 0), 16_666_666);
    }

    #[test]
    fn overlay_scale_follows_dpi_and_survives_a_bogus_display() {
        assert_eq!(overlay_scale(1.0, 1.0), 1.0);
        assert_eq!(overlay_scale(1.5, 1.0), 1.5);
        assert_eq!(overlay_scale(2.0, 1.0), 2.0);
        // PUNKTFUNK_OSD_SCALE multiplies the display's own scale, it does not replace it.
        assert_eq!(overlay_scale(2.0, 1.25), 2.5);
        // SDL reports 0.0 when it cannot resolve the window's display — must not collapse
        // the panel to nothing.
        assert_eq!(overlay_scale(0.0, 1.0), 1.0);
        assert_eq!(overlay_scale(f32::NAN, 1.0), 1.0);
        assert_eq!(overlay_scale(-2.0, 1.0), 1.0);
        // A garbage preference degrades to "just the DPI", never to zero.
        assert_eq!(overlay_scale(1.5, 0.0), 1.5);
        assert_eq!(overlay_scale(1.5, f32::NAN), 1.5);
        // Clamped both ways so nothing can hide the OSD or bury the stream under it.
        assert_eq!(overlay_scale(1.0, 100.0), 4.0);
        assert_eq!(overlay_scale(1.0, 0.01), 0.5);
    }

    #[test]
    fn content_to_window_inverts_the_letterbox() {
        // 1920×1080 video letterboxed in a 1600×1200 (4:3) window at 2× HiDPI: scale =
        // 1600/1920, dh = 900, oy = 150 (physical).
        let logical = (800u32, 600u32);
        let surface = (1600u32, 1200u32);
        let video = (1920u32, 1080u32);
        let (wx, wy) = content_to_window(VideoFit::Fit, logical, surface, video, 960, 540);
        assert!((wx - 400.0).abs() < 1.0, "wx = {wx}");
        assert!((wy - 300.0).abs() < 1.0, "wy = {wy}");
        // Roundtrip: normalized window pos → the same host frame pixel.
        let (nx, ny) = (wx / logical.0 as f32, wy / logical.1 as f32);
        let abs = finger_to_frame(VideoFit::Fit, surface, video, nx, ny);
        assert_eq!((abs.w, abs.h), video);
        assert!((abs.x - 960).abs() <= 1, "x = {}", abs.x);
        assert!((abs.y - 540).abs() <= 1, "y = {}", abs.y);
        // Out-of-range host coords clamp onto the video, never the bars.
        let (_, wy_clamped) = content_to_window(VideoFit::Fit, logical, surface, video, 0, 10_000);
        assert!(wy_clamped <= 300.0 + 225.0 + 1.0, "wy = {wy_clamped}"); // ≤ bottom of content
    }

    #[test]
    fn crop_maps_the_window_onto_the_visible_frame() {
        // 1920×1080 cropped into a 3216×1440 phone-shaped window: sides full, the top
        // and bottom ~110 frame rows cut. The window's top edge is frame row ~110.
        let surface = (3216, 1440);
        let video = (1920, 1080);
        let top = finger_to_frame(VideoFit::Crop, surface, video, 0.0, 0.0);
        assert_eq!((top.x, top.y, top.w, top.h), (0, 110, 1920, 1080));
        let corner = finger_to_frame(VideoFit::Crop, surface, video, 1.0, 1.0);
        assert_eq!((corner.x, corner.y), (1920, 970));
        // Stretch reaches every frame edge from every window edge.
        let s = finger_to_frame(VideoFit::Stretch, surface, video, 1.0, 1.0);
        assert_eq!((s.x, s.y), (1920, 1080));
    }

    #[test]
    fn resize_decision_follows_the_d2_discipline() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut pending = None;
        assert_eq!(
            resize_decision(t0, &mut pending, None, None, (1280, 720), (1000, 600)),
            ResizeAction::Wait
        );

        // Still debouncing → wait, pending kept.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(399),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1000, 600)
            ),
            ResizeAction::Wait
        );
        assert!(pending.is_some(), "pending survives the wait");

        // Debounce settled → request the even-floored, clamped pixel size.
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1001, 601)
            ),
            ResizeAction::Settled(Some((1000, 600))),
            "odd pixels floor to even"
        );
        assert!(pending.is_none(), "pending consumed");

        // Spacing: a request went out < 1 s ago → wait without dropping the pending
        // stamp, so a later tick retries.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(900),
                &mut pending,
                Some(t0),
                Some((1000, 600)),
                (1280, 720),
                (800, 500)
            ),
            ResizeAction::Wait
        );
        assert!(pending.is_some());
        assert_eq!(
            resize_decision(
                t0 + ms(1000),
                &mut pending,
                Some(t0),
                Some((1000, 600)),
                (1280, 720),
                (800, 500)
            ),
            ResizeAction::Settled(Some((800, 500)))
        );

        // Equal to the streamed mode → settle (persist) but no request.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (1280, 720)
            ),
            ResizeAction::Settled(None)
        );

        // A size already requested once (rejected, or rolled back host-side) is never
        // re-asked — no request → rollback → request loop.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                Some((1000, 600)),
                (1280, 720),
                (1000, 600)
            ),
            ResizeAction::Settled(None)
        );

        // Tiny windows clamp to the host's floor.
        let mut pending = Some(t0);
        assert_eq!(
            resize_decision(
                t0 + ms(400),
                &mut pending,
                None,
                None,
                (1280, 720),
                (100, 80)
            ),
            ResizeAction::Settled(Some((320, 200)))
        );
    }

    #[test]
    fn resize_indicator_shows_until_the_target_frame_or_timeout() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        let mut ind = ResizeIndicator::default();
        assert!(!ind.active());

        ind.steering(1000, 600, t0);
        assert!(ind.active());

        // A stale old-mode frame still draining does not lift it.
        ind.decoded(1280, 720);
        assert!(ind.active(), "an off-target frame keeps the scrim up");

        ind.decoded(1000, 600);
        assert!(!ind.active(), "the target frame lifts the scrim");
        ind.tick(t0 + ms(10_000)); // a late tick after clearing is inert
        assert!(!ind.active());

        // A switch whose target frame never arrives (rejected / host-capped) times out.
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        ind.tick(t0 + ResizeIndicator::TIMEOUT - ms(1));
        assert!(ind.active(), "still within the timeout window");
        ind.tick(t0 + ResizeIndicator::TIMEOUT);
        assert!(!ind.active(), "timeout lifts a switch that never delivered");
    }

    #[test]
    fn resize_indicator_retargets_and_rearms_the_timeout_mid_drag() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;

        // A drag through sizes re-arms the timeout, so a slow gesture never trips it.
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        let near = t0 + ResizeIndicator::TIMEOUT - ms(1);
        ind.steering(1200, 700, near); // new target → timeout re-armed from `near`
        ind.tick(t0 + ResizeIndicator::TIMEOUT + ms(1)); // past A's window, within B's
        assert!(
            ind.active(),
            "retarget re-armed the timeout — no mid-drag flicker"
        );

        // Re-steering the same size does not re-arm (a repeated identical request cannot
        // hold the scrim open forever).
        let mut ind = ResizeIndicator::default();
        ind.steering(1000, 600, t0);
        ind.steering(1000, 600, t0 + ms(500)); // same target, later — `since` unchanged
        ind.tick(t0 + ResizeIndicator::TIMEOUT);
        assert!(
            !ind.active(),
            "an unchanged target keeps the original timeout"
        );
    }

    fn counters() -> PresentCounters {
        PresentCounters {
            mode: "fifo",
            vrr: Cadence::Unknown,
            smoothing: false,
            q_drop: 0,
            q_dry: 0,
            gated: 0,
            forced: 0,
        }
    }

    /// The present line names the live mode; counters show only when non-zero, and VRR only
    /// once measured.
    #[test]
    fn the_present_line_says_only_what_moved() {
        let quiet = desktop_extras(&counters(), None, None, 0);
        assert_eq!(quiet.len(), 1);
        assert_eq!(quiet[0].text, "present: fifo");
        assert!(
            !quiet[0].advanced_only,
            "Standard Detailed shows the present path too"
        );
        let busy = PresentCounters {
            vrr: Cadence::Variable,
            smoothing: true,
            q_drop: 2,
            q_dry: 1,
            gated: 7,
            forced: 1,
            ..counters()
        };
        assert_eq!(
            desktop_extras(&busy, None, None, 0)[0].text,
            "present: fifo · vrr yes · smoothing · qdrop 2 · qdry 1 · gated 7 · forced 1"
        );
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        assert_eq!(
            desktop_extras(&no_mode, None, None, 2)[0].text,
            "codec_fallbacks 2"
        );
    }

    /// Integrity tells three quiet states apart: a lane that cannot see damage, one that
    /// looked and saw none, and one with only half its detectors.
    #[test]
    fn the_integrity_line_distinguishes_clean_from_unmeasurable() {
        let no_mode = PresentCounters {
            mode: "",
            ..counters()
        };
        let line = |now: DecodeHealth, prev: Option<DecodeHealth>| {
            desktop_extras(&no_mode, Some(now), prev, 0)
                .into_iter()
                .map(|e| e.text)
                .next()
        };
        assert!(desktop_extras(&no_mode, None, None, 0).is_empty());
        let clean = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        assert_eq!(line(clean, None), None);
        let radv = DecodeHealth {
            status_queries: false,
            ..clean
        };
        assert_eq!(
            line(radv, None).as_deref(),
            Some("integrity: no driver status")
        );
        let damaged = DecodeHealth {
            damaged: 4,
            failed: 2,
            run: 3,
            worst_run: 3,
            ..clean
        };
        assert_eq!(
            line(damaged, None).as_deref(),
            Some("integrity: damaged 4 · driver-failed 2 · run 3")
        );
        let recovered_hard = DecodeHealth {
            damaged: 4,
            worst_run: 40,
            ..clean
        };
        assert_eq!(
            line(recovered_hard, None).as_deref(),
            Some("integrity: damaged 4 · worst run 40")
        );
        let refusing = DecodeHealth {
            refused: 60,
            run: 60,
            worst_run: 60,
            ..clean
        };
        assert_eq!(
            line(refusing, None).as_deref(),
            Some("integrity: refused 60 · run 60")
        );
        // Windowed against the last window's cumulative counters.
        let later = DecodeHealth {
            damaged: 6,
            ..clean
        };
        let before = DecodeHealth {
            damaged: 4,
            ..clean
        };
        assert_eq!(
            line(later, Some(before)).as_deref(),
            Some("integrity: damaged 2")
        );
        let e = desktop_extras(&no_mode, Some(damaged), None, 0);
        assert!(e[0].advanced_only && e[0].role == hud::Role::Warn);
    }

    #[test]
    fn hdr_tag_follows_the_swapchain() {
        assert_eq!(hdr_shown(false, true, false), hud::Hdr::Sdr);
        assert_eq!(hdr_shown(true, true, false), hud::Hdr::Hdr);
        assert_eq!(hdr_shown(true, false, false), hud::Hdr::ToneMapped);
        assert_eq!(hdr_shown(true, false, true), hud::Hdr::Untonemapped);
    }

    fn frame_at(fit: VideoFit, surface: (u32, u32), x: f32, y: f32) -> (i32, i32, u32, u32) {
        let a = finger_to_frame(fit, surface, (1920, 1080), x, y);
        (a.x, a.y, a.w, a.h)
    }

    #[test]
    fn finger_maps_across_a_perfectly_filled_surface() {
        // Video exactly fills the window: normalized finger maps straight through.
        let s = (1920, 1080);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.0, 0.0), (0, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
    }

    #[test]
    fn finger_rebases_onto_the_letterboxed_frame() {
        // 16:9 video in 16:10 glass (1280×800) letterboxes: the picture is 1280×720,
        // centered with 40px bars. A finger in the top bar clamps to the frame's top edge.
        let s = (1280, 800);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
        // y=0.01 → window pixel 8, above the 40px bar → clamps to frame top (0).
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.01), (960, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
    }
}
