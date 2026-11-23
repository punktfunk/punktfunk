//! macOS desktop-audio capture through a Core Audio process tap (macOS 14.2+).
//!
//! A private global stereo tap takes every process's output except this one and the
//! replayers: tools that play other apps' audio again from a process of their own, which a
//! tap holding both would carry twice. [`REPLAYERS`] are known ahead, and a [`Probe`] finds
//! any other by what it plays. A private aggregate device carries the tap, and an IOProc on it
//! hands interleaved `f32` to the capturer over a bounded channel (drop, never block the IO
//! thread).
//! The tap mixes at its own rate, which need not be the output device's; [`convert`] resamples
//! to the delivered rate and lays stereo into the session's layout. Client-only audio mutes the
//! Mac's speakers while the tap is read, the replayers through a second tap on the aggregate.
//!
//! Tap and aggregate are private to this process and do not outlive it. The mute holds only
//! while this process reads the tap, so a crash cannot leave the Mac silent, and nothing needs
//! restoring or healing at host start. The tap needs the System Audio Recording permission,
//! and the app bundle `NSAudioCaptureUsageDescription`. There is no public way to read that
//! grant, and a refused tap delivers digital silence, the same as a quiet Mac.

mod convert;

use crate::capture_policy::CaptureStats;
use crate::replay_detect::{Decimator, Judge, Verdict};
use crate::{wiring_plan, AudioCapturer, CaptureRate, VirtualMic};
use anyhow::{anyhow, Context, Result};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyProcessObjectList,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatLinearPCM, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// The rate of a tap built like the capturer's and never started. It is not the output
/// device's (a 192 kHz device can carry a 48 kHz tap), and [`convert::settle_rate`] never
/// delivers above it, so only `rate <= this` is honest.
pub(super) fn probe_capture_rate() -> CaptureRate {
    match Tap::global(CATapMuteBehavior::Unmuted, &[]).and_then(|tap| tap_format(tap.id)) {
        Ok(format) => CaptureRate::Engine(format.mSampleRate.round() as u32),
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "hi-res capture-rate probe: tap format unreadable");
            CaptureRate::Unknown
        }
    }
}

pub(super) fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(TapCapturer::open(channels, rate_hz)?))
}

/// The tap follows every process's output; there is no sink to name.
pub(super) fn open_audio_capture_named(
    channels: u32,
    rate_hz: u32,
    _sink: Option<&str>,
    _tap: bool,
) -> Result<Box<dyn AudioCapturer>> {
    open_audio_capture(channels, rate_hz)
}

pub(super) fn per_session_sink_possible() -> bool {
    false
}

/// Apps record a capture device, and only an audio server plug-in can add one.
pub(super) fn open_virtual_mic(_channels: u32) -> Result<Box<dyn VirtualMic>> {
    anyhow::bail!(
        "open virtual mic: macOS needs the Punktfunk audio server plug-in, which this host does \
         not install yet"
    )
}

pub(super) fn open_virtual_mic_named(
    channels: u32,
    _source: Option<&str>,
) -> Result<Box<dyn VirtualMic>> {
    open_virtual_mic(channels)
}

pub(super) fn wiring_snapshot() -> Option<wiring_plan::Wiring> {
    None
}

/// Bundle ids of the processes playing audio now, lowercased, without this host. A process
/// with no bundle id (a command-line tool) has no name to list.
pub(super) fn playing_apps() -> Result<Vec<String>> {
    let me = std::process::id() as i32;
    let mut apps = Vec::new();
    for obj in prop_list(SYSTEM, kAudioHardwarePropertyProcessObjectList)? {
        let playing = prop::<u32>(obj, kAudioProcessPropertyIsRunningOutput).unwrap_or(0) != 0;
        if !playing || prop::<i32>(obj, kAudioProcessPropertyPID).ok() == Some(me) {
            continue;
        }
        if let Some(id) = bundle_id(obj).filter(|id| !id.is_empty() && !apps.contains(id)) {
            apps.push(id);
        }
    }
    apps.sort();
    Ok(apps)
}

/// Desktop audio from a process tap. Fields drop in order: IO stops before the aggregate and
/// the tap are destroyed.
pub struct TapCapturer {
    chunks: Receiver<Vec<f32>>,
    /// Each chunk's buffer goes back to the IOProc once copied out.
    spare: SyncSender<Vec<f32>>,
    channels: u32,
    rate_hz: u32,
    /// Shared with the IOProc, so drops count only while a session reads.
    active: Arc<AtomicBool>,
    /// Any non-zero sample since open, for [`Self::note_silence`].
    heard: Arc<AtomicBool>,
    opened: Instant,
    silence_noted: bool,
    io: IoProc,
    _aggregate: Aggregate,
    tap: Tap,
    /// Mutes the replayers the tap leaves out; read on the aggregate, its samples never used.
    _silencer: Option<Tap>,
    tap_hz: f64,
    last_format_check: Instant,
    /// The process list at the last check, and the replayers in it the taps were built for.
    processes: Vec<AudioObjectID>,
    replayers: Vec<AudioObjectID>,
    /// One for each process that could be a replayer and is not known yet.
    probes: Vec<Probe>,
}

impl TapCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<TapCapturer> {
        anyhow::ensure!(
            matches!(channels, 2 | 6 | 8),
            "process tap backend supports 2/6/8 channels (got {channels})"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
        let mute = mute_behavior();
        let processes = process_list();
        let replayers = replayers_in(&processes);
        let tap = Tap::global(mute, &replayers)?;
        let silencer = (mute != CATapMuteBehavior::Unmuted && !replayers.is_empty())
            .then(|| Tap::only(&replayers, mute))
            .transpose()?;
        let format = tap_format(tap.id)?;
        let tap_hz = format.mSampleRate.round() as u32;
        let open_hz = convert::settle_rate(tap_hz, rate_hz);
        // The capture tap goes first: the IOProc reads the first two channels.
        let uids: Vec<&str> = std::iter::once(&tap)
            .chain(&silencer)
            .map(|t| t.uid.as_str())
            .collect();
        let aggregate = Aggregate::create(&uids)?;
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (spare, spare_rx) = sync_channel::<Vec<f32>>(64);
        let active = Arc::new(AtomicBool::new(true));
        let heard = Arc::new(AtomicBool::new(false));
        let state = IoState {
            tx,
            spare: spare_rx,
            active: active.clone(),
            heard: heard.clone(),
            channels: channels as usize,
            resampler: convert::Resampler::new(tap_hz, open_hz, 2)?,
            stereo: Vec::new(),
            resampled: Vec::new(),
            tap_hz,
            open_hz,
            stats: CaptureStats::default(),
            last_stats: Instant::now(),
            last_callback: None,
        };
        let io = IoProc::start(aggregate.id, state)?;
        tracing::info!(
            channels,
            requested_hz = rate_hz,
            opened_hz = open_hz,
            tap_hz,
            tap_ch = format.mChannelsPerFrame,
            muted = mute != CATapMuteBehavior::Unmuted,
            replayers_left_out = replayers.len(),
            "macOS process tap capturing"
        );
        if channels > 2 {
            tracing::info!(
                channels,
                "the process tap mixes to stereo — surround channels stay silent until a \
                 multichannel virtual output device exists"
            );
        }
        let mut capturer = TapCapturer {
            chunks: rx,
            spare,
            channels,
            rate_hz: open_hz,
            active,
            heard,
            opened: Instant::now(),
            silence_noted: false,
            io,
            _aggregate: aggregate,
            tap,
            _silencer: silencer,
            tap_hz: format.mSampleRate,
            last_format_check: Instant::now(),
            processes,
            replayers,
            probes: Vec::new(),
        };
        capturer.start_probes();
        Ok(capturer)
    }

    /// A replayer that started, restarted, quit or was just found: the taps leave out the
    /// wrong processes, so the lease reopens. Verdicts are read only when something changed.
    fn check_replayers(&mut self) -> Result<()> {
        let now = process_list();
        let judged = self.settle_probes();
        if judged || now != self.processes {
            anyhow::ensure!(
                replayers_in(&now) == self.replayers,
                "audio replay process started or stopped"
            );
            self.processes = now;
        }
        self.start_probes();
        Ok(())
    }

    /// Keeps each finished probe's verdict, and lets go of the finished probes and of those
    /// whose process stopped reading or playing. True when a verdict landed.
    fn settle_probes(&mut self) -> bool {
        let mut judged = false;
        self.probes.retain(|probe| {
            let verdict = probe.verdict();
            // A process that quit before its verdict has no name left to keep it under.
            if let (Some(verdict), Some(app)) = (verdict, identity(probe.process)) {
                match verdict {
                    Verdict::Replays => {
                        tracing::info!(%app, "audio replayer found — leaving it out of the capture")
                    }
                    Verdict::Own => tracing::debug!(%app, "process plays audio of its own"),
                }
                remember(&app, verdict);
            }
            judged |= verdict.is_some();
            verdict.is_none() && duplex(probe.process)
        });
        judged
    }

    /// Starts judging each process that reads and plays audio and has no verdict or probe yet.
    fn start_probes(&mut self) {
        let me = own_process_object();
        for &process in &self.processes {
            let probed = self.probes.iter().any(|p| p.process == process);
            if Some(process) == me || probed || !duplex(process) {
                continue;
            }
            let Some(app) = identity(process).filter(|id| verdict_of(id).is_none()) else {
                continue;
            };
            match Probe::start(process) {
                Ok(probe) => self.probes.push(probe),
                // Unjudged it stays in the capture. Asking again every second would not help.
                Err(e) => {
                    tracing::debug!(%app, error = %format!("{e:#}"), "audio replay probe did not start");
                    remember(&app, Verdict::Own);
                }
            }
        }
    }

    /// Once: a tap that never carried a sample past [`SILENCE_NOTE_AFTER`]. A refused grant and a
    /// quiet Mac look the same, and an app holding its output open while silent counts as
    /// playing, so this is a note, not a verdict.
    fn note_silence(&mut self) {
        if self.silence_noted || self.opened.elapsed() < SILENCE_NOTE_AFTER {
            return;
        }
        self.silence_noted = true;
        if !self.heard.load(Ordering::Relaxed) {
            tracing::info!(
                "no audio captured in the first 30 s — fine if the Mac is quiet; if it should \
                 be playing, this host may lack the System Audio Recording permission"
            );
        }
    }
}

const SILENCE_NOTE_AFTER: Duration = Duration::from_secs(30);
/// How often the consumer re-reads the tap's format. A new output device can change its rate.
const FORMAT_CHECK_EVERY: Duration = Duration::from_secs(1);

impl AudioCapturer for TapCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(Duration::from_secs(5))
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        self.note_silence();
        // A verdict is acted on at once: until then a replayer's copy is in every chunk.
        let judged = self.probes.iter().any(|p| p.verdict().is_some());
        if judged || self.last_format_check.elapsed() >= FORMAT_CHECK_EVERY {
            self.last_format_check = Instant::now();
            // Either way the lease reopens, with a resampler for the tap as it now is.
            match tap_format(self.tap.id) {
                Ok(f) if f.mSampleRate == self.tap_hz => {}
                Ok(f) => anyhow::bail!(
                    "process tap rate changed from {} Hz to {} Hz",
                    self.tap_hz,
                    f.mSampleRate
                ),
                Err(e) => return Err(e.context("process tap gone")),
            }
            self.check_replayers()?;
        }
        match self.chunks.recv_timeout(budget) {
            // The copy is made here, off the IO thread, so its buffer can go back.
            Ok(c) => {
                let chunk = c.clone();
                let _ = self.spare.try_send(c);
                Ok(chunk)
            }
            // A quiet tap is not a failure.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("process tap IO ended")),
        }
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.rate_hz
    }

    fn drain(&mut self) {
        while let Ok(c) = self.chunks.try_recv() {
            let _ = self.spare.try_send(c);
        }
        self.active.store(true, Ordering::Relaxed);
    }

    fn idle(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

impl Drop for TapCapturer {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        self.io.stop();
    }
}

/// Client-only audio silences the Mac's speakers while the tap is read. Host-and-client,
/// follow-default and a keep-host-audio session leave them playing. `MutedWhenTapped` lifts
/// the moment reading stops.
fn mute_behavior() -> CATapMuteBehavior {
    let mode = pf_host_config::config().audio_output_mode;
    if mode.prefers_host_hardware()
        || mode.keeps_default()
        || crate::capture_policy::session_keeps_default()
    {
        CATapMuteBehavior::Unmuted
    } else {
        CATapMuteBehavior::MutedWhenTapped
    }
}

/// Replayers known ahead, so not even their first second is doubled: ARK (SoundSource, Audio
/// Hijack) and FineTune. Each mutes an app with a tap of its own and plays it again, so a tap
/// holding both carries every sound twice, the replay milliseconds late.
const REPLAYERS: &[&str] = &["com.rogueamoeba.arkaudiod", "com.finetuneapp.finetune"];

/// What each probed app plays, by [`identity`], for the life of the host.
static JUDGED: Mutex<Vec<(String, Verdict)>> = Mutex::new(Vec::new());

/// Every audio client's process object. Unreadable reads as empty: nothing is left out.
fn process_list() -> Vec<AudioObjectID> {
    prop_list(SYSTEM, kAudioHardwarePropertyProcessObjectList).unwrap_or_default()
}

fn replayers_in(processes: &[AudioObjectID]) -> Vec<AudioObjectID> {
    let replays = |id: String| verdict_of(&id) == Some(Verdict::Replays);
    let replayer = |&p: &AudioObjectID| identity(p).is_some_and(replays);
    processes.iter().copied().filter(replayer).collect()
}

/// The name a verdict is kept under: the bundle id, or the pid of a process that has none.
fn identity(process: AudioObjectID) -> Option<String> {
    bundle_id(process).filter(|id| !id.is_empty()).or_else(|| {
        let pid = prop::<i32>(process, kAudioProcessPropertyPID).ok()?;
        Some(format!("pid {pid}"))
    })
}

fn verdict_of(identity: &str) -> Option<Verdict> {
    if REPLAYERS.contains(&identity) {
        return Some(Verdict::Replays);
    }
    let judged = JUDGED.lock().unwrap_or_else(PoisonError::into_inner);
    judged
        .iter()
        .find(|(id, _)| id == identity)
        .map(|&(_, v)| v)
}

/// The first verdict on an app stands.
fn remember(identity: &str, verdict: Verdict) {
    let mut judged = JUDGED.lock().unwrap_or_else(PoisonError::into_inner);
    if !judged.iter().any(|(id, _)| id == identity) {
        judged.push((identity.to_owned(), verdict));
    }
}

/// Whether a process reads audio and plays audio now. A replayer does while it replays; so
/// does a call, which the probe then clears. A tool that reads in one process and plays from
/// another is never judged: finding it takes a probe of every playing process.
fn duplex(process: AudioObjectID) -> bool {
    let runs = |selector| prop::<u32>(process, selector).unwrap_or(0) != 0;
    runs(kAudioProcessPropertyIsRunningInput) && runs(kAudioProcessPropertyIsRunningOutput)
}

/// Listens to one process beside everyone else until its audio tells what it is. Both taps
/// are unmuted, so a listener hears nothing change. Fields drop in order: IO stops first.
struct Probe {
    process: AudioObjectID,
    verdict: Arc<OnceLock<Verdict>>,
    _io: IoProc,
    _aggregate: Aggregate,
    _rest: Tap,
    _own: Tap,
}

impl Probe {
    fn start(process: AudioObjectID) -> Result<Probe> {
        let rest = Tap::global(CATapMuteBehavior::Unmuted, &[process])?;
        let own = Tap::only(&[process], CATapMuteBehavior::Unmuted)?;
        let hz = tap_format(rest.id)?.mSampleRate.round() as u32;
        // The rest goes first: the IOProc reads the taps as pairs of channels in this order.
        let aggregate = Aggregate::create(&[&rest.uid, &own.uid])?;
        let verdict = Arc::new(OnceLock::new());
        let io = IoProc::start(aggregate.id, ProbeState::new(hz, verdict.clone()))?;
        Ok(Probe {
            process,
            verdict,
            _io: io,
            _aggregate: aggregate,
            _rest: rest,
            _own: own,
        })
    }

    fn verdict(&self) -> Option<Verdict> {
        self.verdict.get().copied()
    }
}

/// What the probe's IOProc owns.
struct ProbeState {
    rest: Decimator,
    own: Decimator,
    judge: Judge,
    verdict: Arc<OnceLock<Verdict>>,
    stereo: Vec<f32>,
    rest_low: Vec<f32>,
    own_low: Vec<f32>,
}

impl ProbeState {
    fn new(tap_hz: u32, verdict: Arc<OnceLock<Verdict>>) -> ProbeState {
        ProbeState {
            rest: Decimator::new(tap_hz),
            own: Decimator::new(tap_hz),
            judge: Judge::default(),
            verdict,
            stereo: Vec::new(),
            rest_low: Vec::new(),
            own_low: Vec::new(),
        }
    }
}

impl Cycle for ProbeState {
    /// The judge compares here, on an IO thread that plays nothing: about a millisecond, four
    /// times a second, until the verdict.
    fn cycle(&mut self, input: &AudioBufferList) {
        if self.verdict.get().is_some() {
            return;
        }
        let (bufs, n) = buffers(input);
        self.rest_low.clear();
        self.own_low.clear();
        let taps = [
            (&mut self.rest, &mut self.rest_low),
            (&mut self.own, &mut self.own_low),
        ];
        for (pair, (decimator, low)) in taps.into_iter().enumerate() {
            self.stereo.clear();
            convert::gather_pair(&bufs[..n], pair, &mut self.stereo);
            decimator.push(&self.stereo, low);
        }
        if let Some(verdict) = self.judge.feed(&self.rest_low, &self.own_low) {
            let _ = self.verdict.set(verdict);
        }
    }
}

/// A private stereo process tap.
struct Tap {
    id: AudioObjectID,
    uid: String,
}

impl Tap {
    /// Every process but this one and `skip`.
    fn global(mute: CATapMuteBehavior, skip: &[AudioObjectID]) -> Result<Tap> {
        objc2::rc::autoreleasepool(|_| {
            let ids = numbers(own_process_object().iter().chain(skip));
            // SAFETY: a fresh allocation; the initializer takes an NSArray of NSNumbers holding
            // process object ids, which `ids` is.
            let desc = unsafe {
                CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &ids,
                )
            };
            Tap::create(&desc, mute)
        })
    }

    /// Only `processes`.
    fn only(processes: &[AudioObjectID], mute: CATapMuteBehavior) -> Result<Tap> {
        objc2::rc::autoreleasepool(|_| {
            let ids = numbers(processes);
            // SAFETY: as in `global`.
            let desc = unsafe {
                CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &ids)
            };
            Tap::create(&desc, mute)
        })
    }

    fn create(desc: &CATapDescription, mute: CATapMuteBehavior) -> Result<Tap> {
        // SAFETY: plain property setters on a live description.
        unsafe {
            desc.setPrivate(true);
            desc.setMuteBehavior(mute);
            desc.setName(&NSString::from_str("Punktfunk desktop audio"));
        }
        // SAFETY: `UUID` returns the description's own non-null NSUUID.
        let uid = unsafe { desc.UUID() }.UUIDString().to_string();
        let mut id: AudioObjectID = 0;
        // SAFETY: `desc` is a valid description and `id` a live out-pointer.
        let status = unsafe { AudioHardwareCreateProcessTap(Some(desc), &mut id) };
        check(status, "create process tap")?;
        Ok(Tap { id, uid })
    }
}

fn numbers<'a>(ids: impl IntoIterator<Item = &'a AudioObjectID>) -> Retained<NSArray<NSNumber>> {
    let ids: Vec<_> = ids.into_iter().map(|&id| NSNumber::new_u32(id)).collect();
    NSArray::from_retained_slice(&ids)
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: `id` is the tap this value created; nothing reads it after drop.
        let status = unsafe { AudioHardwareDestroyProcessTap(self.id) };
        if status != 0 {
            tracing::debug!(status = %os_status(status), "process tap not destroyed");
        }
    }
}

/// A private aggregate device whose inputs are the taps, in order.
struct Aggregate {
    id: AudioObjectID,
}

impl Aggregate {
    fn create(tap_uids: &[&str]) -> Result<Aggregate> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let uid = format!(
            "io.unom.punktfunk.audio-tap-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        objc2::rc::autoreleasepool(|_| {
            let yes = NSNumber::new_bool(true);
            let sub_taps: Vec<_> = tap_uids
                .iter()
                .map(|uid| {
                    NSDictionary::<NSString, AnyObject>::from_slices(
                        &[&*key(kAudioSubTapUIDKey)],
                        &[NSString::from_str(uid).as_ref()],
                    )
                })
                .collect();
            let taps = NSArray::from_retained_slice(&sub_taps);
            let uid = NSString::from_str(&uid);
            let name = NSString::from_str("Punktfunk desktop audio");
            let keys = [
                key(kAudioAggregateDeviceUIDKey),
                key(kAudioAggregateDeviceNameKey),
                key(kAudioAggregateDeviceIsPrivateKey),
                key(kAudioAggregateDeviceTapListKey),
                key(kAudioAggregateDeviceTapAutoStartKey),
            ];
            let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let desc = NSDictionary::<NSString, AnyObject>::from_slices(
                &keys,
                &[
                    uid.as_ref(),
                    name.as_ref(),
                    yes.as_ref(),
                    taps.as_ref(),
                    yes.as_ref(),
                ],
            );
            let cf: &CFDictionary<NSString, AnyObject> = (*desc).as_ref();
            let mut id: AudioObjectID = 0;
            // SAFETY: the keys and values are the documented types, and `id` is live.
            let status = unsafe {
                AudioHardwareCreateAggregateDevice(cf.as_opaque(), NonNull::from(&mut id))
            };
            check(status, "create tap aggregate device")?;
            Ok(Aggregate { id })
        })
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: `id` is the aggregate this value created; its IOProc is already destroyed.
        let status = unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
        if status != 0 {
            tracing::debug!(status = %os_status(status), "tap aggregate device not destroyed");
        }
    }
}

/// What an IOProc does each cycle. Only the HAL's IO thread calls it while IO runs.
trait Cycle: Send {
    fn cycle(&mut self, input: &AudioBufferList);
}

/// What the capture's IOProc owns.
struct IoState {
    tx: SyncSender<Vec<f32>>,
    /// Chunk buffers the reader hands back, so a cycle fills one instead of allocating.
    spare: Receiver<Vec<f32>>,
    active: Arc<AtomicBool>,
    heard: Arc<AtomicBool>,
    channels: usize,
    resampler: convert::Resampler,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
    tap_hz: u32,
    open_hz: u32,
    stats: CaptureStats,
    last_stats: Instant,
    last_callback: Option<Instant>,
}

impl Cycle for IoState {
    /// Allocates nothing once the spare buffers have grown to the tap's cycle.
    fn cycle(&mut self, input: &AudioBufferList) {
        let (bufs, n) = buffers(input);
        self.stereo.clear();
        convert::gather_stereo(&bufs[..n], &mut self.stereo);
        let frames_in = self.stereo.len() / 2;
        let now = Instant::now();
        let quantum = Duration::from_secs_f64(frames_in as f64 / f64::from(self.tap_hz.max(1)));
        self.stats
            .observe_callback(self.last_callback.map(|t| now - t), quantum);
        self.last_callback = Some(now);
        self.resampled.clear();
        self.resampler.process(&self.stereo, &mut self.resampled);
        if self.resampled.is_empty() {
            return;
        }
        let mut out = self.spare.try_recv().unwrap_or_default();
        out.clear();
        convert::place(&self.resampled, self.channels, &mut out);
        if !self.heard.load(Ordering::Relaxed) && out.iter().any(|&s| s != 0.0) {
            self.heard.store(true, Ordering::Relaxed);
        }
        self.stats.observe(&out, self.channels as u32);
        // Lossy: a full channel under a live reader is encode lag.
        if self.tx.try_send(out).is_err() && self.active.load(Ordering::Relaxed) {
            self.stats.dropped_chunks += 1;
        }
        self.stats
            .flush_window(&mut self.last_stats, self.open_hz, Some("process tap"));
    }
}

/// The most `AudioBuffer`s a cycle reads: a tap delivers one interleaved buffer or one per
/// channel, and only the first two channels are read.
const MAX_BUFFERS: usize = 8;

/// The tap's buffers as `(samples, channels)`, and how many of the slots hold one.
fn buffers(list: &AudioBufferList) -> ([(&[f32], usize); MAX_BUFFERS], usize) {
    // SAFETY: `mBuffers` is a C flexible array holding `mNumberBuffers` entries.
    let raw =
        unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
    let mut out = [(&[][..], 0); MAX_BUFFERS];
    let mut n = 0;
    let live = raw
        .iter()
        .filter(|b| !b.mData.is_null() && b.mNumberChannels > 0);
    for (slot, b) in out.iter_mut().zip(live) {
        let len = b.mDataByteSize as usize / size_of::<f32>();
        // SAFETY: the HAL hands `mDataByteSize` bytes of 32-bit float samples at `mData`,
        // valid for this IO cycle, which outlives the borrow.
        let s = unsafe { std::slice::from_raw_parts(b.mData.cast::<f32>(), len) };
        *slot = (s, b.mNumberChannels as usize);
        n += 1;
    }
    (out, n)
}

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the boxed state that `IoProc::start` registered; the HAL calls this
    // proc on one IO thread at a time and never after `AudioDeviceDestroyIOProcID`, which
    // runs before the state is freed. `input` is valid for this cycle.
    let (state, input) = unsafe { (&mut *client.cast::<Box<dyn Cycle>>(), input.as_ref()) };
    // A panic must not unwind into the HAL.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.cycle(input)));
    0
}

/// The IOProc registered on the aggregate, and the state it owns.
struct IoProc {
    device: AudioObjectID,
    id: AudioDeviceIOProcID,
    /// Boxed twice: the HAL carries one thin pointer.
    state: Option<NonNull<Box<dyn Cycle>>>,
}

// SAFETY: `state` is reached only by the HAL's IO thread while IO runs, and freed by `stop`
// after the IOProc is destroyed; `Cycle` is `Send`.
unsafe impl Send for IoProc {}

impl IoProc {
    fn start(device: AudioObjectID, state: impl Cycle + 'static) -> Result<IoProc> {
        let state: Box<dyn Cycle> = Box::new(state);
        let state = NonNull::from(Box::leak(Box::new(state)));
        let mut io = IoProc {
            device,
            id: None,
            state: Some(state),
        };
        // SAFETY: `io_proc` matches `AudioDeviceIOProc`, `state` outlives the registration
        // (freed in `stop`), and `io.id` is a live out-pointer.
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(io_proc),
                state.as_ptr().cast(),
                NonNull::from(&mut io.id),
            )
        };
        check(status, "create tap IOProc")?;
        // SAFETY: `io.id` was just registered on `device`.
        check(unsafe { AudioDeviceStart(device, io.id) }, "start tap IO")?;
        Ok(io)
    }

    /// Stops IO, destroys the IOProc, then frees its state. Idempotent.
    fn stop(&mut self) {
        if self.id.is_some() {
            // SAFETY: `id` is registered on `device`. Called off the IO thread, both calls
            // return once the IOProc can no longer run.
            unsafe {
                AudioDeviceStop(self.device, self.id);
                AudioDeviceDestroyIOProcID(self.device, self.id);
            }
            self.id = None;
        }
        if let Some(state) = self.state.take() {
            // SAFETY: leaked in `start`; no IOProc can reach it any more.
            drop(unsafe { Box::from_raw(state.as_ptr()) });
        }
    }
}

impl Drop for IoProc {
    fn drop(&mut self) {
        self.stop();
    }
}

/// This process's Core Audio object, to leave out of the tap. `None` before the HAL knows it.
fn own_process_object() -> Option<AudioObjectID> {
    let pid = std::process::id() as i32;
    prop_with::<AudioObjectID>(
        SYSTEM,
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        Some(&pid),
    )
    .ok()
    .filter(|&id| id != 0)
}

/// The tap's stream format, which must be native-endian 32-bit float PCM.
fn tap_format(tap: AudioObjectID) -> Result<AudioStreamBasicDescription> {
    let format = prop::<AudioStreamBasicDescription>(tap, kAudioTapPropertyFormat)
        .context("read process tap format")?;
    let float = format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0;
    anyhow::ensure!(
        format.mFormatID == kAudioFormatLinearPCM && float && format.mBitsPerChannel == 32,
        "process tap delivers a format other than 32-bit float PCM ({format:?})"
    );
    Ok(format)
}

fn bundle_id(process: AudioObjectID) -> Option<String> {
    let raw = prop::<*const CFString>(process, kAudioProcessPropertyBundleID).ok()?;
    // SAFETY: the property hands back a +1 CFString, or null.
    let s = unsafe { CFRetained::from_raw(NonNull::new(raw.cast_mut())?) };
    Some(s.to_string().to_ascii_lowercase())
}

fn key(k: &CStr) -> objc2::rc::Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

/// Property types Core Audio writes whole, valid for any bit pattern.
trait Plain: Copy {}
impl Plain for u32 {}
impl Plain for i32 {}
impl Plain for AudioStreamBasicDescription {}
impl Plain for *const CFString {}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn prop<T: Plain>(obj: AudioObjectID, selector: AudioObjectPropertySelector) -> Result<T> {
    prop_with(obj, selector, None)
}

/// One fixed-size property, with an optional `i32` qualifier.
fn prop_with<T: Plain>(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
    qualifier: Option<&i32>,
) -> Result<T> {
    let addr = address(selector);
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let (q_size, q_ptr) = qualifier.map_or((0, std::ptr::null()), |q| {
        (size_of::<i32>() as u32, (q as *const i32).cast::<c_void>())
    });
    // SAFETY: every pointer is live for the call and `out` has `size` writable bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            q_size,
            q_ptr,
            NonNull::from(&mut size),
            NonNull::new_unchecked(out.as_mut_ptr().cast()),
        )
    };
    check(status, &format!("read property {}", fourcc(selector)))?;
    anyhow::ensure!(
        size as usize == size_of::<T>(),
        "property {} is short",
        fourcc(selector)
    );
    // SAFETY: Core Audio wrote all `size_of::<T>()` bytes, and `T: Plain` is valid for any bits.
    Ok(unsafe { out.assume_init() })
}

/// A property that is an array of object ids.
fn prop_list(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Result<Vec<AudioObjectID>> {
    let addr = address(selector);
    let mut size = 0u32;
    // SAFETY: `addr` and `size` are live for the call.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            obj,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    check(status, &format!("size property {}", fourcc(selector)))?;
    let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
    if ids.is_empty() {
        return Ok(ids);
    }
    // SAFETY: `ids` has `size` writable bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr().cast()),
        )
    };
    check(status, &format!("read property {}", fourcc(selector)))?;
    ids.truncate(size as usize / size_of::<AudioObjectID>());
    Ok(ids)
}

fn check(status: i32, what: &str) -> Result<()> {
    anyhow::ensure!(status == 0, "{what}: {}", os_status(status));
    Ok(())
}

/// An OSStatus as its four-character code where it is one (`'!obj'`), else as a number.
fn os_status(status: i32) -> String {
    let code = fourcc(status as u32);
    if code.starts_with('\'') {
        format!("OSStatus {code} ({status})")
    } else {
        format!("OSStatus {status}")
    }
}

fn fourcc(v: u32) -> String {
    let b = v.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        format!("'{}'", String::from_utf8_lossy(&b))
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk's buffer the reader hands back carries the next cycle: the IO thread fills it
    /// in place instead of allocating.
    #[test]
    fn a_returned_buffer_carries_the_next_cycle() {
        let (tx, chunks) = sync_channel(4);
        let (spare, spare_rx) = sync_channel(4);
        let mut state = IoState {
            tx,
            spare: spare_rx,
            active: Arc::default(),
            heard: Arc::default(),
            channels: 2,
            resampler: convert::Resampler::new(48_000, 48_000, 2).unwrap(),
            stereo: Vec::new(),
            resampled: Vec::new(),
            tap_hz: 48_000,
            open_hz: 48_000,
            stats: CaptureStats::default(),
            last_stats: Instant::now(),
            last_callback: None,
        };
        let mut samples = vec![0.25f32; 512 * 2];
        let list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [objc2_core_audio_types::AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (samples.len() * size_of::<f32>()) as u32,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        state.cycle(&list);
        let first = chunks.try_recv().expect("a chunk");
        let at = first.as_ptr();
        spare.send(first).unwrap();
        state.cycle(&list);
        let second = chunks.try_recv().expect("a chunk");
        assert_eq!(second.as_ptr(), at, "the returned buffer, refilled");
        assert!(!second.is_empty());
    }

    /// Two taps side by side in one buffer, the second the first 21 ms late.
    #[test]
    fn a_probe_calls_a_late_copy_a_replayer() {
        let verdict = Arc::new(OnceLock::new());
        let mut state = ProbeState::new(48_000, verdict.clone());
        let (frames, late) = (512, 1_008);
        let mut seed = 1u32;
        let app: Vec<f32> = (0..48_000 * 2)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        for start in (late..app.len() - frames).step_by(frames) {
            let mut samples: Vec<f32> = (start..start + frames)
                .flat_map(|t| [app[t], app[t], app[t - late], app[t - late]])
                .collect();
            let list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [objc2_core_audio_types::AudioBuffer {
                    mNumberChannels: 4,
                    mDataByteSize: (samples.len() * size_of::<f32>()) as u32,
                    mData: samples.as_mut_ptr().cast(),
                }],
            };
            state.cycle(&list);
        }
        assert_eq!(verdict.get(), Some(&Verdict::Replays));
    }

    #[test]
    fn a_seeded_replayer_needs_no_probe() {
        assert_eq!(
            verdict_of("com.rogueamoeba.arkaudiod"),
            Some(Verdict::Replays)
        );
        assert_eq!(verdict_of("io.unom.test.unheard"), None);
        remember("io.unom.test.unheard", Verdict::Own);
        assert_eq!(verdict_of("io.unom.test.unheard"), Some(Verdict::Own));
    }

    #[test]
    fn status_codes_read_as_four_characters() {
        assert_eq!(os_status(0x216f626a), "OSStatus '!obj' (560947818)");
        assert_eq!(os_status(-50), "OSStatus -50");
    }

    #[test]
    fn the_virtual_mic_refuses_with_a_reason() {
        let err = open_virtual_mic(1).err().expect("no plug-in, no mic");
        assert!(format!("{err:#}").contains("plug-in"));
    }

    /// An unmuted tap that never starts, so a listener hears nothing change.
    #[test]
    #[ignore = "creates a real process tap — run with --ignored on a Mac"]
    fn a_live_tap_reports_its_float_rate() {
        assert!(matches!(probe_capture_rate(), CaptureRate::Engine(hz) if hz > 0));
    }
}
