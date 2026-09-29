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
//! Each pass runs the same phases in order: SDL events and the input ticks (`events.rs`),
//! browse actions and the session drain (`stream.rs`), the overlay (`shell.rs`), then the
//! video present and its 1 Hz window (`pace.rs`). [`Shell`] holds what setup built,
//! [`StreamState`] one stream's life.
//!
//! In-stream chords share Ctrl+Alt+Shift: Q release/engage, M mouse model, D
//! disconnect, S stats tier, O quick-action ring, V microphone mute.

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
use crate::vk::{FrameInput, Presented, Presenter};
use anyhow::{Context as _, Result};
use pf_client_core::gamepad::{GamepadPump, GamepadService, MenuEvent, SelectChord};
use pf_client_core::orchestrate::{emit, SessionLine};
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
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod events;
mod pace;
mod shell;
mod stream;

use pace::{OverlayDamage, PresentHealth, PresentWindow};
use stream::ResizeIndicator;

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
    /// is the way back. See [`apply_capture`](shell::apply_capture).
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
    /// Browse mode: return, as Quit does, once no controller has been attached for
    /// [`NO_PAD_GRACE`] while no stream is up. Set by a desktop shell that opened the
    /// console because a controller connected.
    pub until_no_pads: bool,
}

/// How long [`SessionOpts::until_no_pads`] waits: a Bluetooth reconnect or Steam Input
/// re-enumerating a pad is not a player putting the controller down.
pub const NO_PAD_GRACE: Duration = Duration::from_secs(3);

/// [`SessionOpts::until_no_pads`]' clock: armed once a controller was seen, running while
/// none is attached.
#[derive(Default)]
struct PadAbsence {
    seen: bool,
    since: Option<Instant>,
}

impl PadAbsence {
    /// `pads` is `None` while a stream is up, which holds the clock. `true` once no
    /// controller has been attached for [`NO_PAD_GRACE`].
    fn tick(&mut self, pads: Option<usize>, now: Instant) -> bool {
        match pads {
            Some(0) if self.seen => {
                now.duration_since(*self.since.get_or_insert(now)) >= NO_PAD_GRACE
            }
            Some(0) => false,
            Some(_) => {
                self.seen = true;
                self.since = None;
                false
            }
            None => {
                self.since = None;
                false
            }
        }
    }
}

#[cfg(test)]
mod pad_absence_tests {
    use super::{PadAbsence, NO_PAD_GRACE};
    use std::time::{Duration, Instant};

    #[test]
    fn leaves_after_the_grace_never_mid_stream() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut clock = PadAbsence::default();
        // No controller yet: the console was opened some other way, or SDL is still counting.
        assert!(!clock.tick(Some(0), at(0)));
        assert!(!clock.tick(Some(0), at(10_000)));
        assert!(!clock.tick(Some(1), at(10_000)));
        assert!(!clock.tick(Some(0), at(11_000)));
        // A pad back inside the grace resets it.
        assert!(!clock.tick(Some(1), at(12_000)));
        assert!(!clock.tick(Some(0), at(13_000)));
        // A stream holds the clock; the grace starts again once it ends.
        assert!(!clock.tick(None, at(20_000)));
        assert!(!clock.tick(Some(0), at(21_000)));
        let limit = 21_000 + NO_PAD_GRACE.as_millis() as u64;
        assert!(!clock.tick(Some(0), at(limit - 1)));
        assert!(clock.tick(Some(0), at(limit)));
    }
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

/// What setup builds and every pass of the loop reads, across streams. Field order is
/// drop order: the overlay before the presenter it renders on, the presenter before the
/// window, the SDL context last.
struct Shell {
    overlay_damage: OverlayDamage,
    /// Ring Keyboard slot: hold text input on, which summons Steam's OSK under gamescope.
    ring_keyboard: bool,
    /// SDL text input tracks overlay editing (IME / Steam OSK). Toggled edge-wise —
    /// start/stop are not free on Wayland.
    text_input_on: bool,
    overlay_frame: Option<OverlayFrame>,
    stats_verbosity: StatsVerbosity,
    fullscreen: bool,
    mouse: sdl3::mouse::MouseUtil,
    event_pump: sdl3::EventPump,
    /// Native display mode — the `0 = native` fallback for the requested stream mode.
    native: Mode,
    /// Window focus and the gamescope overlay OR into one mask pushed on an edge.
    /// Kept as separate inputs: either would otherwise clear the other's mask.
    focus_lost: bool,
    mask_applied: bool,
    /// Gaming Mode's Steam menu / QAM drive the same physical pad we forward, and
    /// gamescope never takes our X focus away, so SDL's background-input gate cannot
    /// fire there. `None` everywhere else, where window focus is the signal.
    #[cfg(target_os = "linux")]
    overlay_focus: Option<pf_client_core::overlay_focus::OverlayFocus>,
    menu_rx: async_channel::Receiver<MenuEvent>,
    disconnect_rx: async_channel::Receiver<()>,
    /// Last audio-mute mask drawn and when it changed: a local mute's badge is timed off it.
    audio_mute_at: Instant,
    audio_mute_seen: u8,
    /// The pad whose Select+A opened the ring; `None` for a keyboard, touch or closed ring.
    ring_opener: Option<u8>,
    /// Ring pad ownership, edge-tracked: open masks the pads (a held trigger is released
    /// on the host) and polls them into menu events; close re-adopts them.
    ring_was_open: bool,
    chord_rx: async_channel::Receiver<(u8, SelectChord)>,
    escape_rx: async_channel::Receiver<()>,
    pump: GamepadPump,
    gamepad: GamepadService,
    overlay: Option<Box<dyn Overlay>>,
    /// `PUNKTFUNK_OSD_SCALE` on top of the display DPI: a preference, read once.
    osd_scale_pref: f32,
    /// `false` under `PUNKTFUNK_PRESENTER=arrival`: no glass gate, no presenter window line.
    pacing_active: bool,
    present_priority: PresentPriority,
    presenter: Presenter,
    scroll_routing: crate::scroll_routing::ScrollRouting,
    window: sdl3::video::Window,
    sdl_events: sdl3::EventSubsystem,
    sdl_video: sdl3::VideoSubsystem,
    _sdl: sdl3::Sdl,
    opts: SessionOpts,
    /// Browse mode: the console idles between streams.
    browse: bool,
    pad_absence: PadAbsence,
}

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
    /// This second of presents: timings, latch misses, glass steps, busy retries.
    win: PresentWindow,
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
    /// Hand-over to latch, learned from this stream's misses and published to the
    /// host-facing `latch_grid`. Latency intent on a stream at panel rate only.
    need: punktfunk_core::phase::LatchNeed,
    /// What the held frame waits on. The fence paces the loop itself (the presenter waits
    /// it for a millisecond per pass), so the pass turns straight around and drains the
    /// channel first: a newer frame replaces the held one instead of queuing behind it.
    busy_on: crate::vk::BusyOn,
    last_displayed_ns: u64,
    /// Smoothing: the latch slot the last vended frame was aimed at. One present per
    /// slot; a second frame due before the same slot waits for the next.
    last_slot_ns: u64,
    /// The presenter handed the frame back (no swapchain image yet): wake in 1 ms.
    busy_retry: bool,
    /// One-shot log latch: smoothness was requested but PyroWave collapsed the store
    /// to latency (plane-ring retirement assumes newest-wins).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pyro_latency_forced: bool,
    /// Present-failure streaks and the once-per-session demote to software.
    health: PresentHealth,
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
    /// Scrim + spinner. Armed by [`resize_tick`](stream::resize_tick) when it requests a
    /// switch; cleared when a decoded frame reaches the target (or on timeout).
    resize_overlay: ResizeIndicator,
    /// Last presented frame's video dimensions. Touch passthrough maps a finger into
    /// this letterboxed rect; `None` until the first frame, and touches before then drop.
    last_video: Option<(u32, u32)>,
    /// Created with the connector; inert when the host did not negotiate the channel.
    cursor_chan: Option<crate::cursor::CursorChannel>,
    /// Auto-flip fires on changes only, so it never fights a user who chorded away.
    last_hint: Option<bool>,
    /// When `last_hint` last changed; the flip waits out
    /// [`HINT_SETTLE`](events::HINT_SETTLE) from here.
    hint_since: std::time::Instant,
    /// When the user last moved the mouse; the local cursor follows host-driven motion only
    /// after [`FOLLOW_HOST_AFTER`](events::FOLLOW_HOST_AFTER) of stillness.
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
    /// Transient access toast and when it went up — cleared after
    /// [`ACCESS_NOTICE_S`](shell::ACCESS_NOTICE_S). An access change outranks "click to
    /// capture" for a few seconds.
    session_notice: Option<(String, Instant)>,
    /// The ring's End game in flight: the title, and the host's answer once it lands.
    ending_game: Option<(
        String,
        std::sync::mpsc::Receiver<pf_client_core::library::GameEnd>,
    )>,
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

/// The live stream's capture, once connected.
fn capture_mut(stream: &mut Option<StreamState>) -> Option<&mut Capture> {
    stream.as_mut().and_then(|s| s.capture.as_mut())
}

fn run_inner(opts: SessionOpts, mut mode: ModeCtl) -> Result<Outcome> {
    let mut sh = Shell::open(opts, matches!(mode, ModeCtl::Browse(_)))?;
    let mut stream: Option<StreamState> = match &mut mode {
        ModeCtl::Single(build) => {
            let force_software = Arc::new(AtomicBool::new(false));
            let params = build(
                &sh.gamepad,
                sh.native,
                window_display_hdr(&sh.window),
                force_software.clone(),
                sh.presenter.vulkan_decode(),
            );
            Some(sh.start_stream(params, force_software))
        }
        ModeCtl::Browse(_) => None,
    };

    let outcome = 'main: loop {
        // Block in SDL's wait: input/window events and decoded frames (FrameWake) all
        // land in this queue. The timeout only bounds stop-flag/pump-tick latency.
        // Smoothness tightens it to the next latch-slot deadline.
        let timeout = stream
            .as_ref()
            .map_or(Duration::from_millis(15), |st| st.wake_timeout());
        let first = sh.event_pump.wait_event_timeout(timeout);
        let mut queued: Vec<Event> = Vec::new();
        if let Some(e) = first {
            queued.push(e);
        }
        while let Some(e) = sh.event_pump.poll_event() {
            queued.push(e);
        }
        sh.scroll_routing.begin(
            stream.as_ref().and_then(|s| s.capture.as_ref()),
            sh.overlay.as_deref(),
        );
        for event in queued {
            if let ControlFlow::Break(outcome) = sh.on_event(&mut stream, event)? {
                break 'main outcome;
            }
        }
        // Native events forward only when capture owns the entire SDL batch.
        sh.scroll_routing
            .finish(capture_mut(&mut stream), sh.overlay.as_deref());
        let want_mask_ui = sh.ui_wants_mask(&stream);
        sh.pump.tick();
        // One coalesced MouseMove per iteration — pure motion must reach the host
        // without waiting for a click/key to flush it.
        if let Some(cap) = capture_mut(&mut stream) {
            cap.flush_motion();
        }
        if let Some(st) = stream.as_mut() {
            sh.cursor_tick(st);
        }
        sh.text_input_tick();
        sh.pad_owner_tick(&mut stream, want_mask_ui);
        if let ModeCtl::Browse(on_action) = &mut mode {
            if let ControlFlow::Break(outcome) = sh.browse_tick(&mut stream, on_action) {
                break 'main outcome;
            }
        }

        if let ControlFlow::Break(outcome) = sh.drain_session_events(&mut stream) {
            break 'main outcome;
        }

        if let Some(st) = stream.as_mut() {
            sh.stream_tick(st);
        }
        sh.ring_tick(&mut stream);
        sh.overlay_tick(&mut stream)?;

        let presented_video = match stream.as_mut() {
            Some(st) => sh.video_tick(st)?,
            None => false,
        };

        sh.present_overlay_alone(&stream, presented_video)?;
    };

    // Every loop exit converges here, so gamepad teardown belongs here, not on the
    // individual breaks. `detach` only queues; the close (flush, GamepadRemove, rumble
    // stop) runs when the pump drains it. Breaking immediately after detach leaves
    // pads unflushed and, if rumbling, still buzzing.
    sh.pump.shutdown();
    // Join the pump before the device-wide idle: its decode submissions would race
    // vkDeviceWaitIdle otherwise.
    if let Some(st) = stream.take() {
        st.shutdown(&mut sh.presenter);
    }
    // Overlay resources live on the presenter's device: quiesce the queue first, drop
    // the overlay, then the presenter tears down.
    sh.presenter.wait_idle();
    drop(sh.overlay.take());
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
