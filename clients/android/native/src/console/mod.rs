//! Skia console UI drawn onto Android's `SurfaceView` through an owned EGL context.
//!
//! Kotlin retains trust, discovery, library, wake, and pairing services and exchanges their serde
//! models through JNI. Console actions return through a blocking event poll and command drain.
//!
//! `nativeConsoleCreate` returns an opaque integer key into an `Arc<ConsoleHost>` table. Lookups
//! retain the host across destroy-vs-poll races; destroy removes the key, and the final reference
//! stops and joins the render thread. Surface, model, menu, pointer, key, and text calls all use the
//! same retained lookup rather than exposing a Rust pointer to Kotlin.

mod egl;
mod gpu;
mod host;

use host::{Cmd, ConsoleHost, Phase};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::EnvUnowned;

use crate::session::jni_guard;
use pf_client_core::console::{PointerButton, PointerInput};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuSample};
use pf_console_ui::bridge::{CreateOptions, EntryJson, PadsJson, PresetJson};
use pf_console_ui::{
    HostRow, Insets, Key, LibraryGame, LibraryPhase, PairPhase, Platform, SpeedPhase, Stale,
    WakeStatus,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How long `nativeConsoleNextEvent` blocks at most — short enough that Kotlin's poll thread
/// notices `running = false` promptly on teardown (the rumble poll's cadence).
const EVENT_TIMEOUT: Duration = Duration::from_millis(100);

static NEXT_CONSOLE_HANDLE: AtomicU64 = AtomicU64::new(0x2000_0000_0000_0001);

fn console_hosts() -> &'static Mutex<HashMap<jlong, Arc<ConsoleHost>>> {
    static HOSTS: OnceLock<Mutex<HashMap<jlong, Arc<ConsoleHost>>>> = OnceLock::new();
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn insert_host(host: ConsoleHost) -> jlong {
    let host = Arc::new(host);
    let mut hosts = crate::session::lock_recover(console_hosts());
    loop {
        let handle = NEXT_CONSOLE_HANDLE.fetch_add(1, Ordering::Relaxed) as jlong;
        if handle != 0 && !hosts.contains_key(&handle) {
            hosts.insert(handle, host);
            return handle;
        }
    }
}

fn host(handle: jlong) -> Option<Arc<ConsoleHost>> {
    if handle == 0 {
        return None;
    }
    crate::session::lock_recover(console_hosts())
        .get(&handle)
        .cloned()
}

fn remove_host(handle: jlong) -> Option<Arc<ConsoleHost>> {
    if handle == 0 {
        return None;
    }
    crate::session::lock_recover(console_hosts()).remove(&handle)
}

fn json_arg<T: serde::de::DeserializeOwned>(env: &mut jni::Env, s: &JString) -> Option<T> {
    let text = s.try_to_string(env).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            log::error!("console: bad JSON from Kotlin: {e} in {text:.200}");
            None
        }
    }
}

/// One `nativeConsoleXxx(handle, json)` pusher: parse the JSON as `$ty` and hand it to `$apply`
/// on the host. A bad handle or bad JSON is a logged no-op.
macro_rules! json_pusher {
    ($(#[$doc:meta])* $name:ident, $ty:ty, |$h:ident, $v:ident| $apply:expr) => {
        $(#[$doc])*
        #[unsafe(no_mangle)]
        pub extern "system" fn $name(
            mut env: EnvUnowned,
            _this: JObject,
            handle: jlong,
            json: JString,
        ) {
            env.with_env(|env| -> jni::errors::Result<()> {
                if let (Some($h), Some($v)) = (host(handle), json_arg::<$ty>(env, &json)) {
                    $apply;
                }
                Ok(())
            })
            .resolve::<LogErrorAndDefault>()
        }
    };
}

/// Build the console and return its opaque table key.
///
/// The render thread parks until a surface arrives. `0` means setup failed and Kotlin keeps its
/// fallback console; no Rust allocation address crosses JNI.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleCreate(
    mut env: EnvUnowned,
    _this: JObject,
    options: JString,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let Some(mut opts) = json_arg::<CreateOptions>(env, &options) else {
            return Ok(0);
        };
        // The same probe that gates the `CODEC_PYROWAVE` advertisement, so the codec
        // row cannot offer a picture this GPU would never decode. Cached per process.
        opts.pyrowave_ok = crate::pyro::available();
        let (console_opts, entry, store) = opts.into_console(Platform::Android);
        let host = match ConsoleHost::start(console_opts, entry, store) {
            Ok(host) => host,
            Err(e) => {
                log::error!("console: render thread spawn failed: {e}");
                return Ok(0);
            }
        };
        Ok(insert_host(host))
    })
    .resolve::<LogErrorAndDefault>()
}

/// Remove one console key; the final retained call then stops and joins the render thread.
/// Zero, stale, duplicate, and concurrent destroys are no-ops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleDestroy(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        drop(remove_host(handle));
    })
}

/// `NativeBridge.nativeConsoleSurfaceCreated(handle, surface)` — the `SurfaceView`'s surface is
/// up; the render thread wraps it in EGL and starts drawing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceCreated(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    surface: JObject,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        // SAFETY: `env`/`surface` are valid JNI pointers for this call; the raw casts bridge the
        // jni-sys version skew between the `jni` and vendored `ndk` crates (see nativeStartVideo).
        let window = unsafe {
            ndk::native_window::NativeWindow::from_surface(
                env.get_raw() as *mut _,
                surface.as_raw() as *mut _,
            )
        };
        match window {
            Some(w) => h.shared.send(Cmd::SurfaceCreated(w)),
            None => log::error!("console: no ANativeWindow from Surface"),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSurfaceChanged(handle)` — size changed; the thread re-reads it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceChanged(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            h.shared.send(Cmd::SurfaceChanged);
        }
    })
}

/// `NativeBridge.nativeConsoleSurfaceDestroyed(handle)` — BLOCKS until the render thread has
/// released the EGL surface: Android forbids touching a `Surface` after `surfaceDestroyed`
/// returns, and the GL driver would otherwise still be presenting into it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSurfaceDestroyed(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            h.shared.destroy_surface_blocking();
        }
    })
}

/// `NativeBridge.nativeConsoleSetViewport(handle, left, top, right, bottom, scale)` — safe-area
/// insets in surface pixels and the design-unit scale (`0` = the shell's own couch formula).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetViewport(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    left: jfloat,
    top: jfloat,
    right: jfloat,
    bottom: jfloat,
    scale: jfloat,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            h.shared.send(Cmd::Viewport {
                insets: Insets {
                    left: left.max(0.0),
                    top: top.max(0.0),
                    right: right.max(0.0),
                    bottom: bottom.max(0.0),
                },
                scale: (scale > 0.0).then_some(f64::from(scale)),
            });
        }
    })
}

/// `NativeBridge.nativeConsolePadSample(handle, buttons, lx, ly, dpad)` — the raw pad, whenever
/// it changes: `buttons` bit i = a, b, x, y, l1, r1 held; `lx`/`ly` the left stick in wire
/// units (±32767, +y = down); `dpad` bit i = up, down, left, right held. The shared
/// `MenuNav` turns it into menu events with the same dead zone, repeat cadence and hysteresis
/// as the desktop.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsolePadSample(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    buttons: jint,
    lx: jint,
    ly: jint,
    dpad: jint,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            let bit = |v: jint, i: u32| v & (1 << i) != 0;
            h.shared.send(Cmd::PadSample(MenuSample {
                buttons: [
                    bit(buttons, 0),
                    bit(buttons, 1),
                    bit(buttons, 2),
                    bit(buttons, 3),
                    bit(buttons, 4),
                    bit(buttons, 5),
                ],
                lx: lx.clamp(-32767, 32767) as i16,
                ly: ly.clamp(-32767, 32767) as i16,
                dpad: [bit(dpad, 0), bit(dpad, 1), bit(dpad, 2), bit(dpad, 3)],
            }));
        }
    })
}

/// `NativeBridge.nativeConsoleMenu(handle, event)` — a discrete menu event, for input that is
/// already an event on the Kotlin side (a TV remote's D-pad `KeyEvent`s, the touch escape hatch):
/// 0..3 = move up/down/left/right, 4 confirm, 5 back, 6 secondary (Y), 7 tertiary (X),
/// 8 jump back (L1), 9 jump forward (R1).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleMenu(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    event: jint,
) {
    jni_guard((), || {
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
            _ => return,
        };
        if let Some(h) = host(handle) {
            h.shared.send(Cmd::Menu(ev));
        }
    })
}

/// `NativeBridge.nativeConsolePointer(handle, kind, x, y, dy)` — touch/mouse in surface pixels:
/// kind 0 move, 1 primary down (a mouse — acts immediately), 2 primary up, 3 secondary down
/// (= Back), 4 wheel (`dy` steps, + = up), 5 cancel, 6 primary down from a finger/stylus on
/// the glass — the shell defers it so a swipe scrolls instead of acting on contact.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsolePointer(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    kind: jint,
    x: jfloat,
    y: jfloat,
    dy: jfloat,
) {
    jni_guard((), || {
        let input = match kind {
            0 => PointerInput::Move { x, y },
            1 => PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch: false,
            },
            2 => PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            },
            3 => PointerInput::Down {
                x,
                y,
                button: PointerButton::Secondary,
                touch: false,
            },
            4 => PointerInput::Wheel { x, y, dy },
            5 => PointerInput::Cancel,
            6 => PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch: true,
            },
            _ => return,
        };
        if let Some(h) = host(handle) {
            h.shared.send(Cmd::Pointer(input));
        }
    })
}

/// `NativeBridge.nativeConsoleKey(handle, key, shift, repeat)` — a hardware key the console
/// understands: 0..3 left/right/up/down, 4 return, 5 space, 6 escape, 7 backspace, 8 page up,
/// 9 page down, 10 tab, 11 the letter Y, 12 the letter X. Anything else is Kotlin's to keep.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleKey(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    key: jint,
    shift: jboolean,
    repeat: jboolean,
) {
    jni_guard((), || {
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
            _ => return,
        };
        if let Some(h) = host(handle) {
            h.shared.send(Cmd::Key { key, shift, repeat });
        }
    })
}

/// `NativeBridge.nativeConsoleText(handle, text)` — typed characters while the console reports
/// `editing` (see the `editing` event).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleText(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(t)) = (host(handle), text.try_to_string(env)) {
            h.shared.send(Cmd::Text(t));
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSessionPhase(handle, phase, message)` — where the session the
/// console asked for stands: 0 connecting, 1 streaming, 2 failed(message), 3 ended(message or
/// empty = clean), 4 reconnecting(message).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSessionPhase(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    phase: jint,
    message: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        let msg = message.try_to_string(env).unwrap_or_default();
        let ph = match phase {
            0 => Phase::Connecting,
            1 => Phase::Streaming,
            2 => Phase::Failed(msg),
            3 => Phase::Ended((!msg.is_empty()).then_some(msg)),
            4 => Phase::Reconnecting(msg),
            _ => return Ok(()),
        };
        h.shared.send(Cmd::Phase(ph));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

json_pusher!(
/// `NativeBridge.nativeConsoleNavigate(handle, entryJson)` — re-root the console (`{"library":
/// <HostRow>}` opens that host's shelf over Home; `{}` is Home).
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNavigate,
    EntryJson,
    |h, e| h.shared.send(Cmd::Navigate(e.into_entry()))
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetPads(handle, padsJson)` — the connected controllers for the
/// chip, the settings rows and the controllers screen: `{"label": "DualSense", "pref": 1,
/// "pads": [{name, key, pref, steam_virtual, battery: {percent, charging} | null, detail,
/// forwarded, rumble}]}`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPads,
    PadsJson,
    |h, p| {
        let (label, pref, pads) = p.into_pads();
        h.shared.send(Cmd::Pads { label, pref, pads })
    }
);

/// `NativeBridge.nativeConsoleNextEvent(handle): String` — block up to ~100 ms for the next
/// event: `{"action": <OverlayAction>}`, `{"pulse": "move"|"confirm"|"boundary"}`,
/// `{"editing": bool}`, `{"settings": <Settings>}` (persist it), `{"gles": 2|3}`,
/// `{"dead": "<why>"}`. Empty string on timeout / no handle. Run from a Kotlin poll thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNextEvent<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = match host(handle).and_then(|h| h.shared.next_event(EVENT_TIMEOUT)) {
            Some(ev) => ev.to_json(),
            None => String::new(),
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleDrainCmds(handle): String` — every `ConsoleCmd` queued since the
/// last call, as a JSON array (`[]` when none). Poll on a short cadence from the service side.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleDrainCmds<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let out = match host(handle) {
            Some(h) => {
                let cmds = h.handles.bus.drain();
                serde_json::to_string(&cmds).unwrap_or_else(|_| "[]".into())
            }
            None => "[]".into(),
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

// ---- model pushers -----------------------------------------------------------------------

json_pusher!(
/// `NativeBridge.nativeConsoleSetHosts(handle, json)` — the home carousel's rows (`[HostRow]`).
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetHosts,
    Vec<HostRow>,
    |h, rows| h.handles.console.set_hosts(rows)
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetPair(handle, json)` — the pairing ceremony's phase
/// (`"Idle"`, `"Busy"`, `{"Failed": "why"}`, `{"Paired": {"key": "…"}}`).
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPair,
    PairPhase,
    |h, p| h.handles.console.set_pair(p)
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetWake(handle, json)` — the wake-and-wait card's status
/// (`WakeStatus` JSON, or `null` to clear).
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetWake,
    Option<WakeStatus>,
    |h, w| h.handles.console.set_wake(w)
);

/// `NativeBridge.nativeConsoleAdvanceSpeed(handle, key, json)` — a new [`SpeedPhase`] for the
/// speed test on `key`. No setter for the status itself: the shell seeds and clears that slot,
/// which is what makes a dismissed (or superseded) test's late result a no-op here.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleAdvanceSpeed(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    key: JString,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(k), Some(p)) = (
            host(handle),
            key.try_to_string(env),
            json_arg::<SpeedPhase>(env, &json),
        ) {
            h.handles.console.advance_speed(&k, p);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleNotice(handle, text)` — a one-shot toast from a service worker.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleNotice(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(t)) = (host(handle), text.try_to_string(env)) {
            h.handles.console.set_notice(t);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryBegin(handle)` — a fetch is starting for the shelf on
/// screen: bumps the fetch epoch and sets `Loading`. Call this — not a bare `Loading` phase —
/// so the shelf can tell its own result from a previous host's cached one.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryBegin(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            h.handles.library.begin_fetch();
        }
    })
}

json_pusher!(
/// `NativeBridge.nativeConsoleLibraryPhase(handle, json)` — `"Loading"`, `"Empty"`, `"Ready"`,
/// or `{"Error": {"title", "body", "can_retry"}}`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryPhase,
    LibraryPhase,
    |h, p| h.handles.library.set_phase(p)
);

/// `NativeBridge.nativeConsoleLibraryGames(handle, json, cached)` — the catalog (`[LibraryGame]`);
/// `cached` = this is the last-known list from the cache, shown while the fetch runs.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryGames(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    json: JString,
    cached: jboolean,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Some(games)) = (host(handle), json_arg::<Vec<LibraryGame>>(env, &json)) {
            if cached {
                h.handles.library.set_games_cached(games);
            } else {
                h.handles.library.set_games(games);
            }
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleLibraryArt(handle, id, bytes)` — one title's poster, encoded
/// (JPEG/PNG); the shell decodes at the size it draws.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryArt(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    id: JString,
    bytes: JByteArray,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = host(handle) else {
            return Ok(());
        };
        let id = id.try_to_string(env)?;
        let bytes = env.convert_byte_array(&bytes)?;
        h.handles.library.push_art(id, bytes);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

json_pusher!(
/// `NativeBridge.nativeConsoleLibraryRunning(handle, json)` — the host's `/status` `games[]`
/// (`[{"app_id": "steam:570", "state": "running"}, …]`).
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryRunning,
    Vec<pf_client_core::library::RunningGame>,
    |h, games| h.handles.library.set_running(&games)
);

/// `NativeBridge.nativeConsoleLibraryStale(handle, stale)` — 0 fresh, 1 waking, 2 offline.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryStale(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    stale: jint,
) {
    jni_guard((), || {
        if let Some(h) = host(handle) {
            h.handles.library.set_stale(match stale {
                1 => Stale::Waking,
                2 => Stale::Offline,
                _ => Stale::No,
            });
        }
    })
}

json_pusher!(
/// `NativeBridge.nativeConsoleSetSettings(handle, json)` — a settings change made elsewhere
/// (the touch UI, a deep link): the shell reads this on its next mutation. Not a save.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetSettings,
    pf_client_core::trust::Settings,
    |h, s| h.store.set(s)
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetPresets(handle, json)` — the preset catalog
/// `[{id, name, overrides}]`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPresets,
    Vec<PresetJson>,
    |h, p| h.store.set_presets(p.into_iter().map(Into::into).collect())
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetKnownHosts(handle, json)` — the known-hosts records
/// (`KnownHosts` JSON) the console builds `punktfunk://` links from.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetKnownHosts,
    pf_client_core::trust::KnownHosts,
    |h, k| h.store.set_known_hosts(k)
);
