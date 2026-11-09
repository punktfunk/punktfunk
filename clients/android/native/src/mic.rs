//! Android microphone uplink (android-only): capture mic PCM via AAudio (LowLatency **input**),
//! Opus-encode 10 ms mono frames, and push them to the host over the connector's mic plane
//! (`send_mic` → 0xCB datagram). The mirror of [`crate::audio`] in reverse: AAudio's realtime input
//! callback hands captured f32 to a channel; a worker thread we own does the Opus encode + send
//! (encoding is too heavy for the realtime callback, exactly as decode is on the playback side).
//! Like the playback path, the realtime callback is allocation-free: captured bursts are copied
//! into pre-allocated buffers from a recycle free-list (pool empty = drop the chunk, never
//! allocate on the capture thread). Format: 48 kHz **mono**, 10 ms, Opus VOIP with in-band FEC —
//! the host decodes any Opus frame ≤ 120 ms with its stereo decoder (mono packets upmix), so this
//! needs no protocol change; speech gains nothing from stereo, and the shorter frame shaves a
//! buffering interval off the uplink.
//!
//! **Mute** is a flag the encode loop reads per 10 ms frame, never a stream teardown: the AAudio
//! input stream, the input-preset ladder it settled on and its primed buffers all survive a
//! mute/unmute untouched, so toggling costs an atomic load and nothing else. A disconnect (a
//! headset plugged or pulled) reopens the stream on the same encoder; see [`supervise`].
//!
//! The open ladder and its [`walk`] hold no NDK type; their rows run on the host.

#[cfg(target_os = "android")]
use {
    ndk::audio::{
        AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
        AudioSharingMode, AudioStream, AudioStreamBuilder, SessionId,
    },
    punktfunk_core::audio::mic::MicEncoder,
    punktfunk_core::client::NativeClient,
    std::ffi::c_void,
    std::sync::atomic::{AtomicBool, AtomicU64, Ordering},
    std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    std::sync::Arc,
    std::time::Duration,
};

/// What one capture open attempt yields: the stream, plus both halves of the PCM hand-off — the
/// receiver the encode worker drains and the sender that returns emptied buffers for reuse. Note
/// the pair is the mirror image of [`crate::audio::OpenedPlayback`]'s: here the callback produces
/// and the worker consumes.
///
/// Named rather than written inline for the same reason as that one — `clippy::type_complexity`,
/// now that the Android target is actually linted (`:kit:cargoNdkClippy`).
#[cfg(target_os = "android")]
type OpenedCapture = ndk::audio::Result<(AudioStream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>)>;
/// A started stream from [`open_started`]: the capture plus the session id AAudio allocated.
#[cfg(target_os = "android")]
type StartedCapture = (AudioStream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>, i32);

const CHANNELS: usize = 1;
const SAMPLE_RATE: i32 = 48_000;
/// Captured-chunk hand-off depth (each ~ one burst); drops on overflow (best-effort uplink).
/// Bursts are sized in frames, so the wall-time depth is unchanged by the stereo→mono move.
#[cfg(target_os = "android")]
const RING_CHUNKS: usize = 64;
/// Free-list buffer capacity, in interleaved f32 samples: comfortably above a LowLatency input
/// burst (typically ≤ ~480 frames — mono, so samples = frames). A device with larger bursts costs
/// each buffer a one-time grow on the capture thread, after which the steady state is
/// allocation-free again.
#[cfg(target_os = "android")]
const CHUNK_CAP_SAMPLES: usize = 960; // 20 ms mono — the same wall-time as the old stereo value
/// Settling time between a disconnect and the reopen: a headset handing over the route.
#[cfg(target_os = "android")]
const REOPEN_SETTLE_MS: u64 = 250;
/// Reopens that may find no input before the mic gives up: ~2 s, past a Bluetooth handover.
#[cfg(target_os = "android")]
const REOPEN_ATTEMPTS: u32 = 8;

/// Owned by [`crate::session::SessionHandle`]: the encode thread, which owns the AAudio input
/// stream (see [`supervise`]). Dropping this stops the thread, and the thread closes the stream.
#[cfg(target_os = "android")]
pub struct MicCapture {
    /// The audio-session id AAudio allocated (`> 0`) when echo cancellation asked for one — the
    /// hook Kotlin hangs the Java `AcousticEchoCanceler`/`NoiseSuppressor` on. `0` = none. The
    /// first open's; a reopen after a disconnect keeps the HAL preset but not the Java effects.
    session_id: i32,
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Why [`run`] returned.
#[cfg(target_os = "android")]
#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Shutdown,
    /// AAudio reported the stream disconnected; it is dead by contract, reopen.
    Disconnected,
}

/// AAudio's sharing mode, as the ladder names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sharing {
    Exclusive,
    Shared,
}

/// One rung: a sharing mode, and whether the voice preset asks for echo cancellation.
type Rung = (Sharing, bool);

/// What AAudio granted on an open: the rate, the channel count, and whether samples are f32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grant {
    rate: i32,
    channels: i32,
    float: bool,
}

/// The one grant the capture callback reads: it slices `num_frames × CHANNELS` f32, and the
/// encoder takes 48 kHz.
const WANTED: Grant = Grant {
    rate: SAMPLE_RATE,
    channels: CHANNELS as i32,
    float: true,
};

/// The open ladder: Exclusive first — MMAP-exclusive is AAudio's lowest-latency path — falling
/// back to Shared when the device refuses (no MMAP, mic claimed, …); and each sharing mode with
/// the voice preset before without it, because some HALs reject VoiceCommunication (or a session
/// id) outright and a mic without echo cancellation still beats no mic. Without echo cancellation
/// the ladder is the last two rungs. The started-log prints what the device actually GRANTED
/// (`share=`/`session=`).
const RUNGS: [Rung; 4] = [
    (Sharing::Exclusive, true),
    (Sharing::Shared, true),
    (Sharing::Exclusive, false),
    (Sharing::Shared, false),
];

fn capture_rungs(echo_cancel: bool) -> &'static [Rung] {
    if echo_cancel {
        &RUNGS
    } else {
        &RUNGS[2..]
    }
}

/// Walks `rungs` best first to the first stream that opens, is granted [`WANTED`] and starts. A
/// refused open is the next rung, and so is a stream that fails either check: it is dropped,
/// which closes it, first. A wrong grant is never started.
fn walk<S, E: std::fmt::Display>(
    rungs: &[Rung],
    mut open: impl FnMut(Rung) -> Result<S, E>,
    grant: impl Fn(&S) -> Grant,
    mut start: impl FnMut(&S) -> Result<(), E>,
) -> Option<S> {
    for &(sharing, voice) in rungs {
        let preset = if voice { "+VoiceCommunication" } else { "" };
        let stream = match open((sharing, voice)) {
            Ok(stream) => stream,
            Err(e) => {
                log::info!("mic: open {sharing:?}{preset} failed ({e}) — trying the next fallback");
                continue;
            }
        };
        let granted = grant(&stream);
        if granted != WANTED {
            log::warn!(
                "mic: open {sharing:?}{preset} granted rate={} ch={} f32={}, needed \
                 {SAMPLE_RATE}/{CHANNELS}/PCM_Float — trying the next fallback",
                granted.rate,
                granted.channels,
                granted.float,
            );
            continue;
        }
        match start(&stream) {
            Ok(()) => return Some(stream),
            Err(e) => log::error!("mic: request_start: {e}"),
        }
    }
    None
}

/// One open attempt at a given sharing mode (same pattern as [`crate::audio`]: `open_stream`
/// consumes the builder AND the callback, so each try rebuilds the channels it captures).
/// `captured`/`dropped` are the counters the realtime callback bumps.
#[cfg(target_os = "android")]
fn open_capture(
    sharing: Sharing,
    voice: bool,
    captured: &Arc<AtomicU64>,
    dropped: &Arc<AtomicU64>,
    disconnected: &Arc<AtomicBool>,
) -> OpenedCapture {
    let (tx, rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
    // Recycle free-list, mirroring the playback path: the realtime capture callback must
    // not touch the allocator (Android's Scudo has unbounded malloc/free tail latency — an
    // allocation here is a missed burst), so it pops a pre-allocated buffer, copies the
    // burst in and sends it; the encode worker returns drained buffers. Pool empty = DROP
    // the chunk (counted) rather than allocate.
    let (free_tx, free_rx) = sync_channel::<Vec<f32>>(RING_CHUNKS);
    for _ in 0..RING_CHUNKS {
        let _ = free_tx.try_send(Vec::with_capacity(CHUNK_CAP_SAMPLES));
    }
    let cb_captured = captured.clone();
    let cb_dropped = dropped.clone();
    let cb_free_tx = free_tx.clone(); // returns the buffer when the data channel is full

    let callback = move |_s: &AudioStream, data: *mut c_void, num_frames: i32| {
        let Some(n) = crate::audio_format::callback_sample_count(num_frames, CHANNELS) else {
            return AudioCallbackResult::Continue;
        };
        if data.is_null() {
            return AudioCallbackResult::Stop;
        }
        // SAFETY: AAudio supplies `n` captured f32 samples at non-null `data`: `walk` starts
        // only a stream granted mono PCM_Float, and the shared length validator rejected
        // nonpositive or unrepresentable callback counts.
        let inp = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), n) };
        cb_captured.fetch_add(num_frames as u64, Ordering::Relaxed);
        match free_rx.try_recv() {
            Ok(mut buf) => {
                buf.clear();
                buf.extend_from_slice(inp); // retained capacity — no realloc past the first
                match tx.try_send(buf) {
                    Ok(()) => {}
                    Err(TrySendError::Full(buf)) => {
                        // Encoder lagging: drop the chunk, hand the buffer straight back.
                        let _ = cb_free_tx.try_send(buf);
                        cb_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => return AudioCallbackResult::Stop,
                }
            }
            // Pool empty (every buffer in flight): drop, never allocate on this thread.
            Err(_) => {
                cb_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        AudioCallbackResult::Continue
    };

    // NOTE: no `.frames_per_data_callback(...)`: AAudio's own docs call leaving it unset
    // the lowest-latency path (the callback then runs at the device's optimal burst,
    // while pinning a size inserts an adaptation buffer), and the encode side re-chunks
    // to 10 ms frames regardless of how the bursts arrive.
    let mut builder = AudioStreamBuilder::new()?
        .direction(AudioDirection::Input)
        .sample_rate(SAMPLE_RATE)
        .channel_count(CHANNELS as i32)
        .format(AudioFormat::PCM_Float)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .sharing_mode(match sharing {
            Sharing::Exclusive => AudioSharingMode::Exclusive,
            Sharing::Shared => AudioSharingMode::Shared,
        });
    if voice {
        // VoiceCommunication routes the capture through the HAL's AEC/NS; the allocated
        // session id (`None` = allocate) is what Kotlin attaches the Java effects to.
        builder = builder
            .input_preset(AudioInputPreset::VoiceCommunication)
            .session_id(None);
    }
    let cb_disconnected = disconnected.clone();
    let stream = builder
        .data_callback(Box::new(callback))
        .error_callback(Box::new(move |_s, e| {
            log::warn!("mic: AAudio error (device reroute/disconnect?): {e:?}");
            cb_disconnected.store(true, Ordering::SeqCst);
        }))
        .open_stream()?;
    Ok((stream, rx, free_tx))
}

/// [`walk`] the ladder over AAudio: the started stream, its hand-off pair, and the session id
/// AAudio allocated (`0` = none). `None` when the walk found no stream.
#[cfg(target_os = "android")]
fn open_started(
    echo_cancel: bool,
    captured: &Arc<AtomicU64>,
    dropped: &Arc<AtomicU64>,
    disconnected: &Arc<AtomicBool>,
) -> Option<StartedCapture> {
    let (stream, rx, free_tx) = walk(
        capture_rungs(echo_cancel),
        |(sharing, voice)| open_capture(sharing, voice, captured, dropped, disconnected),
        |(stream, ..)| Grant {
            rate: stream.sample_rate(),
            channels: stream.channel_count(),
            float: stream.format() == AudioFormat::PCM_Float,
        },
        |(stream, ..)| stream.request_start(),
    )?;
    // `> 0` is the handle Kotlin hangs the Java AcousticEchoCanceler/NoiseSuppressor off as the
    // HAL preset's backstop; only a voice rung asks for one.
    let session_id = match stream.session_id() {
        SessionId::Allocated(id) => id.get(),
        SessionId::None => 0,
    };
    log::info!(
        "mic: AAudio input started rate={} ch={} fmt={:?} share={:?} session={session_id}",
        stream.sample_rate(),
        stream.channel_count(),
        stream.format(),
        stream.sharing_mode(),
    );
    Some((stream, rx, free_tx, session_id))
}

#[cfg(target_os = "android")]
impl MicCapture {
    /// Open low-latency 48 kHz mono capture and start the Opus uplink worker.
    ///
    /// `echo_cancel` selects the voice-communication preset and an audio session for Kotlin's
    /// effects. The realtime callback only copies into recycled buffers and validates every foreign
    /// pointer/length pair before slicing. `muted` remains session-owned across capture restarts.
    /// Returns `None` without disturbing the rest of the stream when setup fails.
    pub fn start(
        client: Arc<NativeClient>,
        echo_cancel: bool,
        muted: Arc<AtomicBool>,
    ) -> Option<MicCapture> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        // The worker opens the stream and owns it for life, so it never crosses threads; the
        // first open's outcome comes back here for the session id.
        let (ready_tx, ready_rx) = sync_channel::<Option<i32>>(1);
        let join = match std::thread::Builder::new()
            .name("pf-mic".into())
            .spawn(move || supervise(client, echo_cancel, muted, sd, ready_tx))
        {
            Ok(join) => join,
            Err(e) => {
                log::error!("mic: encode thread spawn failed: {e}");
                return None;
            }
        };
        match ready_rx.recv() {
            Ok(Some(session_id)) => Some(MicCapture {
                session_id,
                shutdown,
                join: Some(join),
            }),
            _ => {
                let _ = join.join();
                None
            }
        }
    }

    /// The audio-session id AAudio allocated (`> 0`; see [`MicCapture::start`]), `0` = none.
    pub fn session_id(&self) -> i32 {
        self.session_id
    }
}

#[cfg(target_os = "android")]
impl Drop for MicCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join(); // the worker stops and closes the AAudio stream on its way out
        }
    }
}

/// Own the input stream for the session: open it, run the uplink against it, and open it again
/// when AAudio disconnects it (a headset plugged or pulled, Bluetooth taking the route). The
/// stream is dead by AAudio's contract then; without a reopen the mic stayed silent while the
/// session still offered its mute. The encoder and `seq` carry over, so the host sees one stream.
#[cfg(target_os = "android")]
fn supervise(
    client: Arc<NativeClient>,
    echo_cancel: bool,
    muted: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    ready: SyncSender<Option<i32>>,
) {
    // Fold this Opus-encode/uplink thread into the client's hot-thread set so the ADPF session the
    // decode thread opens keeps mic encode on a fast core too (the playback side's decode_loop
    // does the same). No-op below API 33.
    client.register_hot_thread();
    crate::audio::boost_audio_thread("mic");
    // Self-heal on: the capture callback drops the NEWEST chunk when the channel is full, so a
    // stall here would otherwise become standing mic delay the host never makes back up.
    let mut up = match MicEncoder::new(true) {
        Ok(up) => up,
        Err(e) => {
            log::error!("mic: opus encoder init: {e} — mic disabled");
            let _ = ready.send(None);
            return;
        }
    };
    // Complexity 5 roughly halves encode cost for no audible loss on speech at this rate.
    if let Err(e) = up.encoder_mut().set_complexity(5) {
        log::warn!("mic: opus complexity not applied: {e}");
    }
    let captured = Arc::new(AtomicU64::new(0));
    // Chunks discarded on the capture thread (free-list empty / encoder lagging); logged
    // throttled from the encode loop.
    let dropped = Arc::new(AtomicU64::new(0));
    let mut ready = Some(ready);
    let mut attempts: u32 = 0;
    while !shutdown.load(Ordering::Relaxed) {
        let disconnected = Arc::new(AtomicBool::new(false));
        let Some((stream, rx, free_tx, session_id)) =
            open_started(echo_cancel, &captured, &dropped, &disconnected)
        else {
            if let Some(r) = ready.take() {
                log::error!("mic: open_stream (RECORD_AUDIO granted?): every mode refused");
                let _ = r.send(None);
                return;
            }
            attempts += 1;
            if attempts > REOPEN_ATTEMPTS {
                log::warn!("mic: no input device after the disconnect — the mic stays off");
                break;
            }
            crate::audio::nap(&shutdown, REOPEN_SETTLE_MS);
            continue;
        };
        attempts = 0;
        match ready.take() {
            Some(r) => {
                let _ = r.send(Some(session_id));
            }
            None => log::info!("mic: reopened after a disconnect (session={session_id})"),
        }
        let exit = run(
            &mut up,
            &client,
            &rx,
            &free_tx,
            &shutdown,
            &disconnected,
            &muted,
            &captured,
            &dropped,
        );
        let _ = stream.request_stop();
        drop(stream); // → AAudioStream_close
        if exit == Exit::Shutdown {
            break;
        }
        log::warn!("mic: input disconnected — reopening in {REOPEN_SETTLE_MS} ms");
        crate::audio::nap(&shutdown, REOPEN_SETTLE_MS);
    }
    log::info!(
        "mic: stopped (sent={} captured_frames={} dropped_chunks={} stale_frames={} \
         muted_frames={})",
        up.sent,
        captured.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        up.stale,
        up.muted_frames,
    );
}

/// Consumer for one stream: drain captured f32 into the encoder, which sends 10 ms mono frames
/// with `send_mic`, until shutdown or a disconnect. Drained chunk buffers go back to the
/// callback's free-list. A mute drops formed frames; the stream itself runs on untouched.
#[cfg(target_os = "android")]
#[allow(clippy::too_many_arguments)] // one call site, `supervise`
fn run(
    up: &mut MicEncoder,
    client: &NativeClient,
    rx: &Receiver<Vec<f32>>,
    free_tx: &SyncSender<Vec<f32>>,
    shutdown: &AtomicBool,
    disconnected: &AtomicBool,
    muted: &AtomicBool,
    captured: &AtomicU64,
    dropped: &AtomicU64,
) -> Exit {
    while !shutdown.load(Ordering::Relaxed) {
        if disconnected.load(Ordering::SeqCst) {
            return Exit::Disconnected;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(mut chunk) => {
                up.push(chunk.drain(..));
                let _ = free_tx.try_send(chunk);
                // Take whatever else queued while we were away, so a post-stall backlog lands as
                // ONE lump the self-heal can size up.
                while let Ok(mut chunk) = rx.try_recv() {
                    up.push(chunk.drain(..));
                    let _ = free_tx.try_send(chunk);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue, // wake to re-check shutdown/disconnect
            // The callback dropped its sender: the stream is gone.
            Err(RecvTimeoutError::Disconnected) => return Exit::Disconnected,
        }
        let before = up.sent;
        up.drain(muted.load(Ordering::Relaxed), |seq, pts, packet| {
            let _ = client.send_mic(seq, pts, packet.to_vec());
        });
        if up.sent / 500 != before / 500 {
            log::info!(
                "mic: sent={} captured_frames={} dropped_chunks={} stale_frames={} \
                 muted_frames={} peak={:.3}",
                up.sent,
                captured.load(Ordering::Relaxed),
                dropped.load(Ordering::Relaxed),
                up.stale,
                up.muted_frames,
                up.peak,
            );
            up.peak = 0.0;
        }
    }
    Exit::Shutdown
}

#[cfg(test)]
mod tests {
    use super::{capture_rungs, walk, Grant, Sharing, WANTED};
    use std::cell::RefCell;

    /// What a rung's stream does in a scripted walk.
    #[derive(Clone, Copy)]
    enum At {
        Refuses,
        /// Opens with [`WANTED`]; starts when `true`.
        Opens(bool),
        /// Opens with this grant, and would start.
        Grants(Grant),
    }

    /// A stream that logs its close.
    struct Stream<'a>(usize, &'a RefCell<Vec<String>>);

    impl Drop for Stream<'_> {
        fn drop(&mut self) {
            self.1.borrow_mut().push(format!("close {}", self.0));
        }
    }

    /// Walks a ladder whose rung `i` does `script[i]`: the rung used, then each start and close.
    fn walked(script: &[At]) -> (Option<usize>, Vec<String>) {
        let log = RefCell::new(Vec::new());
        let rungs = vec![(Sharing::Shared, false); script.len()];
        let mut next = 0;
        let used = walk(
            &rungs,
            |_| {
                next += 1;
                match script[next - 1] {
                    At::Refuses => Err("refused"),
                    _ => Ok(Stream(next - 1, &log)),
                }
            },
            |s| match script[s.0] {
                At::Grants(g) => g,
                _ => WANTED,
            },
            |s| {
                log.borrow_mut().push(format!("start {}", s.0));
                match script[s.0] {
                    At::Opens(false) => Err("start failed"),
                    _ => Ok(()),
                }
            },
        );
        let events = log.borrow().clone();
        (used.map(|s| s.0), events)
    }

    /// A row: its name, the script, the rung used, and each start and close.
    type Row = (
        &'static str,
        &'static [At],
        Option<usize>,
        &'static [&'static str],
    );

    /// R5-4: the callback slices mono f32 at 48 kHz, so any other grant is closed unstarted.
    #[test]
    fn a_grant_the_callback_cannot_read_is_the_next_rung() {
        let grants = [
            (
                "stereo",
                Grant {
                    channels: 2,
                    ..WANTED
                },
            ),
            (
                "44.1 kHz",
                Grant {
                    rate: 44_100,
                    ..WANTED
                },
            ),
            (
                "i16",
                Grant {
                    float: false,
                    ..WANTED
                },
            ),
        ];
        for (name, g) in grants {
            let want = (Some(1), vec!["close 0".to_string(), "start 1".to_string()]);
            assert_eq!(walked(&[At::Grants(g), At::Opens(true)]), want, "{name}");
        }
    }

    #[test]
    fn the_ladder_starts_the_first_stream_that_opens() {
        use At::{Opens, Refuses};
        let rows: [Row; 5] = [
            (
                "the first rung is used",
                &[Opens(true), Opens(true)],
                Some(0),
                &["start 0"],
            ),
            (
                "a refused open is the next rung",
                &[Refuses, Opens(true)],
                Some(1),
                &["start 1"],
            ),
            (
                "every rung refused is no mic",
                &[Refuses, Refuses],
                None,
                &[],
            ),
            (
                "R5-2: a rung that opens and fails to start is closed, then the next is tried",
                &[Opens(false), Opens(true)],
                Some(1),
                &["start 0", "close 0", "start 1"],
            ),
            (
                "a ladder that never starts is no mic, and every stream it opened is closed",
                &[Opens(false), Refuses, Opens(false)],
                None,
                &["start 0", "close 0", "start 2", "close 2"],
            ),
        ];
        for (name, script, used, events) in rows {
            assert_eq!(
                walked(script),
                (used, events.iter().map(|e| e.to_string()).collect()),
                "{name}"
            );
        }
    }

    /// Echo cancellation tries the voice preset in both modes before dropping it.
    #[test]
    fn the_voice_rungs_lead_and_the_plain_ladder_is_their_tail() {
        use Sharing::{Exclusive, Shared};
        assert_eq!(
            capture_rungs(true),
            [
                (Exclusive, true),
                (Shared, true),
                (Exclusive, false),
                (Shared, false)
            ]
        );
        assert_eq!(capture_rungs(false), [(Exclusive, false), (Shared, false)]);
    }
}
