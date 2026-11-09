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
use crate::console_lifecycle::{Action, Event as SurfaceEvent, Lifecycle};
use anyhow::{bail, Result};
use ndk::native_window::NativeWindow;
use pf_client_core::console::{PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuNav, MenuSample, PadInfo};
use pf_console_ui::bridge::{
    phase_code, Event, Published, PHASE_PROFILE_GONE, PHASE_PROFILE_LOCKED,
};
use pf_console_ui::console::FrameCost;
use pf_console_ui::{
    Console, ConsoleEntry, ConsoleHandles, ConsoleOptions, InputSource, Insets, Key, SnapshotStore,
    Viewport,
};
use punktfunk_core::config::GamepadPref;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What Kotlin asks the render thread to do.
pub(super) enum Cmd {
    Menu(MenuEvent),
    /// A remote's OK, down (`true`) or up.
    Ok(bool),
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
    /// A session edge: its `bridge::phase_code` and message. `SessionPhase` borrows the
    /// message, so the render thread decodes it.
    Phase(u8, String),
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
    /// What the shell raised; Kotlin speaks `announce` through `announceForAccessibility`,
    /// which is a no-op with no screen reader running.
    Console(Event),
    /// The GLES generation the context came up with — Kotlin logs it, nothing more.
    Gles(GlesVersion),
    /// The render thread ended on an error or a panic. Kotlin falls back to its own console.
    Dead(String),
}

impl HostEvent {
    /// The JSON Kotlin parses.
    pub(super) fn to_json(&self) -> String {
        match self {
            HostEvent::Console(e) => e.to_json(),
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

pub(super) type Shared = crate::console_lifecycle::Shared<Cmd, HostEvent>;

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
                let loop_shared = thread_shared.clone();
                let run = move || -> Result<()> {
                    let console = Console::new(opts, entry, &thread_handles)?;
                    render_loop(console, loop_shared, thread_store)
                };
                // A panic ends the thread as an error does: Kotlin must hear of both, or it
                // keeps a frozen console up. The hook has logged the panic itself.
                let dead = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(_) => Some("the render thread panicked".to_owned()),
                };
                if let Some(msg) = dead {
                    log::error!("console: render thread ended: {msg}");
                    thread_shared.emit(HostEvent::Dead(msg));
                }
                thread_shared.retire();
            })?;
        Ok(ConsoleHost {
            shared,
            handles,
            store,
            thread: Some(thread),
        })
    }

    /// Signal the render loop and join it once, from `Drop`. The handle table never drops a
    /// console, so this guards ownership rather than a live teardown path.
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
    if let Err(e) = crate::sys::set_thread_nice(None, -8) {
        log::debug!("console: setpriority(-8) failed (non-fatal): {e}");
    }
}

/// One extra ~vsync period per frame once the console is idle: 60 Hz → ~30, 120 Hz → ~40.
const IDLE_FRAME_STEP: Duration = Duration::from_millis(16);

/// The render thread. Owns EGL + Skia + the console; runs until `Cmd::Quit`.
fn render_loop(mut console: Console, shared: Arc<Shared>, store: Arc<SnapshotStore>) -> Result<()> {
    let mut glass = Glass::new(EglContext::new()?, console.gpu_cache_bytes());
    shared.emit(HostEvent::Gles(glass.egl.version));
    let mut ui = Ui::new();
    let mut published = Published::new(&console, &store);
    let mut menu_out: Vec<MenuEvent> = Vec::new();

    loop {
        // With no surface up, wait for something to arrive.
        let cmds = shared.take(glass.surface.is_none());
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
                    shared.emit(HostEvent::Console(Event::Pulse(p)));
                }
            }
        }

        // Half-rate after 60 s without input — one extra frame period between swaps, so an
        // idle carousel stops redrawing a phone's panel at full rate; the aurora still
        // breathes, at half tempo. Any input restores full rate on its own frame.
        if console.idle() {
            std::thread::sleep(IDLE_FRAME_STEP);
        }
        glass.draw(&mut console, &ui)?;
        glass.report(SurfaceEvent::Tick)?;
        shared.set_holds_launch(console.holds_launch());
        published.publish(&mut console, &store, |e| shared.emit(HostEvent::Console(e)));
    }
}

/// The EGL surface the console draws into, the Skia surface wrapped over it, the window under
/// both, and the lifecycle that decides what a failure among them does.
struct Glass {
    // Declaration order is drop order: the Skia surface, the EGL surface, the window under it,
    // then the two contexts.
    skia: Option<(skia_safe::Surface, u32, u32)>,
    surface: Option<EglSurface>,
    /// Kotlin's window: outlives the EGL surface over it, and stays for the next attach when
    /// that surface fails.
    window: Option<NativeWindow>,
    gpu: Option<Gpu>,
    egl: EglContext,
    /// The Skia resource budget the console asked for.
    cache_bytes: usize,
    life: Lifecycle,
    /// What a frame is costing, logged once a window. A slow TV's log bundle carries it.
    cost: FrameCost,
}

impl Glass {
    fn new(egl: EglContext, cache_bytes: usize) -> Glass {
        Glass {
            egl,
            gpu: None,
            window: None,
            surface: None,
            skia: None,
            cache_bytes,
            life: Lifecycle::default(),
            cost: FrameCost::default(),
        }
    }

    /// Tell the lifecycle what happened and do what it answers, until it answers nothing.
    /// `Err` ends the thread.
    fn report(&mut self, mut ev: SurfaceEvent) -> Result<()> {
        loop {
            match self.life.step(ev) {
                Action::Nothing => return Ok(()),
                Action::Attach => ev = self.attach()?,
                Action::DropSurface => {
                    self.drop_surface();
                    return Ok(());
                }
                Action::Detach => {
                    self.detach();
                    return Ok(());
                }
                Action::Die => {
                    self.detach();
                    bail!(
                        "GL surface failed {} times in a row — giving the screen back",
                        self.life.failures()
                    );
                }
            }
        }
    }

    /// Take Kotlin's new window in place of what is held: a surface still up (Kotlin re-created
    /// the view without a destroy in between), or a window kept for a retry.
    fn created(&mut self, w: NativeWindow) -> Result<()> {
        self.detach();
        self.window = Some(w);
        self.report(SurfaceEvent::Created)
    }

    /// Release in order: the Skia surface, then the current binding, then the EGL surface and
    /// the window under it. The context itself goes with `self`.
    fn detach(&mut self) {
        self.drop_surface();
        self.window = None;
    }

    /// `detach` short of the window.
    fn drop_surface(&mut self) {
        self.skia = None;
        if self.surface.is_some() {
            self.egl.release_current();
        }
        self.surface = None;
    }

    /// Put an EGL surface over the window held: `Attached` or `AttachFailed`. `Err` = no Skia
    /// context over the first surface.
    fn attach(&mut self) -> Result<SurfaceEvent> {
        let Some(w) = self.window.as_ref() else {
            return Ok(SurfaceEvent::AttachFailed);
        };
        match self.egl.window_surface(w) {
            Ok(s) => {
                if self.gpu.is_none() {
                    self.gpu = Some(Gpu::new(&self.egl, self.cache_bytes)?);
                }
                self.surface = Some(s);
                Ok(SurfaceEvent::Attached)
            }
            Err(e) => {
                log::error!("console: window surface: {e:#}");
                Ok(SurfaceEvent::AttachFailed)
            }
        }
    }

    /// Wrap Skia over the EGL surface when it has no wrap or its size moved: `Wrapped` or
    /// `WrapFailed`, `None` when the wrap stands or nothing is up.
    fn wrap(&mut self) -> Option<SurfaceEvent> {
        let (Some(s), Some(g)) = (self.surface.as_mut(), self.gpu.as_mut()) else {
            return None;
        };
        // Every frame, not only when `Cmd::SurfaceChanged` lands: that command crosses a channel,
        // and a frame built between the window's resize and its arrival carries the old size into
        // the new buffer. A rotation then presents the previous layout read at the new stride,
        // with whatever the allocator returned showing through the rows nobody covered.
        s.refresh_size();
        let (w, h) = (s.width, s.height);
        if matches!(&self.skia, Some((_, sw, sh)) if (*sw, *sh) == (w, h)) {
            return None;
        }
        self.skia = None;
        Some(match g.wrap_window(&self.egl, w, h) {
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
                // Start the frame window here, not at loop entry: the console parks
                // with no surface while a stream is up, and a window that had been
                // open across that would report its first frame as "1 frame in 20 min".
                self.cost = FrameCost::default();
                SurfaceEvent::Wrapped
            }
            Err(e) => {
                log::error!("console: {e:#}");
                SurfaceEvent::WrapFailed
            }
        })
    }

    /// One frame, if there is somewhere to draw: wrap, draw, swap, each result reported.
    fn draw(&mut self, console: &mut Console, ui: &Ui) -> Result<()> {
        if let Some(ev) = self.wrap() {
            self.report(ev)?;
        }
        let (Some(s), Some(g), Some((surf, w, h))) =
            (self.surface.as_ref(), self.gpu.as_mut(), self.skia.as_mut())
        else {
            return Ok(());
        };
        let (w, h) = (*w, *h);
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
        let now = Instant::now();
        if let Some(r) = self.cost.add(now - drew, now) {
            log::info!(
                "console: {w}×{h}, {} frames in {:?} — {:.1} ms/frame mean, {:.1} ms peak",
                r.frames,
                r.window,
                r.mean_ms,
                r.peak_ms,
            );
        }
        match s.swap() {
            Ok(()) => self.report(SurfaceEvent::Shown),
            Err(e) => {
                log::warn!("console: {e:#} — dropping the surface");
                self.report(SurfaceEvent::SwapFailed)
            }
        }
    }
}

/// What Kotlin's commands leave behind for the frame: the pad synthesizer, the viewport and
/// the pad legend.
struct Ui {
    nav: MenuNav,
    sample: MenuSample,
    insets: Insets,
    scale: Option<f64>,
    pad_label: Option<String>,
    pad_pref: Option<GamepadPref>,
    pads: Vec<PadInfo>,
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
            Cmd::Ok(down) => {
                if let Some(p) = console.ok(down, InputSource::Keys) {
                    shared.emit(HostEvent::Console(Event::Pulse(p)));
                }
            }
            Cmd::Menu(ev) => {
                // Discrete events are the remote/keyboard path (Kotlin routes pad
                // buttons through PadSample) — with one wrinkle: a pad's SELECT also
                // arrives here (SkiaConsoleShell's ▲-on-Home shortcut), briefly
                // reading as keys. The next real pad press corrects the legend.
                if let Some(p) = console.menu(ev, InputSource::Keys) {
                    shared.emit(HostEvent::Console(Event::Pulse(p)));
                }
            }
            Cmd::PadSample(s) => {
                self.sample = s;
                *poll_now = true;
            }
            Cmd::Pointer(p) => {
                console.pointer(p);
            }
            Cmd::Key { key, shift, repeat } => {
                console.key(key, shift, repeat);
            }
            Cmd::Text(t) => {
                console.text(&t);
            }
            Cmd::Phase(code, msg) => {
                if let Some(phase) = phase_code(code, &msg) {
                    // Coming back from a stream: whatever is held on the pad now (the chord
                    // that ended it) must be released before it can act here.
                    let back = matches!(phase, SessionPhase::Ended(_) | SessionPhase::Failed(_));
                    match code {
                        PHASE_PROFILE_GONE => console.profile_gone(),
                        PHASE_PROFILE_LOCKED => console.profile_locked(),
                        _ => {}
                    }
                    console.session_phase(phase);
                    if back {
                        self.nav.reset();
                    }
                }
            }
            Cmd::Navigate(entry) => console.navigate(entry),
            Cmd::SurfaceCreated(w) => {
                // A fresh surface is a fresh entry: the next poll snapshots the pad, so a button
                // still held from before does not fire into the first frame, a retried one too.
                self.nav.reset();
                glass.created(w)?;
            }
            // Only a wake-up: `Glass::draw` re-reads the size every frame, so the resize does not
            // wait on this and cannot be missed if it arrives late.
            Cmd::SurfaceChanged => {}
            // Acked whatever the lifecycle answers: Kotlin's destroy waits on it.
            Cmd::SurfaceDestroyed => {
                glass.report(SurfaceEvent::Destroyed)?;
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
