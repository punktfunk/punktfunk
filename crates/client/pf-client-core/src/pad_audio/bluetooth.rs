//! The Bluetooth sink: report `0x36` on a raw HID handle beside SDL's, one per block on a local
//! clock. The speaker pair is Opus-encoded here; [`pad_bt`](punktfunk_core::audio::pad_bt)
//! shapes the rest of the report.

use super::{haptics_live, rumble_now};
use crate::sc2_capture::Dev;
use anyhow::{anyhow, bail, Context};
use punktfunk_core::audio::pad_bt::{
    media_state, speaker_block, HapticsDecimator, MediaReport, BT_BLOCK_FRAMES, BT_OPUS_BITRATE,
    BT_OPUS_BYTES, BT_REPORT_PERIOD, BT_SPEAKER_FRAMES,
};
use punktfunk_core::audio::pad_mix::PAD_CHANNELS;
use punktfunk_core::quic::HID_RAW_OUTPUT;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Instant;

/// Buffered frames before the first report: one block of jitter slack past the one being sent.
const PRIME_FRAMES: usize = BT_BLOCK_FRAMES * 2;
/// Backlog past this drops the oldest frames: a host clock running ahead of ours.
const CAP_FRAMES: usize = PRIME_FRAMES + BT_BLOCK_FRAMES;
/// Consecutive refused writes, about half a second, that mean the pad is gone.
const MAX_REFUSED: u32 = 48;

/// `finished()` is pad-gone: the renderer drops it and re-correlates.
pub(super) struct BtOut {
    pcm_tx: SyncSender<Vec<f32>>,
    recycle_rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BtOut {
    /// `hid_path` is the SDL slot's node; `pad` the wire pad whose rumble the reports carry.
    pub(super) fn open(hid_path: &str, pad: u8) -> anyhow::Result<BtOut> {
        let dev = Dev::open(hid_path)
            .ok_or_else(|| anyhow!("open pad hid node {hid_path}: {}", sdl3::get_error()))?;
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .context("pad speaker opus encoder")?;
        enc.set_bitrate(opus::Bitrate::Bits(BT_OPUS_BITRATE))
            .and_then(|()| enc.set_vbr(false))
            .and_then(|()| enc.set_complexity(0))
            .context("configure pad speaker opus encoder")?;
        // 64 × 5 ms slack; the recycle pool keeps steady state allocation-free.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-bt".into())
            .spawn(move || {
                crate::audio_rt::boost_and_log("pf-pad-audio-bt");
                if let Err(e) = write_loop(&dev, &mut enc, pad, &pcm_rx, &recycle_tx, &stop_t) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio bluetooth writer ended");
                }
            })
            .context("spawn pad-audio bluetooth writer")?;
        Ok(BtOut {
            pcm_tx,
            recycle_rx,
            stop,
            thread: Some(thread),
        })
    }

    pub(super) fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    pub(super) fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for BtOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One report per [`BT_REPORT_PERIOD`] while primed. An empty ring stops the reports until the
/// floor refills; a write that runs late restarts the clock instead of bursting.
fn write_loop(
    dev: &Dev,
    enc: &mut opus::Encoder,
    pad: u8,
    pcm_rx: &Receiver<Vec<f32>>,
    recycle_tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let mut ring: VecDeque<f32> = VecDeque::new();
    let mut coils = HapticsDecimator::default();
    let mut packer = MediaReport::default();
    let mut block = vec![0f32; BT_BLOCK_FRAMES * PAD_CHANNELS];
    let mut speaker = [0f32; BT_SPEAKER_FRAMES * 2];
    let mut opus = [0u8; BT_OPUS_BYTES];
    let absorb = |ring: &mut VecDeque<f32>, mut chunk: Vec<f32>| {
        ring.extend(chunk.drain(..));
        let _ = recycle_tx.try_send(chunk);
    };
    let mut due: Option<Instant> = None;
    let mut refused = 0u32;
    while !stop.load(Ordering::Relaxed) {
        match due {
            Some(t) => std::thread::sleep(t.saturating_duration_since(Instant::now())),
            None => match pcm_rx.recv_timeout(BT_REPORT_PERIOD) {
                Ok(chunk) => absorb(&mut ring, chunk),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        }
        while let Ok(chunk) = pcm_rx.try_recv() {
            absorb(&mut ring, chunk);
        }
        let frames = ring.len() / PAD_CHANNELS;
        if frames > CAP_FRAMES {
            ring.drain(..(frames - CAP_FRAMES) * PAD_CHANNELS);
        }
        let at = match due {
            Some(t) => t,
            None if frames >= PRIME_FRAMES => Instant::now(),
            None => continue,
        };
        let n = ring.len().min(block.len());
        for (d, s) in block.iter_mut().zip(ring.drain(..n)) {
            *d = s;
        }
        block[n..].fill(0.0);

        speaker_block(&block, &mut speaker);
        let len = enc
            .encode_float(&speaker, &mut opus)
            .context("encode pad speaker")?;
        opus[len..].fill(0);
        let rumble = if haptics_live(pad) {
            None
        } else {
            rumble_now(pad)
        };
        let report = packer.pack(&media_state(rumble), &coils.block(&block), &opus);
        if dev.write(HID_RAW_OUTPUT, &report) < 0 {
            refused += 1;
            if refused >= MAX_REFUSED {
                bail!("pad refused {refused} reports: {}", sdl3::get_error());
            }
        } else {
            refused = 0;
        }

        let next = at + BT_REPORT_PERIOD;
        let now = Instant::now();
        due = if ring.is_empty() {
            None
        } else if now > next + BT_REPORT_PERIOD {
            Some(now)
        } else {
            Some(next)
        };
    }
    Ok(())
}
