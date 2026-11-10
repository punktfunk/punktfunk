//! The PyroWave decode lane (android-only): Vulkan compute decode + present, beside the
//! MediaCodec path rather than inside it.
//!
//! PyroWave is the opt-in wired-LAN wavelet codec (`design/pyrowave-codec-plan.md`). It
//! shares nothing with the other three codecs on this client: no `AMediaCodec`, no
//! `AImageReader`, no `ASurfaceControl` layer — the access unit is one self-delimiting
//! packet stream decoded by GPU compute into three Y′CbCr planes, and those planes are
//! sampled straight into a Vulkan swapchain on the same `SurfaceView`
//! ([`present`]). The decoder itself is the SHARED one the Linux and Windows clients use
//! (`pf_client_video::video_pyrowave`); only the device and the present path are ours.
//!
//! **Intra-only, which removes most of a decode loop.** Every frame stands alone, so
//! there is no reference chain to lose: no re-anchor freeze, no keyframe requests, no
//! concealment to hold off the screen. A damaged frame decodes to localized blur and the
//! next one is clean — the same call the Apple port made. What remains is receive →
//! decode → present.
//!
//! 64-bit only, mirroring `pyrowave-sys`: on armeabi-v7a the codec is not built (Vulkan's
//! armv7 calling convention has no bindgen form), and those boxes have neither the link
//! nor the Vulkan 1.3 device this needs. [`available`] answers for the whole lane.

#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod asc;
#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod device;
#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod present;

#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod imp {
    use super::{asc, device::PyroDevice, present::Present};
    use anyhow::{anyhow, Result};
    use ndk::native_window::NativeWindow;
    use pf_client_video::video_color::ColorDesc;
    use pf_client_video::video_pyrowave::{DecodeTimeline, PyroWaveDecoder};
    use punktfunk_core::au::AuMemory;
    use punktfunk_core::client::NativeClient;
    use punktfunk_core::error::PunktfunkError;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    use crate::decode::{note_received_frame, now_realtime_ns, DecodeOptions, NO_VIDEO_PATIENCE};

    /// The HUD's decoder label. Not an `AMediaCodec` name because there is no codec object
    /// here — naming the device keeps the line useful (it is the GPU that decodes).
    fn decoder_label(name: &str) -> String {
        format!("PyroWave / {name}")
    }

    /// Does this device decode PyroWave? Drives the `CODEC_PYROWAVE` advertisement.
    ///
    /// Answered once per process: the probe creates and destroys a Vulkan instance, which is
    /// milliseconds rather than microseconds, and Kotlin asks on every settings screen and
    /// every connect. The answer cannot change under a running app — it is a property of the
    /// driver — so a cache is the whole story rather than an optimization with a caveat.
    pub(crate) fn available() -> bool {
        static CAPABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CAPABLE.get_or_init(super::device::pyrowave_capable)
    }

    /// How often the split below reaches logcat.
    const SPLIT_EVERY: Duration = Duration::from_secs(5);

    /// Where a frame's time goes while the stats overlay is on: mean queue wait, parse,
    /// record, submit→done ([`Retire`]'s latest) and present, then the decoder's own GPU
    /// stage times. The HUD's one `decode` figure cannot tell a slow GPU from a down-clocked one.
    #[derive(Default)]
    struct SplitWindow {
        started: Option<Instant>,
        frames: u32,
        /// µs sums: wait, parse, record, gpu, present.
        sum: [u64; 5],
    }

    impl SplitWindow {
        fn add(&mut self, decoder: &PyroWaveDecoder, wait_us: u64, gpu_us: u64, present_us: u64) {
            let started = *self.started.get_or_insert_with(|| {
                decoder.gpu_stage_report(true); // the stage window starts with ours
                Instant::now()
            });
            let s = decoder.last_split();
            let frame = [
                wait_us,
                s.parse_us.into(),
                s.record_us.into(),
                gpu_us,
                present_us,
            ];
            for (acc, v) in self.sum.iter_mut().zip(frame) {
                *acc += v;
            }
            self.frames += 1;
            if started.elapsed() < SPLIT_EVERY {
                return;
            }
            let ms = |i: usize| self.sum[i] as f64 / f64::from(self.frames) / 1000.0;
            let (found, copied) = decoder.take_ring_counts();
            log::info!(
                "pyro: {} frames, mean ms: wait {:.2} parse {:.2} record {:.2} gpu {:.2} \
                 present {:.2}; AUs in place {}, copied in {}",
                self.frames,
                ms(0),
                ms(1),
                ms(2),
                ms(3),
                ms(4),
                found,
                copied
            );
            for line in decoder.gpu_stage_report(true) {
                log::info!("pyro:   {line}");
            }
            *self = SplitWindow {
                started: Some(Instant::now()),
                ..Default::default()
            };
        }
    }

    /// A frame the lane submitted and moved on from.
    struct Retiring {
        timeline: Arc<DecodeTimeline>,
        turn: u64,
        submitted: Instant,
        /// pts and receipt stamp while the overlay is on.
        hud: Option<(u64, i128)>,
    }

    /// Notices finished decodes, so the lane never waits on one: per frame it waits on the
    /// decoder's timeline, stamps the HUD's `decode` stage and keeps the GPU time for ADPF
    /// and the split. Joined on drop, which must come before the decoder's and the device's.
    struct Retire {
        tx: Option<mpsc::Sender<Retiring>>,
        thread: Option<std::thread::JoinHandle<()>>,
        /// Submit → done of the last frame it saw, µs.
        gpu_us: Arc<AtomicU64>,
    }

    impl Retire {
        /// Past this a decode is wedged; the frame goes unstamped and the lane's own slot
        /// wait reports the fault.
        const PATIENCE: Duration = Duration::from_millis(500);

        fn spawn(stats: Arc<crate::stats::VideoStats>) -> Result<Retire> {
            let (tx, rx) = mpsc::channel::<Retiring>();
            let gpu_us = Arc::new(AtomicU64::new(0));
            let last = gpu_us.clone();
            let thread = std::thread::Builder::new()
                .name("pf-pyro-retire".into())
                .spawn(move || {
                    for r in rx {
                        if r.timeline.wait(r.turn, Self::PATIENCE).is_err() {
                            continue;
                        }
                        last.store(r.submitted.elapsed().as_micros() as u64, Ordering::Relaxed);
                        if let Some((pts_ns, received_ns)) = r.hud {
                            let decoded_ns = now_realtime_ns();
                            let us = crate::stats::sane_us(decoded_ns - received_ns);
                            stats.note_decoded(pts_ns, decoded_ns, us);
                        }
                    }
                })?;
            Ok(Retire {
                tx: Some(tx),
                thread: Some(thread),
                gpu_us,
            })
        }

        fn send(&self, r: Retiring) {
            if let Some(tx) = &self.tx {
                let _ = tx.send(r);
            }
        }

        fn gpu_us(&self) -> u64 {
            self.gpu_us.load(Ordering::Relaxed)
        }
    }

    impl Drop for Retire {
        fn drop(&mut self) {
            drop(self.tx.take()); // the thread drains what is queued, then ends
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Build a decoder for the session's current mode.
    fn open_decoder(
        client: &NativeClient,
        dev: &PyroDevice,
        mode: punktfunk_core::config::Mode,
    ) -> Result<PyroWaveDecoder> {
        // The wavelet bitstream carries no VUI, so the colour contract is the handshake's.
        let color = ColorDesc {
            primaries: client.color.primaries,
            transfer: client.color.transfer,
            matrix: client.color.matrix,
            full_range: client.color.full_range != 0,
        };
        PyroWaveDecoder::new(
            &dev.vkd,
            mode.width,
            mode.height,
            client.shard_payload as usize,
            client.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
            color,
            client.bit_depth >= 10,
        )
    }

    /// Have `decoder` read AUs where the session assembles them, and lend its ring to the
    /// session. A device without the memory for it keeps copying. Returns what the session holds.
    fn read_in_place(
        client: &NativeClient,
        decoder: &mut PyroWaveDecoder,
        dev: &Arc<PyroDevice>,
        lent: Option<Arc<dyn AuMemory>>,
    ) -> Option<Arc<dyn AuMemory>> {
        if let Err(e) = decoder.enable_in_place(dev.clone()) {
            log::warn!("pyro: packets are copied into the decoder ({e:#})");
            return lent;
        }
        log::info!("pyro: the decoder reads AUs where the reassembler wrote them");
        lend(client, decoder, lent)
    }

    /// Hand the session `decoder`'s ring when it is not the one it already holds: a resize can
    /// outgrow the old one.
    fn lend(
        client: &NativeClient,
        decoder: &PyroWaveDecoder,
        lent: Option<Arc<dyn AuMemory>>,
    ) -> Option<Arc<dyn AuMemory>> {
        match decoder.au_memory() {
            Some(m) if !lent.as_ref().is_some_and(|l| Arc::ptr_eq(l, &m)) => {
                client.set_au_memory(m.clone());
                Some(m)
            }
            _ => lent,
        }
    }

    /// The lane's entry point on the `pf-decode` thread. Runs until `shutdown` is set, the
    /// session closes, or bring-up fails — the last of which is terminal for video: nothing
    /// else on this client decodes PyroWave, and the host already negotiated it.
    pub(crate) fn run(
        client: Arc<NativeClient>,
        window: NativeWindow,
        shutdown: Arc<AtomicBool>,
        stats: Arc<crate::stats::VideoStats>,
        opts: DecodeOptions,
    ) {
        let result = run_inner(&client, &window, &shutdown, &stats, &opts);
        // Back to heap memory: the next lane lends its own ring, and this one can go.
        client.set_au_memory(Arc::new(punktfunk_core::au::HeapPool::default()));
        if let Err(e) = result {
            if shutdown.load(Ordering::Relaxed) {
                // Teardown, not a fault: Kotlin sets the flag and releases the Surface in the
                // same breath, so whatever we were mid-call on lost its window under us.
                log::info!("pyro: stopping ({e:#})");
                return;
            }
            // Say it in the shape the reports arrive in: the session stays up around a dead
            // video plane (audio, input and the library all keep working), so "the stream
            // looks alive but the screen is black" is the symptom this line has to explain.
            log::error!(
                "pyro: PyroWave video failed: {e:#} — this session has no picture. Audio and \
                 input keep working, so the stream will look alive while the screen stays \
                 black. Pick a different codec in Settings to recover."
            );
        }
    }

    fn run_inner(
        client: &Arc<NativeClient>,
        window: &NativeWindow,
        shutdown: &Arc<AtomicBool>,
        stats: &Arc<crate::stats::VideoStats>,
        opts: &DecodeOptions,
    ) -> Result<()> {
        crate::decode::boost_thread_priority();
        // Shared: a payload-ring slot still lent to the session keeps the device alive.
        let dev = Arc::new(PyroDevice::new(PyroDevice::open_surface(window)?)?);
        stats.set_decoder(&decoder_label(&dev.vkd.device_name), false);
        let smooth = opts.present_priority == 1;
        // The compositor takes our buffers where it can: no acquire, its own scaling. The
        // swapchain stays for the smoothness intent (FIFO), ChromeOS and drivers without import.
        let asc = if !smooth && dev.buffers && crate::decode::asc_backend_selected(opts.chromeos) {
            asc::AscTarget::new(
                &dev,
                window,
                opts.surface_size.clone(),
                opts.src_crop.clone(),
                client.mode().refresh_hz as f32,
                opts.force_rate_switch(),
            )
            .unwrap_or_else(|e| {
                log::warn!("pyro: presenting through the swapchain ({e:#})");
                None
            })
        } else {
            None
        };
        let mut present = Present::new(&dev, smooth, asc)?;

        let mut mode = client.mode();
        let mut decoder = open_decoder(client, &dev, mode)?;
        let mut lent = read_in_place(client, &mut decoder, &dev, None);
        // Declared after the decoder, so it is joined first on every exit.
        let retire = Retire::spawn(stats.clone())?;
        let depth = if client.bit_depth >= 10 { 10 } else { 8 };
        let msb_packed = client.bit_depth >= 10;
        log::info!(
            "pyro: {}x{} {}-bit {}, shard payload {} — decoding",
            mode.width,
            mode.height,
            depth,
            if client.chroma_format == punktfunk_core::quic::CHROMA_IDC_444 {
                "4:4:4"
            } else {
                "4:2:0"
            },
            client.shard_payload
        );

        let clock_offset = client.clock_offset_shared();
        let video_e2e = client.video_e2e_shared();
        let measure_decode = client.wants_decode_latency();
        // Nothing-ever-arrived backstop, the one the MediaCodec loops also keep: a session
        // that receives no AU at all sits connected behind a black surface, and this line is
        // what separates that from "we received AUs and could not decode them". No keyframe
        // request rides along — every PyroWave frame is already a keyframe, so there is
        // nothing for the host to re-send.
        let started = Instant::now();
        let mut received: u64 = 0;
        let mut warned_silent = false;
        let mut split = SplitWindow::default();
        // ADPF, as the MediaCodec loop has it: this thread joins the hinted set now, and the
        // session opens on the first shown frame, once the pump has registered too.
        client.register_hot_thread();
        let mut hint: Option<crate::adpf::HintSession> = None;
        let mut hint_tried = false;

        while !shutdown.load(Ordering::Relaxed) {
            // Live mode switch (`NativeClient::request_mode`, or the host's own resize): the
            // decoder is fixed-size, so rebuild it. The device, surface and pipeline are all
            // dimension-independent and stay; the swapchain follows the view, not the stream
            // ([`Present::follow`]).
            let now_mode = client.mode();
            if now_mode.width != mode.width || now_mode.height != mode.height {
                log::info!(
                    "pyro: mode {}x{} -> {}x{}, rebuilding the decoder",
                    mode.width,
                    mode.height,
                    now_mode.width,
                    now_mode.height
                );
                // Drop first: the old decoder's plane ring is freed before the new one
                // allocates, which matters on a 4 GB TV box at 4K.
                drop(decoder);
                mode = now_mode;
                decoder = open_decoder(client, &dev, mode)?;
                lent = read_in_place(client, &mut decoder, &dev, lent);
                split = SplitWindow::default();
            }

            let frame = match client.next_frame(Duration::from_millis(5)) {
                Ok(f) => f,
                Err(PunktfunkError::NoFrame) => {
                    if received == 0 && !warned_silent && started.elapsed() > NO_VIDEO_PATIENCE {
                        warned_silent = true;
                        log::warn!(
                            "pyro: no video after {:?} — the session is connected but the host \
                             has sent no access unit",
                            started.elapsed()
                        );
                    }
                    continue;
                }
                Err(_) => break, // session closed
            };
            received += 1;
            let _ = client.note_frame_index(frame.frame_index);
            let work_started = (Instant::now(), crate::sys::now_monotonic_ns());

            // Receipt stamp: core's reassembly-completion time, not the pull instant — a
            // pull stamp folds this loop's own poll wait into "network". Noting it drains the
            // host timings, which the overlay's host and network figures are built from.
            let want_stamps = stats.enabled() || measure_decode;
            let received_ns = if want_stamps {
                note_received_frame(client, &frame)
            } else {
                0
            };
            let pulled_ns = if want_stamps { now_realtime_ns() } else { 0 };

            let aligned = frame.flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
            let decoded = match decoder.decode_au(frame.data, aligned, frame.complete) {
                Ok(d) => d,
                Err(e) => {
                    // Not recoverable by asking for anything: an intra-only frame that the
                    // GPU refused is a codec or driver fault, not a lost reference.
                    return Err(anyhow!("decode failed: {e:#}"));
                }
            };
            let Some(picture) = decoded else {
                continue; // packets accumulated, frame not complete yet
            };
            lent = lend(client, &decoder, lent);
            // Submitted, not done: the present below follows it on the same queue, and the
            // retire thread notices when it finishes.
            let submitted = Instant::now();
            let submitted_ns = if want_stamps { now_realtime_ns() } else { 0 };
            retire.send(Retiring {
                timeline: decoder.timeline(),
                turn: picture.turn,
                submitted,
                hud: stats.enabled().then_some((frame.pts_ns, received_ns)),
            });

            opts.decoded_size.store(
                crate::session::pack_surface_size(picture.width as i32, picture.height as i32),
                std::sync::atomic::Ordering::Relaxed,
            );
            let crop = crate::session::unpack_src_crop(
                opts.src_crop.load(std::sync::atomic::Ordering::Relaxed),
            );
            present.follow(opts.surface_size.load(Ordering::Relaxed))?;
            let size = (picture.width, picture.height);
            let shown =
                present.show(picture.views, crop, picture.color, depth, msb_packed, size)?;
            if !shown {
                continue; // swapchain was out of date; it has been rebuilt
            }
            if !hint_tried {
                hint_tried = true;
                hint = crate::decode::start_hint(client, mode.refresh_hz, opts.low_latency_mode);
            }
            if let Some(h) = &hint {
                // The GPU half is what the governor under-clocks; reporting it asks for clock.
                // The critical path is CPU up to the decode submit, then the GPU (latest seen).
                let cpu = work_started.0.elapsed().as_nanos() as i64;
                let gpu = retire.gpu_us() as i64 * 1000;
                let to_submit = submitted.duration_since(work_started.0).as_nanos() as i64;
                h.report_work(work_started.1, to_submit + gpu, cpu, gpu);
            }

            // No latch callback on this path, so the HUD drops the `display` stage. `decode`
            // ends when the GPU finishes, as on desktop (stamped by `retire`); present time is
            // in the split log. E2E is stamped whatever the HUD state: the audio plane steers
            // by `video_e2e`.
            let presented_ns = now_realtime_ns();
            let offset = clock_offset.load(Ordering::Relaxed);
            crate::stats::publish_e2e(&video_e2e, presented_ns, offset, frame.pts_ns);
            if stats.enabled() {
                let us = |a: i128, b: i128| crate::stats::sane_us(b - a).unwrap_or(0);
                split.add(
                    &decoder,
                    us(received_ns, pulled_ns),
                    retire.gpu_us(),
                    us(submitted_ns, presented_ns),
                );
            } else {
                split = SplitWindow::default();
            }
        }

        // Every exit path tears down in reverse declaration order, which is Vulkan's: the
        // retire thread, the decoder, the present half, the device, then surface and instance.
        log::info!("pyro: lane stopped after {received} access units");
        Ok(())
    }
}

#[cfg(all(target_os = "android", target_pointer_width = "64"))]
pub(crate) use imp::{available, run};

/// Off 64-bit Android the codec is not built (see the module docs), so the lane is absent and
/// the client never advertises `CODEC_PYROWAVE`. Ungated — `nativePyrowaveCapable` is one of
/// the JNI entry points that links into the host workspace build too.
#[cfg(not(all(target_os = "android", target_pointer_width = "64")))]
pub(crate) fn available() -> bool {
    false
}

/// 32-bit Android only: unreachable behind [`available`], and here so the dispatch in
/// `crate::decode` needs no `cfg` of its own. Not built off Android at all, where there is no
/// `ndk` crate to name the window with (and no decode module to call it).
#[cfg(all(target_os = "android", not(target_pointer_width = "64")))]
pub(crate) fn run(
    _client: std::sync::Arc<punktfunk_core::client::NativeClient>,
    _window: ndk::native_window::NativeWindow,
    _shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _stats: std::sync::Arc<crate::stats::VideoStats>,
    _opts: crate::decode::DecodeOptions,
) {
    log::error!("pyro: PyroWave is not built for this ABI — the session has no video");
}
