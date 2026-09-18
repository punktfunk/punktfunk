//! The console host proper: one render thread that owns the EGL context, the Skia
//! `DirectContext` and the [`Console`], paced by `eglSwapBuffers` while a surface is up
//! and parked while there is none. Everything else — Kotlin's input, surface lifecycle,
//! session edges — arrives through a command queue and is applied on that thread; what the
//! console raises (actions, haptic pulses, editing state, settings to persist) leaves
//! through an event queue a Kotlin poll thread blocks on.
//!
//! The model side needs none of this: `ConsoleShared`/`LibraryShared` are lock-guarded and
//! written straight from JNI, `ConsoleBus` is drained straight from JNI. Only the shell
//! itself is single-threaded, and this thread is that thread.

use super::egl::{EglContext, EglSurface, GlesVersion};
use super::gpu::Gpu;
use anyhow::{bail, Result};
use ndk::native_window::NativeWindow;
use pf_client_core::console::{OverlayAction, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuNav, MenuPulse, MenuSample, PadInfo};
use pf_console_ui::{
    Console, ConsoleEntry, ConsoleHandles, ConsoleOptions, InputSource, Insets, Key, SnapshotStore,
    Viewport,
};
use punktfunk_core::config::GamepadPref;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A session edge as Kotlin reports it — `SessionPhase` borrows its strings, so the queue
/// carries an owned twin.
pub(super) enum Phase {
    Connecting,
    Streaming,
    Failed(String),
    Ended(Option<String>),
    Reconnecting(String),
}

/// What Kotlin asks the render thread to do.
pub(super) enum Cmd {
    Menu(MenuEvent),
    /// The raw pad, whenever it changes; the thread feeds `MenuNav` with the LAST sample every
    /// frame (repeats need a clock) and once on arrival (a press must not wait for a frame).
    PadSample(MenuSample),
    Pointer(PointerInput),
    Key {
        key: Key,
        shift: bool,
        repeat: bool,
    },
    Text(String),
    Phase(Phase),
    Navigate(ConsoleEntry),
    SurfaceCreated(NativeWindow),
    SurfaceChanged,
    /// Acknowledged through `Shared::surface_gen` once the EGL surface is really gone —
    /// Kotlin's `surfaceDestroyed` must not return before that.
    SurfaceDestroyed,
    Viewport {
        insets: Insets,
        scale: Option<f64>,
    },
    Pads {
        label: Option<String>,
        pref: Option<GamepadPref>,
        pads: Vec<PadInfo>,
    },
    Quit,
}

/// What the render thread raises for Kotlin.
pub(super) enum HostEvent {
    Action(OverlayAction),
    Pulse(MenuPulse),
    Editing(bool),
    /// What the console's focus now reads as. Raised only when it changes; Kotlin speaks it
    /// through `announceForAccessibility`, which is a no-op with no screen reader running.
    Announce(String),
    /// The shell saved settings: here is the whole snapshot to persist.
    Settings(Box<pf_client_core::trust::Settings>),
    /// The GLES generation the context came up with — Kotlin logs it, nothing more.
    Gles(GlesVersion),
    /// The render thread died (EGL/Skia init failed). Kotlin falls back to its own console.
    Dead(String),
}

impl HostEvent {
    /// The JSON Kotlin parses. Hand-rolled for the small variants; the two model payloads
    /// ride serde.
    pub(super) fn to_json(&self) -> String {
        match self {
            HostEvent::Action(a) => format!(
                "{{\"action\":{}}}",
                serde_json::to_string(a).unwrap_or_else(|_| "null".into())
            ),
            HostEvent::Pulse(p) => format!(
                "{{\"pulse\":\"{}\"}}",
                match p {
                    MenuPulse::Move => "move",
                    MenuPulse::Confirm => "confirm",
                    MenuPulse::Boundary => "boundary",
                }
            ),
            HostEvent::Editing(e) => format!("{{\"editing\":{e}}}"),
            HostEvent::Announce(text) => format!(
                "{{\"announce\":{}}}",
                serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
            ),
            HostEvent::Settings(s) => format!(
                "{{\"settings\":{}}}",
                serde_json::to_string(s).unwrap_or_else(|_| "null".into())
            ),
            HostEvent::Gles(v) => format!(
                "{{\"gles\":{}}}",
                match v {
                    GlesVersion::Es2 => 2,
                    GlesVersion::Es3 => 3,
                }
            ),
            HostEvent::Dead(msg) => format!(
                "{{\"dead\":{}}}",
                serde_json::to_string(msg).unwrap_or_else(|_| "\"\"".into())
            ),
        }
    }
}

pub(super) struct Shared {
    inbox: Mutex<VecDeque<Cmd>>,
    inbox_cv: Condvar,
    events: Mutex<VecDeque<HostEvent>>,
    events_cv: Condvar,
    /// Bumped by the render thread each time it has torn a surface down; `SurfaceDestroyed`
    /// waits for the bump.
    surface_gen: Mutex<u64>,
    surface_cv: Condvar,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            inbox: Mutex::new(VecDeque::new()),
            inbox_cv: Condvar::new(),
            events: Mutex::new(VecDeque::new()),
            events_cv: Condvar::new(),
            surface_gen: Mutex::new(0),
            surface_cv: Condvar::new(),
        }
    }

    pub(super) fn send(&self, cmd: Cmd) {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(cmd);
        self.inbox_cv.notify_one();
    }

    fn emit(&self, ev: HostEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(ev);
        self.events_cv.notify_one();
    }

    /// Kotlin's poll: the next event, waiting up to `timeout` for one.
    pub(super) fn next_event(&self, timeout: Duration) -> Option<HostEvent> {
        let mut q = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.is_empty() {
            let (guard, _) = self
                .events_cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            q = guard;
        }
        q.pop_front()
    }

    /// Ask for the surface to go and wait (bounded) until it has.
    pub(super) fn destroy_surface_blocking(&self) {
        let before = *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.send(Cmd::SurfaceDestroyed);
        let g = self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bounded: a render thread that died mid-frame must not hang the UI thread forever —
        // by then the EGL surface is gone with it anyway.
        let _ = self
            .surface_cv
            .wait_timeout_while(g, Duration::from_secs(2), |g| *g == before)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    fn ack_surface_gone(&self) {
        *self
            .surface_gen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.surface_cv.notify_all();
    }
}

/// The host as the JNI layer holds it.
pub(super) struct ConsoleHost {
    pub(super) shared: Arc<Shared>,
    pub(super) handles: ConsoleHandles,
    pub(super) store: Arc<SnapshotStore>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ConsoleHost {
    /// Spawn the render owner, which builds Skia state on-thread and parks for a surface.
    /// Thread creation errors return to JNI; later console/build failures emit `Dead` events.
    pub(super) fn start(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
        store: Arc<SnapshotStore>,
    ) -> std::io::Result<ConsoleHost> {
        let shared = Arc::new(Shared::new());
        let handles = ConsoleHandles::new();
        let thread_shared = shared.clone();
        let thread_store = store.clone();
        let thread_handles = handles.clone();
        let thread = std::thread::Builder::new()
            .name("pf-console".into())
            .spawn(move || {
                boost_thread_priority();
                let run = || -> Result<()> {
                    let console = Console::new(opts, entry, &thread_handles)?;
                    render_loop(console, thread_shared.clone(), thread_store)
                };
                if let Err(e) = run() {
                    log::error!("console: render thread ended: {e:#}");
                    thread_shared.emit(HostEvent::Dead(format!("{e:#}")));
                }
            })?;
        Ok(ConsoleHost {
            shared,
            handles,
            store,
            thread: Some(thread),
        })
    }

    /// Signal the render loop and join it once. The final table-held `Arc` calls this from `Drop`.
    fn stop(&mut self) {
        self.shared.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ConsoleHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Best-effort: lift the console's render thread off the default nice band, the same way
/// `decode::setup::boost_thread_priority` lifts the decode thread. This thread IS the console's
/// frame loop — every menu press waits on it — and at default priority a TV box's scheduler is
/// free to park it on a little core behind whatever else the system is doing, which reads as a
/// UI that lags the remote. `-8` rather than the decode path's `-10`: a stream's frames are the
/// harder deadline, and the two should not compete when the console is up during a session.
///
/// Non-fatal if the platform refuses (the exact floor a foreground app may set is policy).
fn boost_thread_priority() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls; PRIO_PROCESS
    // with a TID targets that one task on Linux — the idiom `Process.setThreadPriority` uses.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8) != 0 {
            log::debug!(
                "console: setpriority(-8) failed (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// How often the render loop reports what a frame is costing it. Nothing in a bug report from a
/// TV said whether the console was drawing at 4K or at 60 Hz, so "it feels sluggish" could not be
/// triaged from a log bundle at all — this is that missing line. One line a minute is cheap
/// enough to leave on for everyone, and the answer is only useful from the box that is slow.
const FRAME_REPORT: Duration = Duration::from_secs(60);

/// No input for this long = the console is being looked at, not used — halve the redraw
/// rate (`IDLE_FRAME_STEP` slept between swaps). 60 s keeps every interaction and its
/// afterglow at full smoothness and only calms a genuinely parked screen.
const IDLE_AFTER: Duration = Duration::from_secs(60);
/// One extra ~vsync period per frame while idle: 60 Hz → ~30, 120 Hz → ~40.
const IDLE_FRAME_STEP: Duration = Duration::from_millis(16);

/// The render thread. Owns EGL + Skia + the console; runs until `Cmd::Quit`.
fn render_loop(mut console: Console, shared: Arc<Shared>, store: Arc<SnapshotStore>) -> Result<()> {
    let mut glass = Glass::new(EglContext::new()?);
    shared.emit(HostEvent::Gles(glass.egl.version));
    let mut ui = Ui::new();
    let mut published = Published::new(&console, &store);
    let mut menu_out: Vec<MenuEvent> = Vec::new();

    loop {
        // Take everything queued. With no surface up, block until something arrives.
        let cmds: Vec<Cmd> = {
            let mut q = shared
                .inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if glass.surface.is_none() && q.is_empty() {
                let (guard, _) = shared
                    .inbox_cv
                    .wait_timeout(q, Duration::from_millis(500))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                q = guard;
            }
            q.drain(..).collect()
        };
        let mut poll_now = false;
        for cmd in cmds {
            if !ui.apply(cmd, &mut console, &mut glass, &shared, &mut poll_now)? {
                glass.detach();
                return Ok(());
            }
        }

        // The pad, through the shared synthesizer: once per frame for repeats, plus once
        // right now if a sample just arrived.
        if poll_now || glass.surface.is_some() {
            menu_out.clear();
            ui.nav.poll(&ui.sample, Instant::now(), &mut menu_out);
            for ev in menu_out.drain(..) {
                if let Some(p) = console.menu(ev, InputSource::Pad) {
                    shared.emit(HostEvent::Pulse(p));
                }
            }
        }

        // Half-rate after 60 s without input — one extra frame period between swaps, so an
        // idle carousel stops redrawing a phone's panel at full rate; the aurora still
        // breathes, at half tempo. Any input restores full rate on its own frame.
        if ui.last_input.elapsed() >= IDLE_AFTER {
            std::thread::sleep(IDLE_FRAME_STEP);
        }
        glass.draw(&mut console, &ui);
        if glass.gl_failures >= GL_FAILURE_LIMIT {
            glass.detach();
            bail!(
                "GL surface failed {} times in a row — giving the screen back",
                glass.gl_failures
            );
        }
        published.publish(&mut console, &shared, &store);
    }
}

/// Consecutive GL setup failures (window surface / Skia wrap) the loop tolerates. One is a
/// transient (a window torn down mid-create); a run of them is a context that is not coming
/// back — most likely reclaimed by Android while the app was backgrounded. Dying raises `Dead`,
/// and Kotlin answers with the touch UI; retrying forever leaves a gray never-painted SurfaceView.
const GL_FAILURE_LIMIT: u32 = 3;

/// The EGL surface the console draws into, the Skia surface wrapped over it, and the two ledgers
/// that live and die with them: the GL-failure run and the frame-cost window.
struct Glass {
    // Declaration order is drop order: the Skia surface, the EGL surface, the window under it,
    // then the two contexts.
    skia: Option<(skia_safe::Surface, u32, u32)>,
    surface: Option<EglSurface>,
    /// Held only so the ANativeWindow outlives the EGL surface over it.
    window: Option<NativeWindow>,
    gpu: Option<Gpu>,
    egl: EglContext,
    gl_failures: u32,
    /// What a frame is costing, reported once a `FRAME_REPORT` window (see there).
    frames: u32,
    frame_time: Duration,
    frame_peak: Duration,
    report_at: Instant,
}

impl Glass {
    fn new(egl: EglContext) -> Glass {
        Glass {
            egl,
            gpu: None,
            window: None,
            surface: None,
            skia: None,
            gl_failures: 0,
            frames: 0,
            frame_time: Duration::ZERO,
            frame_peak: Duration::ZERO,
            report_at: Instant::now(),
        }
    }

    /// Release in order: the Skia surface, then the current binding, then the EGL surface and
    /// the window under it. The context itself goes with `self`.
    fn detach(&mut self) {
        self.skia = None;
        if self.surface.is_some() {
            self.egl.release_current();
        }
        self.surface = None;
        self.window = None;
    }

    /// Put an EGL surface over `w`, replacing any that is up (Kotlin re-created the view without
    /// a destroy in between). `false` = the surface failed and the failure run grew.
    fn attach(&mut self, w: NativeWindow, cache_bytes: usize) -> Result<bool> {
        if self.surface.is_some() {
            self.detach();
        }
        match self.egl.window_surface(w.ptr().as_ptr().cast()) {
            Ok(s) => {
                if self.gpu.is_none() {
                    self.gpu = Some(Gpu::new(&self.egl, cache_bytes)?);
                }
                self.surface = Some(s);
                self.window = Some(w);
                self.gl_failures = 0;
                Ok(true)
            }
            Err(e) => {
                log::error!("console: window surface: {e:#}");
                self.gl_failures += 1;
                Ok(false)
            }
        }
    }

    /// One frame, if there is somewhere to draw: re-wrap Skia when the surface size moved, draw,
    /// swap. A failed swap drops the surface and waits for the next one.
    fn draw(&mut self, console: &mut Console, ui: &Ui) {
        let (Some(s), Some(g)) = (self.surface.as_mut(), self.gpu.as_mut()) else {
            return;
        };
        // Every frame, not only when `Cmd::SurfaceChanged` lands: that command crosses a channel,
        // and a frame built between the window's resize and its arrival carries the old size into
        // the new buffer. A rotation then presents the previous layout read at the new stride,
        // with whatever the allocator returned showing through the rows nobody covered.
        s.refresh_size();
        let (w, h) = (s.width, s.height);
        let need_wrap = match &self.skia {
            Some((_, sw, sh)) => *sw != w || *sh != h,
            None => true,
        };
        if need_wrap {
            self.skia = None;
            match g.wrap_window(&self.egl, w, h) {
                Ok(mut surf) => {
                    // A buffer at a new size comes back from the allocator holding whatever was
                    // last in it. The shell covers the frame, so this only matters for the pixels
                    // a partial first frame leaves — and a wrap happens on rotation, not per frame.
                    surf.canvas().clear(skia_safe::Color::BLACK);
                    // The console's real render resolution — the one number a bug report
                    // from a TV never carried. A 4K panel is 4× the fragment work of 1080p
                    // for every pass the shell draws.
                    log::info!("console: drawing at {w}×{h}");
                    self.skia = Some((surf, w, h));
                    self.gl_failures = 0;
                    // Start the frame window here, not at loop entry: the console parks
                    // with no surface while a stream is up, and a window that had been
                    // open across that would report its first frame as "1 frame in 20 min".
                    (
                        self.frames,
                        self.frame_time,
                        self.frame_peak,
                        self.report_at,
                    ) = (0, Duration::ZERO, Duration::ZERO, Instant::now());
                }
                Err(e) => {
                    log::error!("console: {e:#}");
                    self.gl_failures += 1;
                }
            }
        }
        let Some((surf, _, _)) = self.skia.as_mut() else {
            return;
        };
        let viewport = Viewport {
            width: w,
            height: h,
            insets: ui.insets,
            scale: ui.scale,
        };
        // Around the DRAW only, not the swap: `eglSwapBuffers` blocks on vsync, so wall-clock
        // per iteration is always ~the panel period and says nothing. What matters is how much
        // of that period the shell spends building the frame — once that passes the period,
        // the console is missing vsyncs.
        let drew = Instant::now();
        console.frame(
            surf.canvas(),
            &viewport,
            ui.pad_label.as_deref(),
            ui.pad_pref,
            &ui.pads,
        );
        g.context.flush_and_submit();
        let cost = drew.elapsed();
        self.frame_time += cost;
        self.frame_peak = self.frame_peak.max(cost);
        self.frames += 1;
        if self.report_at.elapsed() >= FRAME_REPORT {
            log::info!(
                "console: {w}×{h}, {} frames in {:?} — {:.1} ms/frame mean, {:.1} ms peak",
                self.frames,
                self.report_at.elapsed(),
                self.frame_time.as_secs_f64() * 1000.0 / f64::from(self.frames),
                self.frame_peak.as_secs_f64() * 1000.0,
            );
            (
                self.frames,
                self.frame_time,
                self.frame_peak,
                self.report_at,
            ) = (0, Duration::ZERO, Duration::ZERO, Instant::now());
        }
        if let Err(e) = s.swap() {
            // The window went away under us; wait for the next surface.
            log::warn!("console: {e:#} — dropping the surface");
            self.detach();
        }
    }
}

/// What Kotlin's commands leave behind for the frame: the pad synthesizer, the viewport and
/// the pad legend, and when the last input arrived (the idle throttle's clock).
struct Ui {
    nav: MenuNav,
    sample: MenuSample,
    insets: Insets,
    scale: Option<f64>,
    pad_label: Option<String>,
    pad_pref: Option<GamepadPref>,
    pads: Vec<PadInfo>,
    last_input: Instant,
}

impl Ui {
    fn new() -> Ui {
        Ui {
            nav: MenuNav::new(),
            sample: MenuSample::default(),
            insets: Insets::default(),
            scale: None,
            pad_label: None,
            pad_pref: None,
            pads: Vec::new(),
            last_input: Instant::now(),
        }
    }

    /// Apply one command. `false` = `Quit`. `poll_now` is raised when a pad sample arrived
    /// (a press must not wait for a frame).
    fn apply(
        &mut self,
        cmd: Cmd,
        console: &mut Console,
        glass: &mut Glass,
        shared: &Shared,
        poll_now: &mut bool,
    ) -> Result<bool> {
        match cmd {
            Cmd::Quit => return Ok(false),
            Cmd::Menu(ev) => {
                self.last_input = Instant::now();
                // Discrete events are the remote/keyboard path (Kotlin routes pad
                // buttons through PadSample) — with one wrinkle: a pad's SELECT also
                // arrives here (SkiaConsoleShell's ▲-on-Home shortcut), briefly
                // reading as keys. The next real pad press corrects the legend.
                if let Some(p) = console.menu(ev, InputSource::Keys) {
                    shared.emit(HostEvent::Pulse(p));
                }
            }
            Cmd::PadSample(s) => {
                self.last_input = Instant::now();
                self.sample = s;
                *poll_now = true;
            }
            Cmd::Pointer(p) => {
                self.last_input = Instant::now();
                console.pointer(p);
            }
            Cmd::Key { key, shift, repeat } => {
                self.last_input = Instant::now();
                console.key(key, shift, repeat);
            }
            Cmd::Text(t) => {
                self.last_input = Instant::now();
                console.text(&t);
            }
            Cmd::Phase(ph) => {
                match &ph {
                    Phase::Connecting => console.session_phase(SessionPhase::Connecting),
                    Phase::Streaming => console.session_phase(SessionPhase::Streaming),
                    Phase::Failed(m) => console.session_phase(SessionPhase::Failed(m)),
                    Phase::Ended(r) => {
                        console.session_phase(SessionPhase::Ended(r.as_deref()));
                    }
                    Phase::Reconnecting(m) => {
                        console.session_phase(SessionPhase::Reconnecting(m));
                    }
                }
                // Coming back from a stream: whatever is held on the pad now (the chord
                // that ended it) must be released before it can act here.
                if matches!(ph, Phase::Ended(_) | Phase::Failed(_)) {
                    self.nav.reset();
                }
            }
            Cmd::Navigate(entry) => console.navigate(entry),
            Cmd::SurfaceCreated(w) => {
                // A fresh surface is a fresh entry: snapshot the pad so a button
                // still held from before does not fire into the first frame.
                if glass.attach(w, console.gpu_cache_bytes())? {
                    self.nav.reset();
                }
            }
            // Only a wake-up: `Glass::draw` re-reads the size every frame, so the resize does not
            // wait on this and cannot be missed if it arrives late.
            Cmd::SurfaceChanged => {}
            Cmd::SurfaceDestroyed => {
                glass.detach();
                shared.ack_surface_gone();
            }
            Cmd::Viewport {
                insets: i,
                scale: s,
            } => {
                self.insets = i;
                self.scale = s;
            }
            Cmd::Pads {
                label,
                pref,
                pads: p,
            } => {
                self.pad_label = label;
                self.pad_pref = pref;
                self.pads = p;
            }
        }
        Ok(true)
    }
}

/// The last of each edge-triggered event the render thread raised, so a repeat is silence.
struct Published {
    was_editing: bool,
    /// Last string handed to the screen reader. Repeating one is worse than silence.
    spoken: Option<String>,
    saved_gen: u64,
}

impl Published {
    fn new(console: &Console, store: &SnapshotStore) -> Published {
        Published {
            was_editing: console.editing(),
            spoken: None,
            saved_gen: store.saved_gen(),
        }
    }

    /// Publish what the console raised this frame.
    fn publish(&mut self, console: &mut Console, shared: &Shared, store: &SnapshotStore) {
        while let Some(a) = console.take_action() {
            shared.emit(HostEvent::Action(a));
        }
        let editing = console.editing();
        if editing != self.was_editing {
            self.was_editing = editing;
            shared.emit(HostEvent::Editing(editing));
        }
        let announce = console.focus_announcement();
        if announce != self.spoken {
            self.spoken = announce;
            if let Some(text) = &self.spoken {
                shared.emit(HostEvent::Announce(text.clone()));
            }
        }
        if store.saved_gen() != self.saved_gen {
            let (settings, current_gen) = store.snapshot();
            self.saved_gen = current_gen;
            shared.emit(HostEvent::Settings(Box::new(settings)));
        }
    }
}
