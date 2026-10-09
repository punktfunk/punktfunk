//! Windows audio auto-wiring: virtual-mic inject plus desktop-audio loopback.
//!
//! The two jobs must land on different endpoints — WASAPI loopback recaptures
//! whatever the mic writes, so sharing a cable is an infinite echo. Assignment
//! lives in the pure [`wiring_plan`](super::wiring_plan) module; this crate
//! enumerates, applies the plan, and logs. [`wire_now`] runs on every mic/capture
//! (re)open because endpoints churn (boot registration, hotplug, driver installs).
//!
//! Playback and recording defaults park only while a desktop-audio capture is
//! open (the idle mic pump must not silence speakers or steal the default mic).
//! The operator's devices are remembered in memory plus on-disk crash markers
//! ([`park_default_playback`] / [`park_default_recording`]) and restored when
//! capture closes or on the next process's first wiring pass. A default the
//! operator changed mid-stream is left alone.
//!
//! Default writes go through undocumented `IPolicyConfig` (the call `mmsys.cpl`
//! makes; neither crate exposes it). `audio.output_mode = follow_default` still
//! computes the plan — the mic needs a target — but skips the default writes.

use super::wiring_plan::{self, plan, plan_with_formats, Endpoint, MixFormat, Wiring};
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::sync::Mutex;
use wasapi::Direction;

/// Engine mix format of a render endpoint, or `None` if it cannot be asked.
///
/// Shared-mode capture requests 48 kHz f32 with autoconvert, so WASAPI will
/// silently downmix a voice-carrier (mono / 24 kHz). One `IAudioClient`
/// activation per endpoint, only during a wiring pass. Every failure maps to
/// `None`: the plan treats unknown as non-narrowing, matching pre-format boxes.
pub fn mix_format_of(ep: &Endpoint) -> Option<MixFormat> {
    let fmt = open_endpoint(ep)
        .ok()?
        .get_iaudioclient()
        .ok()?
        .get_mixformat()
        .ok()?;
    Some(MixFormat {
        rate_hz: fmt.get_samplespersec(),
        channels: fmt.get_nchannels(),
        bits: fmt.get_bitspersample(),
    })
}

/// Engine rate the desktop-audio loopback would capture at, answered before
/// `Welcome` and without opening a capture stream.
///
/// Shared-mode `IAudioClient` with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` will
/// interpolate a 96 kHz request on a 48 kHz engine and return success — so the
/// number that matters is [`mix_format_of`]. An operator who wants 96 kHz sets
/// the endpoint's own rate in Windows; the host then sees it here.
///
/// Read-only: enumerates, runs the pure [`plan_with_formats`], reads one mix
/// format. Not [`wire_now_full`] — that parks defaults, mints, and logs, none
/// of which may happen for a session that is about to resolve to Opus.
///
/// Plans from the same [`PlanInputs`] as [`wire_now_full`], so it reads the format of
/// the device the capture will use. The real probe (not [`wiring_plan::no_formats`]) is
/// load-bearing — narrowing demotes a candidate below real hardware. Every failure is
/// [`CaptureRate::Unknown`] (decline): unlike the wiring plan, unknown here means we
/// cannot prove the label.
///
/// Must run on a COM-initialized thread; initializes MTA because the caller is
/// a tokio blocking-pool thread. A repeat init returns `S_FALSE` (success).
pub fn probe_capture_rate() -> super::CaptureRate {
    if let Err(e) = wasapi::initialize_mta().ok() {
        tracing::debug!(error = %e, "hi-res capture-rate probe: CoInitializeEx (MTA) failed");
        return super::CaptureRate::Unknown;
    }
    // Enumeration only — minting stays out of this path, as the doc says.
    let wiring = PlanInputs::read().plan(&mix_format_of);
    let Some(ep) = wiring.loopback_render else {
        tracing::debug!("hi-res capture-rate probe: no desktop-audio loopback endpoint is planned");
        return super::CaptureRate::Unknown;
    };
    // `plan_with_formats` ranked on this format but did not return the numbers.
    // Caching through it to save one `GetMixFormat` would add a second way to disagree.
    match mix_format_of(&ep) {
        Some(f) => super::CaptureRate::Engine(f.rate_hz),
        None => {
            tracing::debug!(device = %ep.0,
                "hi-res capture-rate probe: the planned loopback endpoint would not report its \
                 mix format");
            super::CaptureRate::Unknown
        }
    }
}

/// Active endpoints only (`friendly_name`, `endpoint_id`).
fn list_endpoints(dir: Direction) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let Ok(en) = wasapi::DeviceEnumerator::new() else {
        return out;
    };
    let Ok(coll) = en.get_device_collection(&dir) else {
        return out;
    };
    let Ok(n) = coll.get_nbr_devices() else {
        return out;
    };
    for i in 0..n {
        if let Ok(dev) = coll.get_device_at_index(i) {
            let id = dev.get_id().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            out.push((dev.get_friendlyname().unwrap_or_default(), id));
        }
    }
    out
}

/// True when the loopback plan must prefer real hardware over the silent sink. With voice
/// chat kept on the host, `host_and_client` keeps the silent sink and the host renders the
/// mix to the operator's output instead ([`playthrough_requested`]).
pub fn host_audio_requested() -> bool {
    let cfg = pf_host_config::config();
    cfg.audio_output_mode.prefers_host_hardware()
        && cfg.audio_voice_chat != pf_host_config::VoiceChatRoute::Host
}

/// `host_and_client` with voice chat on the host: capture stays on the silent sink and a
/// render stream on the parked output lets the operator hear the mix.
pub fn playthrough_requested() -> bool {
    let cfg = pf_host_config::config();
    cfg.audio_output_mode.prefers_host_hardware()
        && cfg.audio_voice_chat == pf_host_config::VoiceChatRoute::Host
}

/// The output the operator heard before this capture parked the default on the plan's
/// sink; `None` while nothing is parked. The voice-chat pin and playthrough target.
pub fn parked_previous_render() -> Option<String> {
    PLAYBACK
        .parked
        .lock()
        .unwrap()
        .as_ref()
        .map(|(prev, _)| prev.clone())
}

/// Skip default-device writes: `follow_default` mode, a session's keep-host-audio
/// ask, or a seat host.
///
/// The default endpoints are machine-global while a seat host is one of several
/// on the box, so parking them would hand every seat's playback to whichever
/// seat streamed last. A seat's own endpoints carry its marker and the wiring
/// plan finds them by id.
pub fn keep_default_devices() -> bool {
    pf_host_config::config().audio_output_mode.keeps_default()
        || crate::capture_policy::session_keeps_default()
        || pf_paths::seat::is_seat_host()
}

/// One wiring pass: assignment, fingerprint of the same enumeration the plan consumed
/// (a device arriving mid-pass must not key the waiter to a set the plan never saw),
/// and the render inventory for the no-loopback diagnosis.
pub struct WiredPlan {
    pub wiring: Wiring,
    pub fingerprint: u64,
    pub renders: Vec<Endpoint>,
}

/// Endpoint-set hash with no plan, no default writes, no logs. Cheap poll while a
/// capture waits out a failure; [`wire_now`] runs again only once this moves.
/// Must run on a COM-initialized thread.
pub fn endpoint_fingerprint() -> u64 {
    wiring_plan::fingerprint(
        &list_endpoints(Direction::Render),
        &list_endpoints(Direction::Capture),
    )
}

pub fn wire_now(park_defaults: bool) -> Wiring {
    wire_now_full(park_defaults).wiring
}

/// Last wiring verdict. Change detection for the once-per-change log lives here.
static LAST_WIRING: Mutex<Option<Wiring>> = Mutex::new(None);

/// Snapshot of [`LAST_WIRING`]. A status poll must not run COM or IPolicyConfig writes.
pub fn last_wiring() -> Option<Wiring> {
    LAST_WIRING.lock().unwrap().clone()
}

/// Pad-audio render ids among `renders` — exclusion data [`plan`] consumes.
/// Identity lives in [`super::pad_endpoint`]; this is the per-pass collection.
fn pad_render_ids(renders: &[Endpoint]) -> Vec<String> {
    renders
        .iter()
        .filter(|(_, id)| super::pad_endpoint::is_pad_render_endpoint(id))
        .map(|(_, id)| id.clone())
        .collect()
}

/// Enumerate, plan, apply default-device writes (unless `follow_default`), return the
/// assignment. `park_defaults` is true only from desktop-audio capture open: that parks
/// playback on the loopback sink and recording on the virtual mic. The idle mic pump
/// passes false — it must neither silence speakers nor steal the default microphone.
/// Set once a wait has timed out: minting succeeded but MMDevice never listed the result, so
/// every later pass would pay the ceiling for endpoints that are not coming. True inside a seat,
/// where minting lands a machine-wide devnode the remote session cannot enumerate at all.
static MINTED_NEVER_LISTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Render and capture endpoints, once everything already minted is enumerable. MMDevice publishes
/// a minted endpoint a few tens of milliseconds after the devnode lands, so an immediate
/// enumeration can miss the endpoints this process just created and plan as if the box had none.
/// Waits only for ids [`minted_ids`](super::minted::minted_ids) reports, so a box that mints
/// nothing — or one where they never show, see [`MINTED_NEVER_LISTED`] — pays one enumeration.
fn enumerate_including_minted() -> (Vec<Endpoint>, Vec<Endpoint>) {
    use std::sync::atomic::Ordering;
    // 500 ms ceiling: measured appearance is ~12 ms, and the caller is on the session-open path.
    let attempts = if MINTED_NEVER_LISTED.load(Ordering::Relaxed) {
        1
    } else {
        10
    };
    for attempt in 0..attempts {
        let renders = list_endpoints(Direction::Render);
        let captures = list_endpoints(Direction::Capture);
        let minted = super::minted::minted_ids();
        let listed = |want: &Option<String>, eps: &[Endpoint]| {
            want.as_ref()
                .is_none_or(|id| eps.iter().any(|(_, have)| have == id))
        };
        if listed(&minted.speakers_render, &renders)
            && listed(&minted.mic_render, &renders)
            && listed(&minted.mic_capture, &captures)
        {
            if attempt > 0 {
                tracing::debug!(attempt, "minted audio endpoints became enumerable");
            }
            return (renders, captures);
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    if !MINTED_NEVER_LISTED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "a minted audio endpoint never appeared in this session's device enumeration — \
             planning without it, and not waiting for it again (a seat's remote session cannot \
             enumerate the machine-wide devnode minting creates)"
        );
    }
    (
        list_endpoints(Direction::Render),
        list_endpoints(Direction::Capture),
    )
}

/// Everything a wiring plan reads, gathered once so the capture-rate probe and the wiring
/// pass cannot plan from different inputs.
struct PlanInputs {
    renders: Vec<Endpoint>,
    captures: Vec<Endpoint>,
    mic_want: Option<String>,
    /// Pad-audio ids: platform identity the pure plan filters out of every role.
    pad_ids: Vec<String>,
    /// Minted Speakers/Microphone ids — empty until the provider latches.
    minted: wiring_plan::MintedIds,
}

impl PlanInputs {
    /// Waits for already-minted endpoints: a bare [`list_endpoints`] can miss one that is
    /// still appearing, and the plan would pick a device the capture will not use.
    fn read() -> PlanInputs {
        let (renders, captures) = enumerate_including_minted();
        PlanInputs {
            pad_ids: pad_render_ids(&renders),
            mic_want: std::env::var("PUNKTFUNK_MIC_DEVICE")
                .ok()
                .map(|s| s.to_lowercase()),
            minted: super::minted::minted_ids(),
            renders,
            captures,
        }
    }

    fn plan(&self, probe: &dyn Fn(&Endpoint) -> Option<MixFormat>) -> Wiring {
        plan_with_formats(
            &self.renders,
            &self.captures,
            self.mic_want.as_deref(),
            host_audio_requested(),
            probe,
            // Stereo: the floor every session uses, and the only count a narrowing verdict
            // can be made against without a session. Hi-res carries only stereo.
            2,
            &self.pad_ids,
            &self.minted,
        )
    }
}

/// COM-initialized thread. Logged only when the assignment changes.
pub fn wire_now_full(park_defaults: bool) -> WiredPlan {
    recover_orphaned_default();
    // Mint BEFORE enumerating, and wait for what was minted to become enumerable. A freshly
    // minted endpoint is not in MMDevice's list for a few tens of milliseconds, and a plan built
    // from the earlier snapshot reports "no render endpoints exist at all" about endpoints this
    // same pass just created.
    super::minted::ensure_provisioned();
    let inputs = PlanInputs::read();
    let (renders, captures) = (&inputs.renders, &inputs.captures);
    let fingerprint = wiring_plan::fingerprint(renders, captures);
    // Mix formats only when parking defaults. The idle mic pump does not care which
    // loopback wins and must not activate an IAudioClient per render on every pass.
    let probe: &dyn Fn(&Endpoint) -> Option<MixFormat> = if park_defaults {
        &mix_format_of
    } else {
        &wiring_plan::no_formats
    };
    let wiring = inputs.plan(probe);
    let done = |wiring: Wiring| WiredPlan {
        wiring,
        fingerprint,
        renders: renders.clone(),
    };

    let changed = {
        let mut last = LAST_WIRING.lock().unwrap();
        let changed = last.as_ref() != Some(&wiring);
        *last = Some(wiring.clone());
        changed
    };
    if changed {
        tracing::info!(
            mic_render = wiring.mic_render.as_ref().map(|(n, _)| n.as_str()),
            mic_capture = wiring.mic_capture.as_ref().map(|(n, _)| n.as_str()),
            loopback_render = wiring.loopback_render.as_ref().map(|(n, _)| n.as_str()),
            loopback_last_resort = wiring.loopback_last_resort,
            mic_withheld = wiring.mic_withheld,
            readiness = ?wiring_plan::readiness(&wiring),
            renders = ?renders.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            "audio wiring plan"
        );
        if let (Some(why), Some((name, _))) = (&wiring.loopback_narrowing, &wiring.loopback_render)
        {
            tracing::warn!(
                device = %name,
                "the desktop-audio loopback endpoint {why} — streamed audio will sound worse \
                 than it does on the host. Attach or select a 48 kHz stereo output device, or \
                 set audio.output_mode = host_and_client (PUNKTFUNK_HOST_AUDIO=1) to prefer \
                 real hardware"
            );
        }
        if wiring.mic_render.is_some() && wiring.loopback_unsatisfiable() {
            // Per-endpoint reasons plus only unused remedies — static "install Steam" advice
            // is wrong when the Microphone half is already the mic reservation.
            tracing::warn!(
                "desktop audio unavailable: {}",
                wiring_plan::describe_no_loopback(renders, &wiring)
            );
        }
    }

    if keep_default_devices() {
        if changed {
            tracing::info!(
                mode = %pf_host_config::config().audio_output_mode.as_str(),
                session_asked = crate::capture_policy::session_keeps_default(),
                "leaving the audio default devices untouched (follow_default mode, or a \
                 session's keep-host-audio ask)"
            );
        }
        return done(wiring);
    }
    // If the default render is the mic target, apps render into the virtual mic.
    // Move it first: a parked `prev` that is the cable would restore that after a stream.
    if let Some((mic_name, mic_id)) = &wiring.mic_render {
        if default_render_id().as_deref() == Some(mic_id.as_str()) {
            // host_audio plan: real hardware first, so the new default is audible.
            match plan(
                renders,
                captures,
                inputs.mic_want.as_deref(),
                true,
                &inputs.pad_ids,
                &inputs.minted,
            )
            .loopback_render
            {
                Some((name, id)) => match set_default_endpoint(&id) {
                    Ok(()) => tracing::info!(mic = %mic_name, device = %name,
                        "default playback was the virtual-mic target — moved it so desktop \
                         audio no longer feeds the mic"),
                    Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                        "move the default playback off the virtual-mic target"),
                },
                None => {
                    if changed {
                        tracing::warn!(mic = %mic_name,
                            "default playback is the virtual-mic target and no other usable \
                             render endpoint exists — desktop audio will feed the mic");
                    }
                }
            }
        }
    }
    // Idle, nothing parked: default on the virtual mic moves to a real microphone.
    // Session park cannot heal this — it only remembers a prev that is not already ours.
    if !park_defaults && RECORDING.parked.lock().unwrap().is_none() {
        if let Some((mic_name, mic_id)) = &wiring.mic_capture {
            if default_capture_id().as_deref() == Some(mic_id.as_str()) {
                if let Some((name, id)) = wiring_plan::real_capture(captures, Some(mic_id.as_str()))
                {
                    match set_default_endpoint(id) {
                        Ok(()) => tracing::info!(from = %mic_name, device = %name,
                            "default recording was left on the virtual mic outside a stream — \
                             moved it back to a real microphone"),
                        Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                            "move the default recording off the virtual mic"),
                    }
                }
            }
        }
    }
    if park_defaults {
        if let Some((name, id)) = &wiring.loopback_render {
            let mic_id = wiring.mic_render.as_ref().map(|(_, m)| m.as_str());
            park_default_playback(name, id, changed, mic_id);
        }
        // Recording park is session-scoped: idle park hands the default mic (and, via
        // eCommunications, in-game voice) to a virtual mic nothing feeds.
        if let Some((name, id)) = &wiring.mic_capture {
            park_default_recording(name, id, changed);
        }
    }
    done(wiring)
}

/// A default device parked for the capture's life: `(previous_id, id_we_set)` in memory,
/// mirrored to a crash marker (two lines, previous id then set id) so a crash cannot leave
/// the box parked. Each caller keeps its own write policy for the park itself.
struct DefaultSlot {
    parked: Mutex<Option<(String, String)>>,
    /// File name under `pf_paths::config_dir()`.
    marker: &'static str,
    current: fn() -> Option<String>,
    what: &'static str,
}

static PLAYBACK: DefaultSlot = DefaultSlot {
    parked: Mutex::new(None),
    marker: "audio-default.prev",
    current: default_render_id,
    what: "playback",
};

static RECORDING: DefaultSlot = DefaultSlot {
    parked: Mutex::new(None),
    marker: "audio-default-rec.prev",
    current: default_capture_id,
    what: "recording",
};

impl DefaultSlot {
    fn marker_path(&self) -> std::path::PathBuf {
        pf_paths::config_dir().join(self.marker)
    }

    /// Remember the operator default `cur` before parking on `id`. The first park stores it
    /// unless it is `never_prev`; a plan change mid-stream keeps it and updates only what we
    /// set. Nothing changes while `cur` already is `id`.
    fn remember(&self, cur: Option<&str>, id: &str, never_prev: Option<&str>) {
        if cur == Some(id) {
            return;
        }
        let mut parked = self.parked.lock().unwrap();
        match parked.as_mut() {
            None => {
                if let Some(prev) = cur.filter(|c| Some(*c) != never_prev) {
                    let _ = std::fs::write(self.marker_path(), format!("{prev}\n{id}"));
                    *parked = Some((prev.to_string(), id.to_string()));
                }
            }
            Some((prev, set)) if set != id => {
                let _ = std::fs::write(self.marker_path(), format!("{prev}\n{id}"));
                *set = id.to_string();
            }
            Some(_) => {}
        }
    }

    /// Put the remembered default back. No-op if never parked; an operator change
    /// mid-stream wins. COM-initialized thread (capture exit path).
    fn restore(&self) {
        let Some((prev, set)) = self.parked.lock().unwrap().take() else {
            return;
        };
        let _ = std::fs::remove_file(self.marker_path());
        if (self.current)().as_deref() != Some(set.as_str()) {
            return;
        }
        match set_default_endpoint(&prev) {
            Ok(()) => tracing::info!("default {} device restored after streaming", self.what),
            Err(e) => tracing::warn!(error = %format!("{e:#}"),
                "restore the default {} device after streaming", self.what),
        }
    }

    /// Consume the crash marker. Returns the previous id only if the current default is still
    /// the endpoint we set (an operator change since wins). The file is removed either way.
    fn take_marker(&self) -> Option<String> {
        let path = self.marker_path();
        let s = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        let mut lines = s.lines();
        let (prev, set) = (lines.next()?, lines.next()?);
        ((self.current)().as_deref() == Some(set)).then(|| prev.to_string())
    }
}

/// Current default render endpoint id. Pad-endpoint provisioning uses this so a
/// freshly minted pad speaker never stays the default playback device.
pub fn default_render_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Render)
        .ok()?
        .get_id()
        .ok()
}

/// Current default capture endpoint id. Read before asserting so an already-correct
/// default costs zero IPolicyConfig writes.
pub fn default_capture_id() -> Option<String> {
    wasapi::DeviceEnumerator::new()
        .ok()?
        .get_default_device(&Direction::Capture)
        .ok()?
        .get_id()
        .ok()
}

/// Once per process: restore a default a previous run left parked, only if it is still
/// the endpoint we set. Runs on the first wiring pass (mic pump at host start), not the
/// first stream.
fn recover_orphaned_default() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for slot in [&PLAYBACK, &RECORDING] {
            let Some(prev) = slot.take_marker() else {
                continue;
            };
            let what = slot.what;
            match set_default_endpoint(&prev) {
                Ok(()) => tracing::info!(
                    "restored the default {what} device a previous host run left parked"
                ),
                Err(e) => tracing::warn!(error = %format!("{e:#}"),
                    "restore the default {what} device left by a previous run"),
            }
        }
        // Same idea for the per-app voice-chat pins a crash left behind.
        super::voice_route::recover_orphaned();
    });
}

/// Uninstall twin of [`recover_orphaned_default`]: restore if still parked on ours,
/// then always drop the marker (no next host run consumes it). No `Once` — the
/// uninstaller is a fresh process. Must run before the devnode sweep: Windows would
/// otherwise re-pick by its own ranking, not the operator's pre-park device.
///
/// Returns whether a device was actually put back.
pub fn unpark_default_for_uninstall() -> bool {
    let mut restored = false;
    for slot in [&PLAYBACK, &RECORDING] {
        if let Some(prev) = slot.take_marker() {
            restored |= set_default_endpoint(&prev).is_ok();
        }
    }
    restored
}

/// Park playback on `id` for the capture's life, remembering the operator default first.
/// The mic target is never stored as `prev` — restoring it would feed the virtual mic.
/// Guards the race the hygiene pass in [`wire_now`] usually already closed.
fn park_default_playback(name: &str, id: &str, changed: bool, mic_id: Option<&str>) {
    PLAYBACK.remember(default_render_id().as_deref(), id, mic_id);
    match set_default_endpoint(id) {
        Ok(()) => {
            if changed {
                tracing::info!(device = %name,
                    "audio wiring: default playback = desktop-audio loopback source");
            }
        }
        Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
            "audio wiring: set the default playback device"),
    }
}

/// Park recording on `id` for the capture's life, remembering the operator default first.
fn park_default_recording(name: &str, id: &str, changed: bool) {
    let cur = default_capture_id();
    RECORDING.remember(cur.as_deref(), id, None);
    // `set_default_endpoint` is not a no-op on an unchanged default: it fires
    // SetDefaultEndpoint for all three roles. Write only when the plan changed or
    // the default drifted, or the policy store churns on every reopen.
    if changed || cur.as_deref() != Some(id) {
        match set_default_endpoint(id) {
            Ok(()) => {
                if changed {
                    tracing::info!(device = %name,
                        "audio wiring: default recording = virtual mic (apps record the client's mic)");
                }
            }
            Err(e) => tracing::warn!(device = %name, error = %format!("{e:#}"),
                "audio wiring: set the default recording device"),
        }
    }
}

/// Re-set the default playback to the endpoint we are already capturing, without a
/// wiring pass. One `IPolicyConfig` write: the capture is bound explicitly, so a
/// hijacked default only moves where apps render. Does not touch [`PLAYBACK`] — the
/// operator's original default is still owed back at stream end.
pub fn reassert_default_playback(id: &str) -> bool {
    match set_default_endpoint(id) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "re-assert the default playback device");
            false
        }
    }
}

/// Inverse of [`park_default_playback`]: see [`DefaultSlot::restore`].
pub fn restore_default_playback() {
    PLAYBACK.restore();
}

/// Inverse of [`park_default_recording`]: see [`DefaultSlot::restore`].
pub fn restore_default_recording() {
    RECORDING.restore();
}

/// Endpoints reshaped for the capture's life: `(id, channels before, rate_hz)`. Each
/// endpoint's first reshape wins; a reopen onto another sink adds it. Memory only: after a
/// crash, the next capture's reshape corrects the count.
static RESHAPED: Mutex<Vec<(String, u16, u32)>> = Mutex::new(Vec::new());

/// Give a render endpoint `channels` until [`restore_endpoint_channels`]. `from` is its count now.
pub fn reshape_endpoint(id: &str, from: u16, channels: u16, rate_hz: u32) -> Result<()> {
    set_endpoint_channels(id, channels, rate_hz)?;
    let mut reshaped = RESHAPED.lock().unwrap();
    if !reshaped.iter().any(|(r, ..)| r == id) {
        reshaped.push((id.to_string(), from, rate_hz));
    }
    Ok(())
}

/// Inverse of [`reshape_endpoint`] for every endpoint reshaped. Capture exit path.
pub fn restore_endpoint_channels() {
    let reshaped = std::mem::take(&mut *RESHAPED.lock().unwrap());
    for (id, channels, rate_hz) in reshaped {
        match set_endpoint_channels(&id, channels, rate_hz) {
            Ok(()) => tracing::info!(
                channels,
                "desktop-audio sink speaker layout restored after streaming"
            ),
            Err(e) => tracing::warn!(error = %format!("{e:#}"),
                "restore the desktop-audio sink speaker layout after streaming"),
        }
    }
}

/// Open by endpoint id. Goes through [`super::pad_endpoint::open_wasapi_device`]
/// so every caller shares one resolution path (see that helper).
pub fn open_endpoint(ep: &Endpoint) -> Result<wasapi::Device> {
    super::pad_endpoint::open_wasapi_device(&ep.1)
        .map_err(|e| anyhow!("open endpoint {:?}: {e:#}", ep.0))
}

// Undocumented IPolicyConfig: default-endpoint, endpoint-visibility and device-format writes.

/// `IPolicyConfig` vtable. The placeholder arrays hold the methods between the called
/// ones so the slot offsets stay correct.
#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface: unsafe extern "system" fn(
        *mut c_void,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    /// GetMixFormat, GetDeviceFormat, ResetDeviceFormat.
    _reserved_a: [*const c_void; 3],
    /// `(id, endpoint WAVEFORMATEX*, mix WAVEFORMATEX*)`. `c_void`: `wasapi` builds the formats
    /// against its own `windows` version.
    set_device_format: unsafe extern "system" fn(
        *mut c_void,
        windows::core::PCWSTR,
        *const c_void,
        *const c_void,
    ) -> windows::core::HRESULT,
    /// Get/SetProcessingPeriod, Get/SetShareMode, Get/SetPropertyValue.
    _reserved_b: [*const c_void; 6],
    set_default_endpoint: unsafe extern "system" fn(
        *mut c_void,
        windows::core::PCWSTR,
        u32,
    ) -> windows::core::HRESULT,
    set_endpoint_visibility: unsafe extern "system" fn(
        *mut c_void,
        windows::core::PCWSTR,
        i32,
    ) -> windows::core::HRESULT,
}

// Mirrors undocumented `IPolicyConfig`: there is no header. Calls go by slot
// index, so a field added above `set_default_endpoint` compiles and invokes a
// different function. These asserts pin the slot indexes and the table size.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *const c_void;
    // 3 IUnknown slots, then GetMixFormat..ResetDeviceFormat: `set_device_format` is slot 6
    // (0-based), `set_default_endpoint` 13, `set_endpoint_visibility` 14.
    assert!(offset_of!(IPolicyConfigVtbl, query_interface) == 0);
    assert!(offset_of!(IPolicyConfigVtbl, add_ref) == size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, release) == 2 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, _reserved_a) == 3 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_device_format) == 6 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, _reserved_b) == 7 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_default_endpoint) == 13 * size_of::<P>());
    assert!(offset_of!(IPolicyConfigVtbl, set_endpoint_visibility) == 14 * size_of::<P>());
    assert!(size_of::<IPolicyConfigVtbl>() == 15 * size_of::<P>());
};

/// A live `IPolicyConfig`; the `IUnknown` inside releases it on drop.
#[repr(transparent)]
#[derive(Clone)]
struct PolicyConfig(windows::core::IUnknown);

// SAFETY: one COM pointer (transparent over `IUnknown`) to an object whose table starts with
// `IPolicyConfigVtbl`, the layout the asserts above pin.
unsafe impl windows::core::Interface for PolicyConfig {
    type Vtable = IPolicyConfigVtbl;
    /// IPolicyConfig, Windows 7 and later.
    const IID: windows::core::GUID =
        windows::core::GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
}

/// Run `f` with a live `IPolicyConfig` and a NUL-terminated UTF-16 `device_id`.
fn with_policy_config<R>(
    device_id: &str,
    f: impl FnOnce(&PolicyConfig, windows::core::PCWSTR) -> R,
) -> Result<R> {
    use windows::core::{IUnknown, Interface, GUID, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

    // PolicyConfigClient coclass.
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    let wide = HSTRING::from(device_id);
    // SAFETY: a plain activation by static CLSID; the result is an owned IUnknown.
    let unk: IUnknown = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) }
        .map_err(|e| anyhow!("CoCreateInstance(PolicyConfig): {e}"))?;
    let policy: PolicyConfig = unk
        .cast()
        .map_err(|e| anyhow!("QueryInterface(IPolicyConfig): {e}"))?;
    Ok(f(&policy, PCWSTR(wide.as_ptr())))
}

/// Set `device_id` as default for eConsole/eMultimedia/eCommunications via
/// `IPolicyConfig::SetDefaultEndpoint`. Errs if any role fails.
pub fn set_default_endpoint(device_id: &str) -> Result<()> {
    use windows::core::Interface;
    with_policy_config(device_id, |pc, id| {
        let mut result = Ok(());
        for role in 0u32..=2 {
            // SAFETY: live IPolicyConfig from `with_policy_config`; in-range ERole.
            let hr = unsafe { (pc.vtable().set_default_endpoint)(pc.as_raw(), id, role) };
            if hr.is_err() {
                result = hr
                    .ok()
                    .map_err(|e| anyhow!("SetDefaultEndpoint(role {role}): {e}"));
            }
        }
        result
    })?
}

/// Show or hide an endpoint via `IPolicyConfig::SetEndpointVisibility`. Hidden
/// means `DEVICE_STATE_DISABLED`: gone from ACTIVE enumeration, cannot be opened,
/// but the devnode, driver, and stamped identity stay — showing it again is not
/// a PnP reinstall. Pad-endpoint provider hides the idle DualSense speaker so
/// libScePad titles do not take the haptics path against an unserviced endpoint.
pub fn set_endpoint_visibility(device_id: &str, visible: bool) -> Result<()> {
    use windows::core::Interface;
    with_policy_config(device_id, |pc, id| {
        // SAFETY: live IPolicyConfig from `with_policy_config`; INT bool.
        let hr = unsafe { (pc.vtable().set_endpoint_visibility)(pc.as_raw(), id, visible as i32) };
        hr.ok()
            .map_err(|e| anyhow!("SetEndpointVisibility({visible}): {e}"))
    })?
}

/// Set a render endpoint's speaker layout via `IPolicyConfig::SetDeviceFormat`, the write
/// behind Windows' speaker setup. Tries device formats in Sunshine's order, first accepted
/// wins. The zeroed mix format lets the engine derive its own from the device format.
fn set_endpoint_channels(device_id: &str, channels: u16, rate_hz: u32) -> Result<()> {
    let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
    // 5.1 drivers split between back (0x3F) and side (0x60F) surrounds.
    let masks: &[u32] = if channels == 6 {
        &[mask, 0x60F]
    } else {
        &[mask]
    };
    // (store bits, valid bits, type): 24-in-32, 24, 16, f32, 32 — Sunshine's order.
    let samples = [
        (32, 24, wasapi::SampleType::Int),
        (24, 24, wasapi::SampleType::Int),
        (16, 16, wasapi::SampleType::Int),
        (32, 32, wasapi::SampleType::Float),
        (32, 32, wasapi::SampleType::Int),
    ];
    set_endpoint_format(device_id, channels, rate_hz, masks, &samples)
}

/// `IPolicyConfig::SetDeviceFormat` with the first of `samples` (store bits, valid bits,
/// type) the driver takes, per mask. The write replaces the endpoint's whole stored format
/// set, and the driver validates it, so `Err` means it has no `channels`-channel mode at all.
pub fn set_endpoint_format(
    device_id: &str,
    channels: u16,
    rate_hz: u32,
    masks: &[u32],
    samples: &[(usize, usize, wasapi::SampleType)],
) -> Result<()> {
    use wasapi::WaveFormat;
    use windows::core::Interface;
    // WAVEFORMATEXTENSIBLE is 40 bytes, packed.
    let mix = [0u8; 40];
    with_policy_config(device_id, |pc, id| {
        let mut last = None;
        for &m in masks {
            for (store, valid, ty) in samples {
                let wave = WaveFormat::new(
                    *store,
                    *valid,
                    ty,
                    rate_hz as usize,
                    channels.into(),
                    Some(m),
                );
                let fmt = std::ptr::from_ref(wave.as_waveformatex_ref()).cast();
                // SAFETY: live IPolicyConfig from `with_policy_config`; `fmt` points at `wave` (a
                // full WAVEFORMATEXTENSIBLE, cbSize 22), `mix` at 40 bytes; both outlive the call.
                let hr = unsafe {
                    (pc.vtable().set_device_format)(pc.as_raw(), id, fmt, mix.as_ptr().cast())
                };
                match hr.ok() {
                    Ok(()) => return Ok(()),
                    Err(e) => last = Some(e),
                }
            }
        }
        Err(anyhow!(
            "SetDeviceFormat({channels} ch, {rate_hz} Hz): {}",
            last.unwrap()
        ))
    })?
}
