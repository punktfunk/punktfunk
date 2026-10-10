//! USB-layer pad audio capturer — the [`AudioCapturer`] face of the channel
//! [`pf_inject::dualsense_usbip`] publishes. Counterpart to [`super::pad_sink`]
//! for a pad that is a real USB device rather than a minted PipeWire graph.
//!
//! The pad arrives over `vhci_hcd` with its own USB Audio Class card.
//! `snd-usb-audio` and PipeWire's ALSA monitor build the DualSense UCM sinks;
//! every game route converges on the isochronous OUT endpoint. Capture there
//! so any route lands, no impersonated graph needs keeping faithful, and wine
//! can derive a matching ContainerId for pad and speaker (see the usbip crate).
//!
//! Decode lives in the USB handler. This type only owns the published channel.

use crate::AudioCapturer;
use anyhow::{anyhow, Result};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Idle wait matching [`super::pad_sink::PadSinkCapturer`] so a quiet pad
/// behaves the same on both transports.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Interleaved `f32` quad frames lifted straight off the pad's isochronous OUT endpoint.
pub struct PadUsbCapturer {
    rx: Receiver<Vec<f32>>,
    pad: u8,
}

/// Hardware quad → 0xD1 wire: duplicate speaker `ch1` onto `ch0`, pass coils.
/// DualSense ISO OUT is `[hpL, hpR+speaker, coilA, coilB]`; the wire speaker
/// pair is ch0/1. Headphone-left is not a remote-pad surface.
fn normalize_hw_quad(mut chunk: Vec<f32>) -> Vec<f32> {
    for frame in chunk.chunks_exact_mut(4) {
        frame[0] = frame[1];
    }
    chunk
}

impl PadUsbCapturer {
    /// Claim the USB audio stream of the pad at OS slot `pad` — the index pf-inject
    /// attached the card under, not the client's wire numbering.
    ///
    /// Fails until usbip publishes one (normal between thread start and attach).
    /// The streamer's open-with-backoff retries, so this is a late start, not silence.
    pub fn open(pad: u8) -> Result<PadUsbCapturer> {
        let rx = pf_inject::dualsense_usbip::take_audio_rx(pad)
            .ok_or_else(|| anyhow!("no usbip pad audio published for pad {pad} (not attached?)"))?;
        tracing::info!(pad, "pad audio capturing from the USB isochronous endpoint");
        // The pad's ALSA card is real, so WirePlumber greets it with its global 40 % default —
        // which is -23.88 dB applied BEFORE the isochronous endpoint we capture from, and which
        // stacks with the same default on the client. Undo it once the card shows up; see
        // [`super::pad_card_volume`]. Best effort, off-thread, never fatal.
        super::pad_card_volume::spawn_pin(pad);
        Ok(PadUsbCapturer { rx, pad })
    }
}

impl AudioCapturer for PadUsbCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(IDLE_TIMEOUT)
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.rx.recv_timeout(budget.min(IDLE_TIMEOUT)) {
            Ok(chunk) => Ok(normalize_hw_quad(chunk)),
            // Quiet pad (game not writing), not a dead one — same as the sink capturer.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            // The sender lives inside the attached USB device, so this can only mean the pad went
            // away. Err tells the streamer to reopen, which is what re-arrival should do.
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!(
                "usbip pad {} detached — audio endpoint gone",
                self.pad
            )),
        }
    }

    fn channels(&self) -> u32 {
        pf_inject::dualsense_usbip::PAD_AUDIO_CHANNELS as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn capturer() -> (std::sync::mpsc::SyncSender<Vec<f32>>, PadUsbCapturer) {
        let (tx, rx) = sync_channel(4);
        (tx, PadUsbCapturer { rx, pad: 0 })
    }

    /// Channel count the 0xD1 framer splits into speaker (ch0/1) and coils (ch2/3).
    #[test]
    fn reports_the_hardware_quad() {
        assert_eq!(capturer().1.channels(), 4);
    }

    /// Empty chunk, not an error: the streamer keeps a capturer through silence.
    #[test]
    fn silence_is_an_empty_chunk_not_an_error() {
        let (_tx, mut c) = capturer();
        let got = c
            .next_chunk_within(Duration::from_millis(10))
            .expect("silence must not error");
        assert!(got.is_empty());
    }

    /// `Err` so the streamer reopens instead of spinning on a dead channel.
    #[test]
    fn detach_surfaces_as_an_error() {
        let (tx, mut c) = capturer();
        drop(tx);
        assert!(c.next_chunk_within(Duration::from_millis(10)).is_err());
    }

    /// Verbatim hardware quads put the speaker on wire ch1 only; the client's
    /// split-sink never reaches the physical speaker.
    #[test]
    fn normalizes_the_hardware_quad_to_the_wire_layout() {
        let (tx, mut c) = capturer();
        tx.send(vec![0.9, 0.5, 0.25, -0.25, 0.8, 0.4, 0.2, -0.2])
            .expect("send");
        assert_eq!(
            c.next_chunk_within(Duration::from_millis(50))
                .expect("chunk"),
            vec![0.5, 0.5, 0.25, -0.25, 0.4, 0.4, 0.2, -0.2]
        );
    }

    #[test]
    fn normalize_duplicates_the_speaker_channel() {
        assert_eq!(
            normalize_hw_quad(vec![0.9, 0.5, 0.25, -0.25]),
            vec![0.5, 0.5, 0.25, -0.25]
        );
    }
}
