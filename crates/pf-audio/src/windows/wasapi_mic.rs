//! WASAPI virtual microphone: write client-decoded PCM into an existing virtual
//! device's **render** endpoint so its **capture** side is a host-recordable mic.
//! Windows has no user-mode way to create a capture endpoint.
//!
//! Target is [`audio_control::wire_now`] on every open; `PUNKTFUNK_MIC_DEVICE`
//! overrides. The plan keeps this write off the desktop-audio loopback endpoint
//! so inject cannot echo ([`wiring_plan`](super::wiring_plan)). Missing
//! candidates auto-install the Steam Streaming pair
//! ([`install_steam_audio_pair`]); else the pump retries.
//!
//! `push` feeds a bounded drop-oldest ring. A COM-apartment thread (`!Send`
//! WASAPI objects live only there) event-renders through a prime→hold→re-prime
//! jitter buffer whose depth the pump sets ([`VirtualMic::set_target_depth`]).
//! Any WASAPI error exits the thread, `alive` goes false, and the pump reopens.
//!
//! A running stream holds a kernel power request that vetoes sleep. After
//! [`IDLE_STOP_AFTER`] of silence the loop `IAudioClient::Stop`s (endpoint
//! stays; only the request is released) and parks on the queue condvar.
//! `PUNKTFUNK_MIC_ALWAYS_ON=1` never idle-stops.

use super::{audio_control, MicBackendStats, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

const CHANNELS: u32 = 2;
const BLOCK_ALIGN: usize = 2 * 4;
/// Fallback prime (~48 ms) until the pump's first jitter estimate, and forever
/// under `PUNKTFUNK_MIC_LEGACY_BUFFER=1`. Covers a ~42 ms inter-burst gap so
/// WASAPI's ~10 ms pull does not insert mid-stream silence.
const PRIME_BYTES: usize = (SAMPLE_RATE as usize * 48 / 1000) * BLOCK_ALIGN;
/// Legacy inject-queue cap (~120 ms, drop oldest): prime plus burst headroom,
/// used only while the pump is not driving the target.
const MAX_QUEUE_BYTES: usize = (SAMPLE_RATE as usize * 120 / 1000) * BLOCK_ALIGN;
/// Overflow headroom (~32 ms) above the render loop's prime when the adaptive
/// target drives the ring.
const CAP_HEADROOM_BYTES: usize = (SAMPLE_RATE as usize * 32 / 1000) * BLOCK_ALIGN;
/// Silence-only output this long stops the render stream so an idle host can
/// sleep (a running WASAPI stream holds a power request). Longer than a talk
/// pause; resume is one condvar wake + `IAudioClient::Start`.
const IDLE_STOP_AFTER: Duration = Duration::from_secs(10);
/// Condvar wait while idle-stopped; bounds how long a host-shutdown `stop` can
/// sit unnoticed (same as the pump's `drain_sleep`).
const IDLE_WAKE_CHECK: Duration = Duration::from_millis(250);

/// Inject ring plus the idle-stop wake: `push` notifies empty→non-empty, which
/// is when a stopped stream must resume.
type MicQueue = (Mutex<VecDeque<u8>>, Condvar);

/// `PUNKTFUNK_MIC_ALWAYS_ON=1`: never idle-stop. Escape hatch if a virtual
/// driver's capture side misbehaves while its render side is paused.
fn mic_always_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_MIC_ALWAYS_ON") == Some(true))
}

pub struct WasapiVirtualMic {
    queue: Arc<MicQueue>,
    stop: Arc<AtomicBool>,
    /// False once the render thread has exited — the pump's reopen signal.
    alive: Arc<AtomicBool>,
    ring: Arc<RingShared>,
    join: Option<JoinHandle<()>>,
}

/// Pump handle ↔ render thread: de-jitter target in, effective prime +
/// reset-on-read counters out. All `Relaxed` — telemetry, not a barrier.
#[derive(Default)]
struct RingShared {
    /// Pump-set jitter target in bytes. `0` = no estimate yet → fixed
    /// [`PRIME_BYTES`] and [`MAX_QUEUE_BYTES`].
    target_bytes: AtomicUsize,
    /// Prime threshold (bytes) published by the last render iteration.
    prime_bytes: AtomicUsize,
    /// Full-drain re-prime arms (see [`MicBackendStats`]).
    reprimes: AtomicU64,
    /// Per-channel samples dropped by the overflow cap.
    overflow: AtomicU64,
}

impl WasapiVirtualMic {
    pub fn open(channels: u32) -> Result<Self> {
        anyhow::ensure!(
            channels == CHANNELS,
            "virtual mic is stereo-only (got {channels})"
        );
        let queue = Arc::new((Mutex::new(VecDeque::<u8>::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let ring = Arc::new(RingShared::default());
        let (q, st, rg, al) = (queue.clone(), stop.clone(), ring.clone(), alive.clone());
        let (name, join) = crate::ready::spawn_ready(
            "punktfunk-wasapi-mic",
            Duration::from_secs(5),
            move |ready| {
                if let Err(e) = render_thread(q, st, rg, ready) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi virtual-mic thread failed");
                }
                // Drop and device error both: this instance is done; the pump reopens.
                al.store(false, Ordering::Release);
            },
            |join| {
                // No `WasapiVirtualMic` owns the thread here, so no `Drop` stops it: unset, it
                // holds its render client forever.
                stop.store(true, Ordering::SeqCst);
                crate::ready::reap_timed_out("wasapi virtual-mic", join)
            },
        )?;
        tracing::info!(device = %name,
            "WASAPI virtual mic ready (client mic → this device's render endpoint)");
        Ok(WasapiVirtualMic {
            queue,
            stop,
            alive,
            ring,
            join: Some(join),
        })
    }
}

impl Drop for WasapiVirtualMic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl VirtualMic for WasapiVirtualMic {
    fn push(&self, pcm: &[f32]) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let (lock, wake) = &*self.queue;
        let Ok(mut q) = lock.lock() else {
            return false;
        };
        let was_empty = q.is_empty();
        q.reserve(pcm.len() * 4);
        for &s in pcm {
            q.extend(s.to_le_bytes());
        }
        // Drop-oldest: mic is real-time; stale is worse than a gap. Adaptive
        // bound is prime + headroom; else the fixed 120 ms.
        let cap = if self.ring.target_bytes.load(Ordering::Relaxed) == 0 {
            MAX_QUEUE_BYTES
        } else {
            // `max(PRIME_BYTES)` covers the one render period before first publish.
            self.ring
                .prime_bytes
                .load(Ordering::Relaxed)
                .max(PRIME_BYTES)
                + CAP_HEADROOM_BYTES
        };
        if q.len() > cap {
            let excess = q.len() - cap;
            q.drain(..excess);
            self.ring
                .overflow
                .fetch_add((excess / BLOCK_ALIGN) as u64, Ordering::Relaxed);
        }
        drop(q);
        if was_empty {
            // Resume signal for an idle-stopped stream. Waiter re-checks under
            // the lock, so this cannot be a lost wakeup.
            wake.notify_one();
        }
        true
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn discard(&self) {
        if let Ok(mut q) = self.queue.0.lock() {
            q.clear();
        }
    }

    fn channels(&self) -> u32 {
        CHANNELS
    }

    fn set_target_depth(&self, samples_per_ch: usize) {
        self.ring
            .target_bytes
            .store(samples_per_ch * BLOCK_ALIGN, Ordering::Relaxed);
    }

    fn depth(&self) -> Option<(usize, usize)> {
        let prime = self.ring.prime_bytes.load(Ordering::Relaxed);
        if prime == 0 {
            return None; // render loop hasn't published a prime yet
        }
        let q = self.queue.0.lock().ok()?;
        Some((q.len() / BLOCK_ALIGN, prime / BLOCK_ALIGN))
    }

    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats {
            reprimes: self.ring.reprimes.swap(0, Ordering::Relaxed),
            overflow_dropped: self.ring.overflow.swap(0, Ordering::Relaxed),
        }
    }
}

/// Resolve the mic inject target from the wiring plan, auto-installing the
/// Steam Streaming pair when nothing usable exists (then re-planning). Runs
/// on the COM-initialized render thread.
/// The device, its name, and whether it is the minted mic (whose pins [`repair_pins`] owns).
fn resolve_target() -> Result<(wasapi::Device, String, bool)> {
    // Endpoints must exist before this open: the pump holds one device for its
    // life, so racing provision here latches the cable while later plans pair
    // default recording with a minted mic nothing writes into.
    super::minted::ensure_blocking();
    // park_defaults=false: the idle mic pump must not park the operator's
    // playback/recording defaults; only desktop-audio capture may.
    let mut wiring = audio_control::wire_now(false);
    if wiring.mic_render.is_none() && !wiring.mic_withheld {
        // Withheld: the Streaming Microphone exists and the plan gave it to
        // loopback. Reinstalling the pair changes nothing and costs a 5 s settle.
        tracing::info!("no usable virtual mic device present — attempting auto-install");
        if install_steam_audio_pair() {
            wiring = audio_control::wire_now(false);
        }
    }
    let Some(ep) = wiring.mic_render else {
        if wiring.mic_withheld {
            anyhow::bail!(
                "the Steam Streaming Microphone is carrying desktop audio (game audio outranks \
                 the mic; taking it would have silenced the stream) — install VB-Audio Virtual \
                 Cable to give the mic its own device, or set PUNKTFUNK_MIC_DEVICE=<friendly-name \
                 substring> to force a target."
            );
        }
        anyhow::bail!(
            "no virtual-mic render endpoint on this box. Install Steam (the host mints its own \
             microphone endpoint from Steam's streaming drivers — Steam never needs to run), or \
             install VB-Audio Virtual Cable, or set PUNKTFUNK_MIC_DEVICE=<friendly-name \
             substring>."
        );
    };
    let minted = super::minted::minted_ids().mic_render.as_deref() == Some(ep.1.as_str());
    repair_pins(minted);
    let name = ep.0.clone();
    Ok((audio_control::open_endpoint(&ep)?, name, minted))
}

/// Before the stream opens or resumes: a pin set to another format since only turns the mic
/// into noise once audio flows, and a capture pin's change never invalidates this stream.
fn repair_pins(minted: bool) {
    if minted {
        super::minted::repair_mic_formats();
    }
}

/// Best-effort install of both Steam Streaming driver INFs so mic inject and
/// desktop-audio loopback can land on different devices (sharing one is an
/// echo; [`super::wiring_plan`]). Microphone first (inject target), speakers
/// second (loopback / silent sink). Returns true if either installed. No-op
/// when the INFs are absent, install is denied (needs admin; host is SYSTEM),
/// or `PUNKTFUNK_NO_MIC_INSTALL=1`. [`super::wasapi_cap`] installs the
/// same pair when no silent sink exists.
pub fn install_steam_audio_pair() -> bool {
    let mic = try_install_steam_audio("SteamStreamingMicrophone.inf");
    let spk = try_install_steam_audio("SteamStreamingSpeakers.inf");
    mic || spk
}

/// The capture side's install latch: `None` until it tries, then whether the INFs existed.
static TRIED_WITH_INFS: Mutex<Option<bool>> = Mutex::new(None);

/// Whether desktop-audio capture may try [`install_steam_audio_pair`] now, marking the try.
/// Once per INF state, not once per process: a try made while Steam was absent re-arms when
/// the INFs appear. Those files are invisible to the endpoint-set fingerprint, so nothing
/// else retries.
pub(super) fn steam_pair_install_due() -> bool {
    let infs = steam_infs_present();
    let mut tried = TRIED_WITH_INFS.lock().unwrap();
    let go = match *tried {
        None => true,
        Some(had_infs) => !had_infs && infs,
    };
    if go {
        *tried = Some(infs);
    }
    go
}

/// NUL-terminated UTF-16 path of a Steam Remote Play INF under
/// `%CommonProgramFiles(x86)%\Steam\drivers\Windows10\{arch}\`. Shared with
/// [`super::pad_endpoint`] (`UpdateDriverForPlugAndPlayDevicesW` when no
/// installed Speakers devnode exposes `oemNN.inf`). `None` if expansion fails.
pub fn steam_driver_inf_path(inf_name: &str) -> Option<Vec<u16>> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;

    #[cfg(target_arch = "x86_64")]
    let subdir = "x64";
    #[cfg(target_arch = "aarch64")]
    let subdir = "arm64";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let subdir = "x86";
    let template = HSTRING::from(format!(
        "%CommonProgramFiles(x86)%\\Steam\\drivers\\Windows10\\{subdir}\\{inf_name}"
    ));
    let mut path = vec![0u16; 1024];
    // SAFETY: `template` is a locally built NUL-terminated UTF-16 buffer that outlives the call, and
    // the output slice is a live local whose length the callee is told via the slice itself.
    let n =
        unsafe { ExpandEnvironmentStringsW(PCWSTR(template.as_ptr()), Some(path.as_mut_slice())) };
    if n == 0 || n as usize > path.len() {
        return None;
    }
    path.truncate(n as usize); // keeps the NUL
    Some(path)
}

/// Whether Steam's streaming-audio INFs exist: the state [`steam_pair_install_due`] keys on.
pub fn steam_infs_present() -> bool {
    use std::os::windows::ffi::OsStringExt;
    ["SteamStreamingMicrophone.inf", "SteamStreamingSpeakers.inf"]
        .iter()
        .any(|inf| {
            steam_driver_inf_path(inf).is_some_and(|wide| {
                // Drop the trailing NUL the FFI callers need; `exists` wants the bare path.
                let len = wide.len().saturating_sub(1);
                std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide[..len])).exists()
            })
        })
}

/// Install one Steam Streaming INF via `DiInstallDriverW`. `inf_name` is a bare
/// filename under Steam's per-arch `drivers\Windows10\{arch}\`.
///
/// Safe: `inf_name` is `&str` and every FFI argument is built locally — no
/// caller precondition.
fn try_install_steam_audio(inf_name: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DiInstallDriverW, DIINSTALLDRIVER_FLAGS,
    };

    if pf_host_config::env_on("PUNKTFUNK_NO_MIC_INSTALL") == Some(true) {
        return false;
    }
    let Some(path) = steam_driver_inf_path(inf_name) else {
        return false;
    };

    // SAFETY: `path` is the expanded, NUL-terminated buffer above and outlives the call; a null
    // parent HWND and a null `NeedReboot` are both documented as accepted.
    let installed =
        unsafe { DiInstallDriverW(None, PCWSTR(path.as_ptr()), DIINSTALLDRIVER_FLAGS(0), None) };
    match installed {
        Ok(()) => {
            tracing::info!(
                inf = inf_name,
                "installed a Steam Streaming virtual audio device"
            );
            std::thread::sleep(Duration::from_secs(5)); // let the audio subsystem register the endpoint
            true
        }
        Err(e) => {
            tracing::info!(
                inf = inf_name,
                error = %e,
                "Steam-audio device not auto-installed (Steam absent / not admin) — see install guidance"
            );
            false
        }
    }
}

fn render_thread(
    queue: Arc<MicQueue>,
    stop: Arc<AtomicBool>,
    shared: Arc<RingShared>,
    ready: SyncSender<Result<String>>,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Build WASAPI objects here so they outlive the loop; a returning closure
    // would drop them. Failure reports Err and exits.
    let setup = (|| -> Result<(wasapi::AudioClient, wasapi::AudioRenderClient, wasapi::Handle, i64, String, bool)> {
        let (device, name, minted) = resolve_target()?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // Autoconvert: WASAPI shared-mode SRC matches the device mix format.
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS as usize,
            None,
        );
        let (default_period, _min) = audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        // Silence-fill so the stream starts without a glitch.
        let buf_frames = audio_client.get_buffer_size().context("buffer size")? as usize;
        let _ = render_client.write_to_device(buf_frames, &vec![0u8; buf_frames * BLOCK_ALIGN], None);
        audio_client.start_stream().context("start render stream")?;
        Ok((audio_client, render_client, h_event, default_period, name, minted))
    })();
    let (audio_client, render_client, h_event, default_period, name, minted) = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(name));
    // Device period in bytes (period is 100 ns units; floor 10 ms if it reads
    // absurd) — pull granularity the adaptive prime builds on.
    let period_bytes = ((default_period.max(0) as usize * SAMPLE_RATE as usize / 10_000_000)
        .max(SAMPLE_RATE as usize / 100))
        * BLOCK_ALIGN;

    // Prime→hold→re-prime: clients burst on their clock, WASAPI pulls per
    // period on another. Greedy drain pads mid-stream silence. Re-prime only
    // after a full drain. Threshold = period + pump target; [`PRIME_BYTES`]
    // until the first estimate, forever under `PUNKTFUNK_MIC_LEGACY_BUFFER=1`.
    let mut buf: Vec<u8> = Vec::new();
    let mut primed = false;
    // `running` is Start/Stop. `idle_mark` is (queue len, first seen at that
    // len): IDLE_STOP_AFTER unprimed at an UNCHANGED length is idle. Length
    // not emptiness covers a sub-prime tail; a real push always moves len
    // (drop-oldest sits above prime, so an unprimed queue cannot return).
    let always_on = mic_always_on();
    let mut running = true;
    let mut idle_mark: Option<(usize, Instant)> = None;
    'render: while !stop.load(Ordering::Relaxed) {
        if !running {
            // Park until push notifies empty→non-empty; timeout keeps `stop` live.
            {
                let (lock, wake) = &*queue;
                let mut q = lock.lock().unwrap();
                while q.is_empty() {
                    if stop.load(Ordering::Relaxed) {
                        break 'render;
                    }
                    let (guard, _timed_out) = wake.wait_timeout(q, IDLE_WAKE_CHECK).unwrap();
                    q = guard;
                }
            }
            // Endpoint died while stopped — same path as any death: exit, pump reopens.
            repair_pins(minted);
            audio_client
                .start_stream()
                .context("resume render stream")?;
            running = true;
            idle_mark = None;
            tracing::debug!("virtual mic stream resumed (client mic audio arrived)");
        }
        // Finite timeout keeps `stop` responsive.
        if h_event.wait_for_event(100).is_err() {
            continue;
        }
        let space = audio_client
            .get_available_space_in_frames()
            .context("available space")? as usize;
        if space == 0 {
            continue;
        }
        let need = space * BLOCK_ALIGN;
        if buf.len() < need {
            buf.resize(need, 0);
        }
        let target = shared.target_bytes.load(Ordering::Relaxed);
        let prime = if target == 0 {
            PRIME_BYTES
        } else {
            period_bytes + target
        };
        shared.prime_bytes.store(prime, Ordering::Relaxed);
        buf[..need].fill(0);
        {
            let mut q = queue.0.lock().unwrap();
            if !primed && q.len() >= prime {
                primed = true;
            }
            if primed {
                let n = q.len().min(need);
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                if q.is_empty() {
                    primed = false; // fully drained — re-prime before producing again
                    shared.reprimes.fetch_add(1, Ordering::Relaxed);
                }
            }
            if primed {
                idle_mark = None;
            } else if !always_on {
                // Unprimed = this period was silence. IDLE_STOP_AFTER at an
                // unchanged length stops the stream. A burst at the boundary
                // moves len and resets. The ≥10 s stale tail is cleared: the
                // pump would discard it anyway, so wait's "empty" ⇔ "nothing".
                match idle_mark {
                    Some((len, since)) if len == q.len() => {
                        if since.elapsed() >= IDLE_STOP_AFTER {
                            q.clear();
                            audio_client
                                .stop_stream()
                                .context("idle-stop render stream")?;
                            running = false;
                            idle_mark = None;
                            tracing::debug!(
                                "virtual mic stream idle-stopped (releases the sleep-blocking \
                                 audio power request; next mic frame resumes it)"
                            );
                            continue 'render;
                        }
                    }
                    _ => idle_mark = Some((q.len(), Instant::now())),
                }
            }
        }
        render_client
            .write_to_device(space, &buf[..need], None)
            .context("write_to_device")?;
    }
    audio_client.stop_stream().ok();
    Ok(())
}
