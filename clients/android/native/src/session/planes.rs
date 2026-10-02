//! Plane start/stop: video (HEVC decode → Surface), host→client audio, mic uplink — plus the
//! ~1 Hz decode-stats drain for the HUD.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JIntArray, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::EnvUnowned;

use super::{jni_guard, lock_recover, SESSIONS};

/// Start the retained session's decoder on a `SurfaceView` window.
///
/// Kotlin supplies its ranked codec (`""` selects the platform default), latency/presenter policy,
/// panel rate, and live surface size. A missing key, existing worker, invalid window, or failed
/// thread spawn leaves the session stopped and safely retryable.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    surface: JObject,
    decoder_name: JString,
    low_latency_mode: jboolean,
    ll_feature: jboolean,
    is_tv: jboolean,
    chromeos: jboolean,
    present_priority: jni::sys::jint,
    smooth_buffer: jni::sys::jint,
    panel_fps: jni::sys::jint,
    surface_w: jni::sys::jint,
    surface_h: jni::sys::jint,
) {
    use super::VideoThread;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    env.with_env(|env| -> jni::errors::Result<()> {
        if handle == 0 {
            return Ok(());
        }
        // The decoder name Kotlin picked (empty string / read failure ⇒ None ⇒ default resolver).
        let decoder = decoder_name
            .try_to_string(env)
            .ok()
            .filter(|s| !s.is_empty());
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let mut guard = lock_recover(&h.video);
        if guard.is_some() {
            return Ok(()); // already streaming
        }
        // SAFETY: Kotlin declares `surface` a non-null `Surface`.
        let window = match unsafe { crate::window_from_surface(env, &surface) } {
            Some(w) => w,
            None => {
                log::error!("nativeStartVideo: no ANativeWindow from Surface");
                return Ok(());
            }
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let client = h.client.clone();
        let sd = shutdown.clone();
        let st = h.stats.clone(); // session-lifetime stats (gate survives surface recreate)

        // Seed the live view size with what the view measures right now; `surfaceChanged` keeps it
        // current from here on (the bars hide and the cutout mode changes AFTER this call).
        h.surface_size.store(
            super::pack_surface_size(surface_w, surface_h),
            std::sync::atomic::Ordering::Relaxed,
        );
        let opts = crate::decode::DecodeOptions {
            decoder_name: decoder,
            ll_feature,
            low_latency_mode,
            is_tv,
            chromeos,
            present_priority,
            smooth_buffer,
            panel_hz: panel_fps,
            surface_size: h.surface_size.clone(),
            src_crop: h.src_crop.clone(),
            decoded_size: h.decoded_size.clone(),
            restart: h
                .video_started
                .swap(true, std::sync::atomic::Ordering::Relaxed),
        };
        let join = match std::thread::Builder::new()
            .name("pf-decode".into())
            .spawn(move || crate::decode::run(client, window, sd, st, opts))
        {
            Ok(join) => join,
            Err(e) => {
                log::error!("nativeStartVideo: decode thread spawn failed: {e}");
                return Ok(());
            }
        };
        *guard = Some(VideoThread {
            shutdown,
            join: Some(join),
        });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoSurfaceSize(handle, width, height)` — the video `SurfaceView`'s
/// on-screen pixel size, re-reported on every `surfaceChanged`.
///
/// The ASurfaceControl presenter composites its child layer into exactly this rectangle, and the
/// view resizes UNDER a surface that is never recreated: the stream screen hides the system bars
/// and asks to draw into the display cutout a frame or two after `surfaceCreated`, both of which
/// grow it. Without this the layer would keep painting the picture at its start-up size, in the
/// corner of a bigger surface. Non-positive values are ignored (they'd blank the picture).
/// No-op on a `0` handle. Stored whether or not video is running — the next `nativeStartVideo`
/// then starts from a measured view rather than the window's guess. Not android-gated: pure `jni`
/// + an atomic store, so it links on the host build too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSurfaceSize(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    width: jni::sys::jint,
    height: jni::sys::jint,
) {
    jni_guard((), || {
        let packed = super::pack_surface_size(width, height);
        if handle == 0 || packed == 0 {
            return;
        }
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        h.surface_size
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    })
}

/// `NativeBridge.nativeVideoSourceCrop(handle, left, top, right, bottom)` — the visible part of
/// the frame as fractions, from Kotlin's placement. The SurfaceView is laid out at the picture's
/// rect; this is the one thing its size cannot say, the edges Crop to fill cuts off. Read live by
/// both presenters. Out-of-range values reset to the full frame; no-op on a `0` handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSourceCrop(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    left: jni::sys::jfloat,
    top: jni::sys::jfloat,
    right: jni::sys::jfloat,
    bottom: jni::sys::jfloat,
) {
    jni_guard((), || {
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        h.src_crop.store(
            super::pack_src_crop(left, top, right, bottom),
            std::sync::atomic::Ordering::Relaxed,
        );
    })
}

/// `NativeBridge.nativeVideoMime(handle): String` — the MediaCodec MIME for the codec the host
/// resolved (`"video/hevc"` / `"video/avc"` / `"video/av01"`), so Kotlin can rank `MediaCodecList`
/// decoders for it before calling [`Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo`].
/// Empty string on a `0` or closed handle. Cheap; safe on the UI thread.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // Never null: Kotlin declares a non-null `String`.
        let Some(h) = SESSIONS.get(handle) else {
            return env.new_string("");
        };
        env.new_string(crate::decode::codec_mime(h.client.codec))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoCodecLabel(handle): String` — a short human label for the codec the
/// host resolved (`"H.264"` / `"HEVC"` / `"AV1"` / `"PyroWave"`), for the stats HUD's video-feed
/// line. Distinct from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime`] because the MIME
/// collapses PyroWave onto `video/hevc` and can't name it. Empty string on a `0` handle. Cheap;
/// safe on the UI thread. Android-gated (reads `crate::decode`), matching `nativeVideoMime`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoCodecLabel<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // `JString::default()` is Java null, which the Kotlin `String` return does not allow.
        let session = if handle == 0 {
            None
        } else {
            SESSIONS.get(handle)
        };
        env.new_string(session.map_or("", |h| crate::decode::codec_label(h.client.codec)))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativePyrowaveCapable(): Boolean` — can this device decode PyroWave?
///
/// Session-independent, unlike every other video query here: it asks the GPU, not the host, so
/// Kotlin calls it before connecting to fold `CODEC_PYROWAVE` into the advertised codec bits and
/// to decide whether the Settings picker offers the row at all. Cached in native after the first
/// call (a Vulkan instance is created and destroyed to answer it), so this is safe to call from
/// composition; the first call is the one to keep off the main thread.
///
/// Guarded like the teardown shims: this one runs third-party driver code, and a panic crossing
/// `extern "system"` aborts the app. "No PyroWave" is the right answer for a driver that faults.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePyrowaveCapable(
    _env: EnvUnowned,
    _this: JObject,
) -> jboolean {
    jni_guard(false, crate::pyro::available)
}

/// `NativeBridge.nativeStopVideo(handle)` — stop + join the decode thread (without closing the
/// session). No-op on `0`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopVideo(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.stop_video();
        }
    })
}

/// `NativeBridge.nativeVideoDrain(handle, on)` — the background keep-alive's video drain.
///
/// While the app is backgrounded the decode thread is down: its `Surface` was destroyed with the
/// window. Nothing else pops the frame queue, so it stands, the jump-to-live detector trips, and
/// the host is asked for a keyframe every two seconds until the user comes back. `on` starts a
/// thread that pops and discards instead; `off` stops and joins it. Idempotent either way, and a
/// no-op on `0`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoDrain(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    on: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            if on {
                h.start_drain();
            } else {
                h.stop_drain();
            }
        }
    })
}

/// `NativeBridge.nativeVideoStatsLines(handle, tier, advanced, panelHz, panelModeHz, preset):
/// String?` — close the overlay window and return it formatted by `punktfunk_core::hud`, one
/// `<role>\t<text>` per line, or `null` when no decode thread runs. `panelHz` is the rate this app
/// may render at and `panelModeHz` the panel's mode (0 = unknown). Poll ~1 Hz; each call closes
/// the window. Not android-gated — pure `jni` + connector reads, so it links on the host build too.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoStatsLines<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
    tier: jint,
    advanced: jboolean,
    panel_hz: jfloat,
    panel_mode_hz: jfloat,
    preset: JString<'local>,
) -> JString<'local> {
    use punktfunk_core::hud::{self, Extra, Role, StatsVerbosity};
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(JString::default());
        };
        if lock_recover(&h.video).is_none() {
            return Ok(JString::default()); // not streaming → no stats
        }
        let preset = preset.try_to_string(env).unwrap_or_default();
        let mut s = h.client.hud_snapshot();
        s.decoder = h.stats.decoder_label();
        // SurfaceFlinger's latch is pipeline depth no client paces under: reported, not charged.
        s.shave_os_floor = true;
        s.preset = (!preset.is_empty()).then_some(preset);
        let (judder, coalesced) = (h.stats.judder_permille(), h.stats.coalesced());
        let cadence: Vec<String> = [
            (judder > 0).then(|| format!("judder {judder}‰")),
            (coalesced > 0).then(|| format!("coalesced {coalesced}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !cadence.is_empty() {
            s.extras.push(Extra {
                text: cadence.join(" · "),
                tier: StatsVerbosity::Detailed,
                advanced_only: true,
                role: Role::Warn,
            });
        }
        if let Some(text) = panel_warning(panel_hz, panel_mode_hz, s.refresh_hz) {
            s.extras.push(Extra {
                text,
                tier: StatsVerbosity::Compact,
                advanced_only: false,
                role: Role::Warn,
            });
        }
        let lines = hud::format(&s, StatsVerbosity::from_index(tier.max(0) as u32), advanced);
        env.new_string(hud::encode_lines(&lines))
    })
    .resolve::<LogErrorAndDefault>()
}

/// A panel below the stream costs judder plus a refresh of latency, and reads as a host fault
/// without this line. `rate` is what this app may render at and `mode` the panel's active mode:
/// the first below the second is Android's per-uid cap (the game default frame rate).
fn panel_warning(rate: f32, mode: f32, stream_hz: u32) -> Option<String> {
    let shown = rate.round() as i32;
    if rate > 0.0 && mode > 0.0 && rate + 1.0 < mode {
        return Some(format!("⚠ app capped {shown} Hz by the system"));
    }
    (rate > 0.0 && stream_hz > 0 && rate + 1.0 < stream_hz as f32)
        .then(|| format!("⚠ panel {shown} Hz, not {stream_hz} · check the game frame-rate limit"))
}

/// `NativeBridge.nativeVideoSize(handle): IntArray?` — the negotiated video mode as
/// `[width, height, refreshHz]`. Resolved at the handshake (Welcome), so it is known before a
/// single frame arrives: the UI sizes the video surface to the STREAM's aspect rather than
/// stretching it to the panel's, and pins the panel's display mode to the stream refresh. The
/// trailing `refreshHz` was appended later — old readers index only 0/1 and never see it. `null`
/// on a `0` handle. Not android-gated — pure `jni` + a connector read, so it links on the host
/// build too. Cheap; safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSize<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        if handle == 0 {
            return Ok(JIntArray::default());
        }
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(JIntArray::default());
        };
        let mode = h.client.mode();
        let buf: [i32; 3] = [
            mode.width as i32,
            mode.height as i32,
            mode.refresh_hz as i32,
        ];
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoDecodedSize(handle): IntArray?` — the decoder's picture size as
/// `[width, height]`, or `null` before the first output format (or on a `0` handle). Differs from
/// [`Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSize`] when the host frames the picture
/// for this device. Cheap (one atomic load); safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoDecodedSize<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        let size = SESSIONS.get(handle).and_then(|h| {
            super::unpack_surface_size(h.decoded_size.load(std::sync::atomic::Ordering::Relaxed))
        });
        let Some((w, h)) = size else {
            return Ok(JIntArray::default());
        };
        let arr = env.new_int_array(2)?;
        arr.set_region(env, 0, &[w, h])?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetVideoStatsEnabled(handle, enabled)` — gate per-frame stats sampling on the
/// HUD actually being visible: while disabled the decode thread skips the clock read + lock per AU.
/// Enabling resets the measurement window so a later show never reports stale data. Sticky for the
/// session (survives video stop/start across surface recreation). No-op on `0`. Not android-gated —
/// pure `jni` + an atomic store, so it links on the host build too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetVideoStatsEnabled(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    enabled: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            // Re-enabling opens a fresh window seeded from the current counters.
            h.client.set_hud_enabled(enabled);
        }
    })
}

/// `NativeBridge.nativeStartAudio(handle, lowLatencyMode, isTv)` — start the Opus→AAudio playback
/// supervisor. `lowLatencyMode` (the experimental toggle) tags the stream usage=Game for the HAL's
/// game-audio routing; `isTv` steers the AAudio open ladder (see `crate::audio::open_ladder`).
/// No-op if already started or on a `0` handle. Best-effort: a failure leaves video streaming.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    low_latency_mode: jboolean,
    is_tv: jboolean,
) {
    jni_guard((), || {
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        let mut guard = lock_recover(&h.audio);
        if guard.is_some() {
            return; // already playing
        }
        match crate::audio::AudioPlayback::start(h.client.clone(), low_latency_mode, is_tv) {
            Some(p) => *guard = Some(p),
            None => log::error!("nativeStartAudio: playback init failed (video unaffected)"),
        }
    })
}

/// `NativeBridge.nativeStopAudio(handle)` — stop + join the audio thread and close AAudio (without
/// closing the session). No-op on `0`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.stop_audio();
        }
    })
}

/// `NativeBridge.nativeStartMic(handle, echoCancel): Int` — start mic capture (AAudio input →
/// Opus → host `send_mic`). `echoCancel` opens the capture under the `VoiceCommunication` preset
/// (the HAL's echo canceller / noise suppressor) and allocates an audio session id; the return
/// value is that id (`> 0`), so Kotlin can attach the Java `AcousticEchoCanceler`/`NoiseSuppressor`
/// as a backstop — `0` when none was allocated (echoCancel off, the preset fell back to the plain
/// open, a `0` handle, or the mic failed entirely). Already running (a surface recreate) returns
/// the running capture's id. Caller MUST hold RECORD_AUDIO; a failure (e.g. no permission) leaves
/// the rest of the session streaming.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartMic(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    echo_cancel: jboolean,
) -> jni::sys::jint {
    jni_guard(0, || {
        let Some(h) = SESSIONS.get(handle) else {
            return 0;
        };
        let mut guard = lock_recover(&h.mic);
        if let Some(m) = guard.as_ref() {
            return m.session_id(); // already capturing — same stream, same session
        }
        // The capture SHARES the session's mute flag, so one started while muted stays muted (and
        // sends nothing) from its very first frame — see `SessionHandle::mic_muted`.
        match crate::mic::MicCapture::start(h.client.clone(), echo_cancel, h.mic_muted.clone()) {
            Some(m) => {
                let session_id = m.session_id();
                *guard = Some(m);
                session_id
            }
            None => {
                log::error!("nativeStartMic: mic init failed (RECORD_AUDIO? — session unaffected)");
                0
            }
        }
    })
}

/// `NativeBridge.nativeStopMic(handle)` — stop + join the mic thread and close the AAudio input
/// stream (without closing the session). No-op on `0`. Leaves the session's mute state alone: a
/// surface recreate stops and restarts the mic, and a user who muted must stay muted through it.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopMic(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.stop_mic();
        }
    })
}

/// `NativeBridge.nativeStartPadAudio(handle, pad, fd, haptics, speaker): Boolean` — start tier-A
/// DualSense pad audio on a descriptor Kotlin has already obtained.
///
/// `fd` comes from `UsbDeviceConnection.getFileDescriptor()` **after** claiming the pad's audio
/// streaming interface. Kotlin owns that connection and **must keep it open until
/// `nativeStopPadAudio` returns**: the renderer borrows the descriptor and never closes it, so
/// closing early would pull it out from under an in-flight isochronous transfer.
///
/// Returns `false` when there is nothing to render (both kinds disabled) or the thread would not
/// start. A kernel that refuses the interface claim is NOT reported here — the renderer discovers
/// that on its own thread and degrades to tier C, because some OEM kernels refuse and there is no
/// app-side fix worth blocking a session on.
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartPadAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jni::sys::jint,
    fd: jni::sys::jint,
    haptics: jboolean,
    speaker: jboolean,
) -> jboolean {
    jni_guard(false, || {
        if fd < 0 || !(0..16).contains(&pad) {
            return false;
        }
        let Some(h) = SESSIONS.get(handle) else {
            return false;
        };
        // Replace any previous renderer first: dropping it joins the old thread, so two of them
        // can never hold the same descriptor at once.
        h.stop_pad_audio();
        // SAFETY: Kotlin keeps the connection that owns `fd` open until `nativeStopPadAudio`
        // returns, and that call (or `nativeClose`) drops, and so joins, this renderer first.
        let started = unsafe {
            crate::pad_audio::start(
                std::sync::Arc::clone(&h.client),
                pad as u8,
                fd,
                haptics,
                speaker,
            )
        };
        // The capability declaration and the rumble suppression are NOT done here: the renderer
        // makes both only once its USB stream actually opens (see `pad_audio::render`). Doing them
        // at spawn time would, on a kernel that refuses the interface claim, take the pad off wire
        // rumble and give it nothing in return — no haptics of any kind.
        match started {
            Some(p) => {
                *lock_recover(&h.pad_audio) = Some(p);
                true
            }
            None => false,
        }
    })
}

/// `NativeBridge.nativePadAudioSelfTest(fd, seconds, hz): Int` — drive the pad directly with a
/// tone through the real client render path, with no host and no session involved.
///
/// The check a standalone harness cannot make: it owns its descriptor by construction, so it can
/// never reveal that the client handed the renderer a descriptor something else was already
/// driving. Returns sample frames written, or negative on failure (see `pad_audio::SelfTest`).
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePadAudioSelfTest(
    _env: EnvUnowned,
    _this: JObject,
    fd: jni::sys::jint,
    seconds: jni::sys::jint,
    hz: jni::sys::jint,
) -> jni::sys::jint {
    jni_guard(-1, || {
        if fd < 0 {
            return -1;
        }
        // SAFETY: Kotlin holds the owning UsbDeviceConnection open across this call and drives no
        // other transfers on it (it opens a dedicated connection for exactly this).
        unsafe { crate::pad_audio::self_test(fd, seconds, hz) }
    })
}

/// `NativeBridge.nativeStopPadAudio(handle, pad)` — stop tier-A pad audio and join its thread.
///
/// Returns only once the render thread is joined, which is the point: Kotlin may close the
/// `UsbDeviceConnection` as soon as this returns and not before.
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopPadAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jni::sys::jint,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.stop_pad_audio();
            if (0..16).contains(&pad) {
                // Withdraw the capability and hand the pad back to wire rumble, in that order:
                // the host stops sending 0xD1 before tier C resumes, so the two never overlap.
                h.client.set_pad_audio_caps(pad as u8, 0);
                crate::pad_audio::set_tier_a(pad as u8, false);
                crate::pad_audio::clear_haptics_liveness(pad as u8);
            }
        }
    })
}

/// `NativeBridge.nativeSetMicMuted(handle, muted)` — mute/unmute the mic uplink mid-stream.
///
/// Muting deliberately does NOT stop the capture: the AAudio input stream, the input-preset rung
/// it settled on and its primed buffers all stay exactly as they are, and the encode loop simply
/// drops each 10 ms frame instead of encoding + sending it. A stop/start would re-run the preset
/// fallback ladder and re-prime buffers on every toggle — hundreds of ms, and possibly a different
/// rung (echo cancellation silently lost). This way a toggle costs one atomic store here and one
/// relaxed load per frame there, and takes effect on the very next 10 ms boundary.
///
/// Sticky for the SESSION (the flag lives on the handle, not on the capture), so the mic restart a
/// surface recreate performs comes back muted with no window for an unmuted frame to escape; a
/// fresh session always starts unmuted. No-op on `0`. Not android-gated — pure `jni` + an atomic
/// store, so it links on the host build too.
///
/// One honest consequence of keeping the stream open: the platform's own recording indicator stays
/// lit while muted, because the mic really is still open. What stops is the encode and the send —
/// no captured audio leaves the process.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetMicMuted(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    muted: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.mic_muted
                .store(muted, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

/// `NativeBridge.nativeMicActive(handle): Boolean` — is a mic capture actually RUNNING? `true` only
/// between a `nativeStartMic` that opened a stream and the matching `nativeStopMic`. The in-stream
/// mute control is offered on this evidence rather than on the user's setting, so a device that
/// refused every AAudio input rung (or a missing RECORD_AUDIO grant) shows no control instead of a
/// lie about a mic that is being heard. `false` on a `0` handle. Cheap (one uncontended lock).
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeMicActive(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|h| lock_recover(&h.mic).is_some())
    })
}

/// `NativeBridge.nativeSetStreamMuted(handle, muted)` — silence this device's speakers.
///
/// Local: nothing reaches the host, so a second client joined to the same display keeps hearing
/// the game. Packets keep arriving and keep decoding — the decode thread zeroes only what it
/// queues for AAudio — so the decoder holds its state, the ring keeps its cadence, and unmute
/// lands in step instead of re-priming. No-op on `0`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetStreamMuted(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    muted: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            h.client.set_audio_muted(muted);
        }
    })
}

/// `NativeBridge.nativeAudioMute(handle): Int` — why this session is silent:
/// `AUDIO_MUTE_LOCAL` (1), `AUDIO_MUTE_HOST` (2), both, or `0`. The overlay names the reason
/// from this; a local unmute leaves an operator mute standing and the player is owed the
/// difference. `0` on a `0` handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeAudioMute(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jint {
    jni_guard(0, || {
        SESSIONS
            .get(handle)
            .map_or(0, |h| jint::from(h.client.audio_mute()))
    })
}

#[cfg(test)]
mod panel_tests {
    use super::panel_warning;

    /// A per-uid cap, a real mode below the stream, and a full-rate panel that warns of nothing.
    #[test]
    fn a_panel_below_the_stream_is_named() {
        assert_eq!(
            panel_warning(60.0, 120.0, 120).as_deref(),
            Some("⚠ app capped 60 Hz by the system")
        );
        assert_eq!(
            panel_warning(60.0, 60.0, 120).as_deref(),
            Some("⚠ panel 60 Hz, not 120 · check the game frame-rate limit")
        );
        assert_eq!(panel_warning(120.0, 120.0, 120), None);
        assert_eq!(panel_warning(0.0, 0.0, 120), None);
    }
}
