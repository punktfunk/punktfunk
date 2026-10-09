//! Linux: capture the per-pad PipeWire sink (`crate::audio::pad_sink`), or the usbip pad's
//! real ALSA card (`crate::audio::pad_usb`) when that transport owns the pad.

use super::engine::{pad_audio_thread, KIND_BIT_HAPTICS, KIND_BIT_SPEAKER};
use super::*;

/// A source exists when the PipeWire daemon is reachable (sinks mint lazily at spawn).
pub(super) fn host_cap(asked: bool) -> bool {
    asked
        && pf_host_config::row_bool("PUNKTFUNK_PAD_AUDIO")
        && crate::audio::pad_sink::pipewire_reachable()
}

/// Mutually exclusive: a usbip pad already owns a real ALSA card; minting sinks beside it
/// duplicates the node graph.
enum LinuxPadCapture {
    Usb(crate::audio::pad_usb::PadUsbCapturer),
    /// UHID pad: mint the sinks a real card would have had.
    Sink(crate::audio::pad_sink::PadSinkCapturer),
}

impl crate::audio::AudioCapturer for LinuxPadCapture {
    fn next_chunk(&mut self) -> anyhow::Result<Vec<f32>> {
        match self {
            LinuxPadCapture::Usb(c) => c.next_chunk(),
            LinuxPadCapture::Sink(c) => c.next_chunk(),
        }
    }

    fn next_chunk_within(&mut self, budget: std::time::Duration) -> anyhow::Result<Vec<f32>> {
        match self {
            LinuxPadCapture::Usb(c) => c.next_chunk_within(budget),
            LinuxPadCapture::Sink(c) => c.next_chunk_within(budget),
        }
    }

    fn channels(&self) -> u32 {
        match self {
            LinuxPadCapture::Usb(c) => c.channels(),
            LinuxPadCapture::Sink(c) => c.channels(),
        }
    }
}

/// Open capture lazily on the streamer thread (open-with-backoff, same as Windows). `edge`
/// selects DualSense Edge for a minted sink. `None` only for empty kinds, a slot past
/// `PUNKTFUNK_PAD_AUDIO_SLOTS`, or spawn failure; the pad still works, without audio.
///
/// Capture follows the pad transport flag, not whether a stream is published yet — otherwise
/// the race between pad arrival and this thread would mint a duplicate node graph over a
/// real usbip card.
///
/// `pad` addresses the client's wire pad; `slot` is the host-wide OS slot the usbip card and
/// the minted node graph are both named by (the sink carries the slot's pad MAC). They differ
/// whenever a session's pads arrive out of wire order, and every host serving two sessions.
pub(in crate::native) fn spawn(
    conn: super::link::SessionLink,
    pad: u8,
    slot: u8,
    kinds: u8,
    edge: bool,
    stop: Arc<AtomicBool>,
) -> Option<PadAudioHandle> {
    if kinds & (KIND_BIT_HAPTICS | KIND_BIT_SPEAKER) == 0 {
        return None;
    }
    if slot >= crate::audio::pad_sink::pad_audio_slots() {
        tracing::debug!(
            pad,
            slot,
            "pad-audio slot past PUNKTFUNK_PAD_AUDIO_SLOTS — not streaming"
        );
        return None;
    }
    let stop_t = stop.clone();
    let usb = pf_inject::dualsense_usbip::usbip_preferred();
    match std::thread::Builder::new()
        .name(format!("punktfunk1-pad{pad}"))
        .spawn(move || {
            pad_audio_thread(
                conn,
                pad,
                kinds,
                move || {
                    if usb {
                        crate::audio::pad_usb::PadUsbCapturer::open(slot).map(LinuxPadCapture::Usb)
                    } else {
                        crate::audio::pad_sink::PadSinkCapturer::open(slot, edge)
                            .map(LinuxPadCapture::Sink)
                    }
                },
                stop_t,
            )
        }) {
        Ok(join) => Some(PadAudioHandle {
            stop,
            join: Some(join),
        }),
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio thread spawn failed — pad streams without audio");
            None
        }
    }
}
