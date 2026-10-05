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

use host::{Cmd, ConsoleHost};
use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::EnvUnowned;

use crate::session::{jni_guard, HandleTable};
use pf_client_core::menu_nav::MenuSample;
use pf_console_ui::bridge::{self, CreateOptions, EntryJson, MenuCode, PadsJson, PresetJson};
use pf_console_ui::{
    HostRow, Insets, LibraryGame, LibraryPhase, PairPhase, Platform, ProfilesAnswer, SpeedPhase,
    Stale, WakeStatus,
};
use std::time::Duration;

/// How long `nativeConsoleNextEvent` blocks at most — short enough that Kotlin's poll thread
/// notices `running = false` promptly on teardown (the rumble poll's cadence).
const EVENT_TIMEOUT: Duration = Duration::from_millis(100);

static CONSOLES: HandleTable<ConsoleHost> = HandleTable::new(0x2000_0000_0000_0001);

/// A Kotlin `Int` code as the `u8` the bridge decoders read; out of range is unknown.
fn code(v: jint) -> Option<u8> {
    u8::try_from(v).ok()
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
                if let (Some($h), Some($v)) = (CONSOLES.get(handle), json_arg::<$ty>(env, &json)) {
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
        Ok(CONSOLES.insert(host))
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
        drop(CONSOLES.remove(handle));
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
        let Some(h) = CONSOLES.get(handle) else {
            return Ok(());
        };
        // SAFETY: Kotlin declares `surface` a non-null `Surface`.
        match unsafe { crate::window_from_surface(env, &surface) } {
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
        if let Some(h) = CONSOLES.get(handle) {
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
        if let Some(h) = CONSOLES.get(handle) {
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
        if let Some(h) = CONSOLES.get(handle) {
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
        if let Some(h) = CONSOLES.get(handle) {
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
/// 8 jump back (L1), 9 jump forward (R1), 10/11 a remote's OK down/up (acts on release, held
/// it is the card's menu).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleMenu(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    event: jint,
) {
    jni_guard((), || {
        let (Some(h), Some(code)) = (
            CONSOLES.get(handle),
            code(event).and_then(bridge::menu_code),
        ) else {
            return;
        };
        h.shared.send(match code {
            MenuCode::Ok(down) => Cmd::Ok(down),
            MenuCode::Menu(ev) => Cmd::Menu(ev),
        });
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
        let input = code(kind).and_then(|kind| bridge::pointer_code(kind, x, y, dy));
        if let (Some(h), Some(input)) = (CONSOLES.get(handle), input) {
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
        if let (Some(h), Some(key)) = (CONSOLES.get(handle), code(key).and_then(bridge::key_code)) {
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
        if let (Some(h), Ok(t)) = (CONSOLES.get(handle), text.try_to_string(env)) {
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
        let Some(h) = CONSOLES.get(handle) else {
            return Ok(());
        };
        // Decoded on the render thread: `SessionPhase` borrows the message.
        let msg = message.try_to_string(env).unwrap_or_default();
        if let Some(phase) = code(phase) {
            h.shared.send(Cmd::Phase(phase, msg));
        }
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
/// forwarded, rumble}], "others": [{name, kind, detail}]}`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPads,
    PadsJson,
    |h, p| {
        let mut p = p;
        h.handles.console.set_other_devices(p.take_others());
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
        let out = match CONSOLES
            .get(handle)
            .and_then(|h| h.shared.next_event(EVENT_TIMEOUT))
        {
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
        let out = match CONSOLES.get(handle) {
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
            CONSOLES.get(handle),
            key.try_to_string(env),
            json_arg::<SpeedPhase>(env, &json),
        ) {
            h.handles.console.advance_speed(&k, p);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeConsoleSetProfiles(handle, fpHex, json)` — the answer to
/// `ConsoleCmd::FetchProfiles` for the host pinned to `fpHex`: `{"Listed": [rows]}`,
/// `"NoProfiles"` or `{"Failed": "why"}`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetProfiles(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    fp_hex: JString,
    json: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if let (Some(h), Ok(fp), Some(a)) = (
            CONSOLES.get(handle),
            fp_hex.try_to_string(env),
            json_arg::<ProfilesAnswer>(env, &json),
        ) {
            h.handles.console.set_profiles(&fp, a);
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
        if let (Some(h), Ok(t)) = (CONSOLES.get(handle), text.try_to_string(env)) {
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
        if let Some(h) = CONSOLES.get(handle) {
            h.handles.library.begin_fetch();
        }
    })
}

json_pusher!(
/// `NativeBridge.nativeConsoleSetPadTest(handle, json)` — `{"held": [..], "axes": [[name, v]]}`,
/// the pad's reading while `ConsoleCmd::PadTest` is on.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetPadTest,
    pf_console_ui::PadTestState,
    |h, v| h.handles.console.set_pad_test(v)
);

json_pusher!(
/// `NativeBridge.nativeConsoleSetLicenses(handle, json)` — `[{"heading", "text"}]`, what this app
/// bundles; the answer to `ConsoleCmd::LoadLicenses`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleSetLicenses,
    Vec<pf_console_ui::LicenseSection>,
    |h, v| h.handles.console.set_licenses(v)
);

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
        if let (Some(h), Some(games)) = (
            CONSOLES.get(handle),
            json_arg::<Vec<LibraryGame>>(env, &json),
        ) {
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
        let Some(h) = CONSOLES.get(handle) else {
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

json_pusher!(
/// `NativeBridge.nativeConsoleLibraryDownloads(handle, json)` — the host's `/status`
/// `{"downloads": [...], "grants": n}`, pushed before the same read's
/// `nativeConsoleLibraryRunning`.
    Java_io_unom_punktfunk_kit_NativeBridge_nativeConsoleLibraryDownloads,
    pf_console_ui::DownloadsPush,
    |h, p| h.handles.library.set_downloads(&p.downloads, p.grants)
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
        if let Some(h) = CONSOLES.get(handle) {
            h.handles
                .library
                .set_stale(code(stale).map_or(Stale::No, bridge::stale_code));
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
