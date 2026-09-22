//! The console's C ABI: the shared shell drawn by Skia's Metal backend into a texture Swift
//! hands over each display-link tick. Swift keeps the device, the queue and the drawables, and
//! presents after `punktfunk_console_frame` returns; Skia's work is already on that queue.
//!
//! The shell is bound to the thread that created it: `frame`, `menu`, `pointer`, `key`, `text`,
//! `phase` and `free` run there (the main thread). `push`, `art`, `next_event` and `drain_cmds`
//! are safe from any thread. The JSON is `pf_console_ui::bridge`'s, which Android speaks too.

use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuDir, MenuEvent};
use pf_console_ui::bridge::{
    CreateOptions, EntryJson, Event, Pads, PadsJson, PresetJson, Published,
};
use pf_console_ui::console::FrameCost;
use pf_console_ui::{
    Console, ConsoleEntry, ConsoleHandles, HostRow, InputSource, Insets, Key, LibraryGame,
    LibraryPhase, PairPhase, Platform, SnapshotStore, SpeedPhase, Stale, Viewport, WakeStatus,
};
use skia_safe::gpu::{self, mtl, DirectContext, SurfaceOrigin};
use skia_safe::ColorType;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

/// A frame at most this often once the console is idle.
const IDLE_FRAME: Duration = Duration::from_micros(33_333);

/// Safe-area insets in texture pixels.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkInsets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

// `punktfunk_console_push` kinds, each with the JSON it takes.

/// `[HostRow]` — the home carousel.
pub const PUNKTFUNK_CONSOLE_PUSH_HOSTS: u8 = 0;
/// `"Idle"`, `"Busy"`, `{"Failed": "why"}` or `{"Paired": {"key": "…"}}`.
pub const PUNKTFUNK_CONSOLE_PUSH_PAIR: u8 = 1;
/// `WakeStatus`, or `null` to clear.
pub const PUNKTFUNK_CONSOLE_PUSH_WAKE: u8 = 2;
/// A JSON string: a one-shot toast.
pub const PUNKTFUNK_CONSOLE_PUSH_NOTICE: u8 = 3;
/// `{"key": "…", "phase": SpeedPhase}` — the speed test on that host moved on.
pub const PUNKTFUNK_CONSOLE_PUSH_SPEED: u8 = 4;
/// Any JSON: a library fetch is starting for the shelf on screen.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_BEGIN: u8 = 5;
/// `"Loading"`, `"Empty"`, `"Ready"` or `{"Error": {"title", "body", "can_retry"}}`.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_PHASE: u8 = 6;
/// `[LibraryGame]` — the fetched catalog.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_GAMES: u8 = 7;
/// `[LibraryGame]` — the cached catalog, shown while the fetch runs.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_CACHED: u8 = 8;
/// `[{"app_id": "steam:570", "state": "running"}]` — the host's running games.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_RUNNING: u8 = 9;
/// `0` fresh, `1` waking, `2` offline.
pub const PUNKTFUNK_CONSOLE_PUSH_LIBRARY_STALE: u8 = 10;
/// `Settings` changed elsewhere; the shell reads it on its next mutation. Not a save.
pub const PUNKTFUNK_CONSOLE_PUSH_SETTINGS: u8 = 11;
/// `[{id, name, overrides}]` — the preset catalog.
pub const PUNKTFUNK_CONSOLE_PUSH_PRESETS: u8 = 12;
/// `KnownHosts` — the records `punktfunk://` links are built from.
pub const PUNKTFUNK_CONSOLE_PUSH_KNOWN_HOSTS: u8 = 13;
/// `{"label", "pref", "pads": [PadInfo]}` — the connected controllers.
pub const PUNKTFUNK_CONSOLE_PUSH_PADS: u8 = 14;
/// `{}` for Home, `{"library": HostRow}` for a shelf — re-roots on the next frame.
pub const PUNKTFUNK_CONSOLE_PUSH_NAVIGATE: u8 = 15;

/// One console. Opaque to C.
pub struct PunktfunkConsole {
    owner: ThreadId,
    shell: Mutex<Shell>,
    handles: ConsoleHandles,
    store: Arc<SnapshotStore>,
    events: Mutex<VecDeque<Event>>,
    /// Pushed from any thread, read by the next frame.
    pads: Mutex<Pads>,
    navigate: Mutex<Option<ConsoleEntry>>,
}

struct Shell {
    console: Console,
    context: DirectContext,
    published: Published,
    cost: FrameCost,
    /// When the last frame was drawn, and at what size.
    drawn: Option<(Instant, u32, u32)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `f`, turning a panic into `default`: unwinding across the C boundary is undefined.
fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

/// # Safety
/// `p` is NULL or a NUL-terminated string.
unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-NULL and NUL-terminated per this function's contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn json<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    match serde_json::from_str(text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::error!("console: bad JSON from Swift: {e} in {text:.200}");
            None
        }
    }
}

fn out_string(s: String) -> *mut c_char {
    CString::new(s).map_or(std::ptr::null_mut(), CString::into_raw)
}

impl PunktfunkConsole {
    /// The shell, if this is its thread.
    fn shell(&self) -> Option<MutexGuard<'_, Shell>> {
        if std::thread::current().id() != self.owner {
            tracing::error!("console: called off the thread that created it; ignored");
            return None;
        }
        Some(lock(&self.shell))
    }

    /// Queue what the shell raised. `true` = it asked to quit.
    fn publish(&self, shell: &mut Shell) -> bool {
        let mut quit = false;
        let mut events = lock(&self.events);
        let Shell {
            console, published, ..
        } = shell;
        published.publish(console, &self.store, |e| {
            quit |= matches!(e, Event::Action(OverlayAction::Quit));
            events.push_back(e);
        });
        quit
    }
}

/// Build a console over Swift's Metal device and queue. `options_json` is the bridge's
/// `CreateOptions`. NULL if the JSON or Skia's Metal context fails; the log says which.
///
/// # Safety
/// `options_json` is a NUL-terminated string. `mtl_device` and `mtl_queue` are a live
/// `id<MTLDevice>` and an `id<MTLCommandQueue>` on it; the console retains both.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_new(
    options_json: *const c_char,
    mtl_device: *mut c_void,
    mtl_queue: *mut c_void,
) -> *mut PunktfunkConsole {
    guard(std::ptr::null_mut(), || {
        // SAFETY: NUL-terminated per this function's contract.
        let Some(opts) = unsafe { str_arg(options_json) }.and_then(json::<CreateOptions>) else {
            return std::ptr::null_mut();
        };
        if mtl_device.is_null() || mtl_queue.is_null() {
            return std::ptr::null_mut();
        }
        // The shell has no Apple row set yet; Android's is the nearest (touch, pads, phones).
        let (opts, entry, store) = opts.into_console(Platform::Android);
        let cache_bytes = opts.gpu_cache_bytes;
        // SAFETY: a live device and a queue on it, per this function's contract; Skia retains
        // both for the context's lifetime.
        let backend = unsafe { mtl::BackendContext::new(mtl_device as _, mtl_queue as _) };
        let Some(mut context) = gpu::direct_contexts::make_metal(&backend, None) else {
            tracing::error!("console: Skia's Metal context did not come up");
            return std::ptr::null_mut();
        };
        context.set_resource_cache_limit(cache_bytes);
        let handles = ConsoleHandles::new();
        let console = match Console::new(opts, entry, &handles) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("console: {e:#}");
                return std::ptr::null_mut();
            }
        };
        let published = Published::new(&console, &store);
        Box::into_raw(Box::new(PunktfunkConsole {
            owner: std::thread::current().id(),
            shell: Mutex::new(Shell {
                console,
                context,
                published,
                cost: FrameCost::default(),
                drawn: None,
            }),
            handles,
            store,
            events: Mutex::new(VecDeque::new()),
            pads: Mutex::new((None, None, Vec::new())),
            navigate: Mutex::new(None),
        }))
    })
}

/// Free a console. NULL is a no-op. Off the creating thread it leaks rather than drop the
/// shell there.
///
/// # Safety
/// `c` is NULL or from `punktfunk_console_new`, and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_free(c: *mut PunktfunkConsole) {
    guard((), || {
        if c.is_null() {
            return;
        }
        // SAFETY: from `punktfunk_console_new` and not used again, per the contract.
        let c = unsafe { Box::from_raw(c) };
        if std::thread::current().id() != c.owner {
            tracing::error!("console: freed off the thread that created it; leaked");
            std::mem::forget(c);
        }
    })
}

/// Draw one frame into `mtl_texture` (BGRA8, `width`×`height`) and submit it to the queue.
/// `scale` is design units per pixel; `0` takes the shell's own formula. `false` = nothing
/// drawn (idle, or the texture could not be wrapped): present nothing.
///
/// # Safety
/// `c` is live. `mtl_texture` is an `id<MTLTexture>` from the console's device, render-target
/// usable, BGRA8Unorm, at least `width`×`height`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_frame(
    c: *const PunktfunkConsole,
    mtl_texture: *mut c_void,
    width: u32,
    height: u32,
    insets: PunktfunkInsets,
    scale: f64,
) -> bool {
    guard(false, || {
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return false;
        };
        let Some(mut shell) = c.shell() else {
            return false;
        };
        if mtl_texture.is_null() || width == 0 || height == 0 {
            return false;
        }
        if let Some(entry) = lock(&c.navigate).take() {
            shell.console.navigate(entry);
        }
        let now = Instant::now();
        if let Some((at, w, h)) = shell.drawn {
            if shell.console.idle() && now - at < IDLE_FRAME && (w, h) == (width, height) {
                return false;
            }
        }
        // SAFETY: a live texture from this device, per the contract; Skia retains it while
        // the render target lives.
        let info = unsafe { mtl::TextureInfo::new(mtl_texture as _) };
        let target = gpu::backend_render_targets::make_mtl((width as i32, height as i32), &info);
        let Some(mut surface) = gpu::surfaces::wrap_backend_render_target(
            &mut shell.context,
            &target,
            SurfaceOrigin::TopLeft,
            ColorType::BGRA8888,
            None,
            None,
        ) else {
            tracing::error!("console: Skia could not wrap the {width}×{height} texture");
            return false;
        };
        if shell
            .drawn
            .is_none_or(|(_, w, h)| (w, h) != (width, height))
        {
            tracing::info!("console: drawing at {width}×{height}");
        }
        let viewport = Viewport {
            width,
            height,
            insets: Insets {
                left: insets.left.max(0.0),
                top: insets.top.max(0.0),
                right: insets.right.max(0.0),
                bottom: insets.bottom.max(0.0),
            },
            scale: (scale > 0.0).then_some(scale),
        };
        {
            let pads = lock(&c.pads);
            shell.console.frame(
                surface.canvas(),
                &viewport,
                pads.0.as_deref(),
                pads.1,
                &pads.2,
            );
        }
        drop(surface);
        shell.context.flush_and_submit();
        let done = Instant::now();
        shell.drawn = Some((now, width, height));
        if let Some(r) = shell.cost.add(done - now, done) {
            tracing::info!(
                "console: {width}×{height}, {} frames in {:?} — {:.1} ms/frame mean, {:.1} ms peak",
                r.frames,
                r.window,
                r.mean_ms,
                r.peak_ms,
            );
        }
        c.publish(&mut shell);
        true
    })
}

/// A discrete menu event: 0..3 move up/down/left/right, 4 confirm, 5 back, 6 secondary (Y),
/// 7 tertiary (X), 8 jump back (L1), 9 jump forward (R1). `source` 1 = a pad (its glyphs),
/// 0 = a remote or keyboard. `false` = Back at the root: the press is the system's.
///
/// # Safety
/// `c` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_menu(
    c: *const PunktfunkConsole,
    event: u8,
    source: u8,
) -> bool {
    guard(true, || {
        let ev = match event {
            0 => MenuEvent::Move(MenuDir::Up),
            1 => MenuEvent::Move(MenuDir::Down),
            2 => MenuEvent::Move(MenuDir::Left),
            3 => MenuEvent::Move(MenuDir::Right),
            4 => MenuEvent::Confirm,
            5 => MenuEvent::Back,
            6 => MenuEvent::Secondary,
            7 => MenuEvent::Tertiary,
            8 => MenuEvent::JumpBack,
            9 => MenuEvent::JumpForward,
            _ => return true,
        };
        let source = if source == 1 {
            InputSource::Pad
        } else {
            InputSource::Keys
        };
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return true;
        };
        let Some(mut shell) = c.shell() else {
            return true;
        };
        if let Some(p) = shell.console.menu(ev, source) {
            lock(&c.events).push_back(Event::Pulse(p));
        }
        !c.publish(&mut shell)
    })
}

/// Touch or mouse in texture pixels: kind 0 move, 1 primary down (a mouse, acts at once),
/// 2 primary up, 3 secondary down (= Back), 4 wheel (`dy` steps, + = up), 5 cancel,
/// 6 primary down from a finger — deferred, so a swipe scrolls instead. `true` = consumed.
///
/// # Safety
/// `c` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_pointer(
    c: *const PunktfunkConsole,
    kind: u8,
    x: f32,
    y: f32,
    dy: f32,
) -> bool {
    guard(false, || {
        let down = |button, touch| PointerInput::Down {
            x,
            y,
            button,
            touch,
        };
        let input = match kind {
            0 => PointerInput::Move { x, y },
            1 => down(PointerButton::Primary, false),
            2 => PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            },
            3 => down(PointerButton::Secondary, false),
            4 => PointerInput::Wheel { x, y, dy },
            5 => PointerInput::Cancel,
            6 => down(PointerButton::Primary, true),
            _ => return false,
        };
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return false;
        };
        let Some(mut shell) = c.shell() else {
            return false;
        };
        let used = shell.console.pointer(input);
        c.publish(&mut shell);
        used
    })
}

/// A hardware key: 0..3 left/right/up/down, 4 return, 5 space, 6 escape, 7 backspace,
/// 8 page up, 9 page down, 10 tab, 11 Y, 12 X. `true` = consumed.
///
/// # Safety
/// `c` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_key(
    c: *const PunktfunkConsole,
    key: u8,
    shift: bool,
    repeat: bool,
) -> bool {
    guard(false, || {
        let key = match key {
            0 => Key::Left,
            1 => Key::Right,
            2 => Key::Up,
            3 => Key::Down,
            4 => Key::Return,
            5 => Key::Space,
            6 => Key::Escape,
            7 => Key::Backspace,
            8 => Key::PageUp,
            9 => Key::PageDown,
            10 => Key::Tab,
            11 => Key::Y,
            12 => Key::X,
            _ => return false,
        };
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return false;
        };
        let Some(mut shell) = c.shell() else {
            return false;
        };
        let used = shell.console.key(key, shift, repeat);
        c.publish(&mut shell);
        used
    })
}

/// Typed text while the console reports `editing`.
///
/// # Safety
/// `c` is live; `utf8` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_text(c: *const PunktfunkConsole, utf8: *const c_char) {
    guard((), || {
        // SAFETY: live and NUL-terminated per the contract.
        let (Some(c), Some(text)) = (unsafe { c.as_ref() }, unsafe { str_arg(utf8) }) else {
            return;
        };
        let Some(mut shell) = c.shell() else {
            return;
        };
        shell.console.text(text);
        c.publish(&mut shell);
    })
}

/// Where the session the console asked for stands: 0 connecting, 1 streaming, 2 failed,
/// 3 ended (`message` NULL or empty = clean), 4 reconnecting.
///
/// # Safety
/// `c` is live; `message` is NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_phase(
    c: *const PunktfunkConsole,
    phase: u8,
    message: *const c_char,
) {
    guard((), || {
        // SAFETY: live and NULL-or-NUL-terminated per the contract.
        let (Some(c), msg) = (unsafe { c.as_ref() }, unsafe { str_arg(message) }) else {
            return;
        };
        let msg = msg.unwrap_or("");
        let phase = match phase {
            0 => SessionPhase::Connecting,
            1 => SessionPhase::Streaming,
            2 => SessionPhase::Failed(msg),
            3 => SessionPhase::Ended((!msg.is_empty()).then_some(msg)),
            4 => SessionPhase::Reconnecting(msg),
            _ => return,
        };
        let Some(mut shell) = c.shell() else {
            return;
        };
        shell.console.session_phase(phase);
        c.publish(&mut shell);
    })
}

#[derive(serde::Deserialize)]
struct SpeedJson {
    key: String,
    phase: SpeedPhase,
}

/// Hand the console a model update; `kind` is a `PUNKTFUNK_CONSOLE_PUSH_*`. A bad kind or bad
/// JSON is a logged no-op.
///
/// # Safety
/// `c` is live; `json_text` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_push(
    c: *const PunktfunkConsole,
    kind: u8,
    json_text: *const c_char,
) {
    guard((), || {
        // SAFETY: live and NUL-terminated per the contract.
        let (Some(c), Some(text)) = (unsafe { c.as_ref() }, unsafe { str_arg(json_text) }) else {
            return;
        };
        let (console, library) = (&c.handles.console, &c.handles.library);
        match kind {
            PUNKTFUNK_CONSOLE_PUSH_HOSTS => {
                json::<Vec<HostRow>>(text).map(|v| console.set_hosts(v))
            }
            PUNKTFUNK_CONSOLE_PUSH_PAIR => json::<PairPhase>(text).map(|v| console.set_pair(v)),
            PUNKTFUNK_CONSOLE_PUSH_WAKE => {
                json::<Option<WakeStatus>>(text).map(|v| console.set_wake(v))
            }
            PUNKTFUNK_CONSOLE_PUSH_NOTICE => json::<String>(text).map(|v| console.set_notice(v)),
            PUNKTFUNK_CONSOLE_PUSH_SPEED => {
                json::<SpeedJson>(text).map(|v| console.advance_speed(&v.key, v.phase))
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_BEGIN => {
                library.begin_fetch();
                Some(())
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_PHASE => {
                json::<LibraryPhase>(text).map(|v| library.set_phase(v))
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_GAMES => {
                json::<Vec<LibraryGame>>(text).map(|v| library.set_games(v))
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_CACHED => {
                json::<Vec<LibraryGame>>(text).map(|v| library.set_games_cached(v))
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_RUNNING => {
                json::<Vec<pf_client_core::library::RunningGame>>(text)
                    .map(|v| library.set_running(&v))
            }
            PUNKTFUNK_CONSOLE_PUSH_LIBRARY_STALE => json::<u8>(text).map(|v| {
                library.set_stale(match v {
                    1 => Stale::Waking,
                    2 => Stale::Offline,
                    _ => Stale::No,
                })
            }),
            PUNKTFUNK_CONSOLE_PUSH_SETTINGS => json(text).map(|v| c.store.set(v)),
            PUNKTFUNK_CONSOLE_PUSH_PRESETS => json::<Vec<PresetJson>>(text)
                .map(|v| c.store.set_presets(v.into_iter().map(Into::into).collect())),
            PUNKTFUNK_CONSOLE_PUSH_KNOWN_HOSTS => json(text).map(|v| c.store.set_known_hosts(v)),
            PUNKTFUNK_CONSOLE_PUSH_PADS => {
                json::<PadsJson>(text).map(|v| *lock(&c.pads) = v.into_pads())
            }
            PUNKTFUNK_CONSOLE_PUSH_NAVIGATE => {
                json::<EntryJson>(text).map(|v| *lock(&c.navigate) = Some(v.into_entry()))
            }
            _ => {
                tracing::error!("console: unknown push kind {kind}");
                None
            }
        };
    })
}

/// One title's poster, encoded (JPEG/PNG); the shell decodes it at the size it draws.
///
/// # Safety
/// `c` is live; `id` is a NUL-terminated string; `bytes` is `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_art(
    c: *const PunktfunkConsole,
    id: *const c_char,
    bytes: *const u8,
    len: usize,
) {
    guard((), || {
        // SAFETY: live and NUL-terminated per the contract.
        let (Some(c), Some(id)) = (unsafe { c.as_ref() }, unsafe { str_arg(id) }) else {
            return;
        };
        if bytes.is_null() || len == 0 {
            return;
        }
        // SAFETY: `len` readable bytes per the contract.
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
        c.handles.library.push_art(id.to_string(), bytes);
    })
}

/// The next event the shell raised, or NULL when there is none: `{"action": OverlayAction}`,
/// `{"pulse": "move"|"confirm"|"boundary"}`, `{"editing": bool}`, `{"announce": "…"}` or
/// `{"settings": Settings}` (persist it). Free with `punktfunk_console_string_free`.
///
/// # Safety
/// `c` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_next_event(c: *const PunktfunkConsole) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return std::ptr::null_mut();
        };
        let event = lock(&c.events).pop_front();
        event.map_or(std::ptr::null_mut(), |e| out_string(e.to_json()))
    })
}

/// Every `ConsoleCmd` queued since the last call, as a JSON array (`[]` when none). Free with
/// `punktfunk_console_string_free`.
///
/// # Safety
/// `c` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_drain_cmds(c: *const PunktfunkConsole) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: live per the contract.
        let Some(c) = (unsafe { c.as_ref() }) else {
            return std::ptr::null_mut();
        };
        let cmds = c.handles.bus.drain();
        out_string(serde_json::to_string(&cmds).unwrap_or_else(|_| "[]".into()))
    })
}

/// Free a string from `punktfunk_console_next_event` or `punktfunk_console_drain_cmds`.
///
/// # Safety
/// `s` is NULL or one of those strings, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_console_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: from `CString::into_raw` in this module, freed once, per the contract.
        drop(unsafe { CString::from_raw(s) });
    }
}
