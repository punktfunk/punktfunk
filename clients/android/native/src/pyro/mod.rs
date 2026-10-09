//! The PyroWave decode lane (android-only): Vulkan compute decode + present, beside the
//! MediaCodec path rather than inside it.
//!
//! PyroWave is the opt-in wired-LAN wavelet codec (`design/pyrowave-codec-plan.md`). It
//! shares nothing with the other three codecs on this client: no `AMediaCodec`, no
//! `AImageReader`, no `ASurfaceControl` layer — the access unit is one self-delimiting
//! packet stream decoded by GPU compute into three Y′CbCr planes, and those planes are
//! sampled straight into a Vulkan swapchain on the same `SurfaceView`
//! ([`present`]). The decoder itself is the SHARED one the Linux and Windows clients use
//! (`pf_client_core::video_pyrowave`); only the device and the present path are ours.
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
mod device;
#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod present;

#[cfg(all(target_os = "android", target_pointer_width = "64"))]
mod imp {
    use super::{device::PyroDevice, present::Present};
    use anyhow::{anyhow, Result};
    use ndk::native_window::NativeWindow;
    use pf_client_core::video_color::ColorDesc;
    use pf_client_core::video_pyrowave::PyroWaveDecoder;
    use punktfunk_core::client::NativeClient;
    use punktfunk_core::error::PunktfunkError;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::decode::{now_realtime_ns, DecodeOptions, NO_VIDEO_PATIENCE};

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
        if let Err(e) = run_inner(&client, &window, &shutdown, &stats, &opts) {
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
        let dev = PyroDevice::new(PyroDevice::open_surface(window)?)?;
        stats.set_decoder(&decoder_label(&dev.vkd.device_name), false);
        let smooth = opts.present_priority == 1;
        let mut present = Present::new(&dev, smooth)?;

        let mut mode = client.mode();
        let mut decoder = open_decoder(client, &dev, mode)?;
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

        while !shutdown.load(Ordering::Relaxed) {
            // Live mode switch (`NativeClient::request_mode`, or the host's own resize): the
            // decoder is fixed-size, so rebuild it. The device, surface and pipeline are all
            // dimension-independent and stay; the swapchain follows the window, not the
            // stream, and is rebuilt by its own out-of-date path.
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

            // Receipt stamp: core's reassembly-completion time, not the pull instant — a
            // pull stamp folds this loop's own poll wait into "network".
            let want_stamps = stats.enabled() || measure_decode;
            let received_ns = if !want_stamps {
                0
            } else if frame.received_ns > 0 {
                frame.received_ns as i128
            } else {
                now_realtime_ns()
            };

            let aligned = frame.flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
            let decoded = match decoder.decode_frame(&frame.data, aligned, frame.complete) {
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

            opts.decoded_size.store(
                crate::session::pack_surface_size(picture.width as i32, picture.height as i32),
                std::sync::atomic::Ordering::Relaxed,
            );
            let crop = crate::session::unpack_src_crop(
                opts.src_crop.load(std::sync::atomic::Ordering::Relaxed),
            );
            let shown = present.show(picture.views, crop, picture.color, depth, msb_packed)?;
            if !shown {
                continue; // swapchain was out of date; it has been rebuilt
            }

            // Presented. Stamped here rather than at a latch callback: this path has no
            // render-timestamp signal (that is the ASurfaceControl backend's), so the HUD
            // shows the capture→decoded headline and drops the `display` stage. Stamped
            // whatever the HUD state: the audio plane steers by `video_e2e`.
            let presented_ns = now_realtime_ns();
            let offset = clock_offset.load(Ordering::Relaxed);
            crate::stats::publish_e2e(&video_e2e, presented_ns, offset, frame.pts_ns);
            if stats.enabled() {
                let decode_us = crate::stats::sane_us(presented_ns - received_ns);
                stats.note_decoded(frame.pts_ns, presented_ns, decode_us);
            }
        }

        // Every exit path tears down in reverse declaration order, which is Vulkan's: the
        // decoder, the present half, the device, then the surface and instance.
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
