//! The client's mode, moved by the host's `StreamConfig` at the first frame of the epoch it names,
//! and each epoch's video format. A `Reconfigured` ack says a switch was accepted; the config
//! says what each epoch delivers.

use super::{PumpEvent, PumpTx};
use crate::config::Mode;
use crate::quic::v2::msg::StreamConfig;
use crate::quic::VideoFormat;
use std::sync::Mutex;

/// Epochs whose format is kept. The host moves one epoch per pipeline swap, so a frame never
/// trails its config by this many.
const BOOKED: usize = 8;

#[derive(Debug, Default)]
pub(crate) struct EpochAnchor {
    /// The host sends a config per epoch (`FEATURE_STREAM_CONFIG` in force).
    pub(crate) on: bool,
    /// An epoch may change the format (`FEATURE_FORMAT_EPOCH` in force): frames wait for
    /// their epoch's config before they go to the decoder.
    pub(crate) formats: bool,
    /// A config whose epoch has shown no frame yet.
    pending: Option<StreamConfig>,
    /// The newest epoch a frame carried.
    seen: Option<u8>,
    /// The format of each recent epoch, newest last.
    booked: Vec<(u8, VideoFormat)>,
    /// The mode ABR prices: the delivered refresh at the size the host encodes. The pump seeds
    /// it from the handshake.
    pub(crate) priced: Mode,
}

/// What an epoch delivers: its mode, and the mode ABR prices when that moved.
pub(crate) type Delivery = (Mode, Option<Mode>);

impl EpochAnchor {
    /// A config arrived. `Some` when its epoch is already on screen.
    pub(crate) fn config(&mut self, cfg: StreamConfig) -> Option<Delivery> {
        self.book(cfg.epoch, cfg.format());
        if self.seen == Some(cfg.epoch) {
            self.pending = None;
            return Some(self.deliver(&cfg));
        }
        self.pending = Some(cfg);
        None
    }

    /// The first frame of `epoch` arrived. `Some` when its config came first.
    pub(crate) fn frame(&mut self, epoch: u8) -> Option<Delivery> {
        self.seen = Some(epoch);
        let cfg = self.pending.take_if(|c| c.epoch == epoch)?;
        Some(self.deliver(&cfg))
    }

    /// `cfg`'s mode, and its encoded picture at that refresh when it moved [`Self::priced`].
    fn deliver(&mut self, cfg: &StreamConfig) -> Delivery {
        let (width, height) = cfg.picture.unwrap_or((cfg.mode.width, cfg.mode.height));
        let priced = Mode {
            width,
            height,
            ..cfg.mode
        };
        let moved = std::mem::replace(&mut self.priced, priced) != priced;
        (cfg.mode, moved.then_some(priced))
    }

    /// `epoch` decodes as `format` from here on. A later booking of the same epoch replaces it.
    pub(crate) fn book(&mut self, epoch: u8, format: VideoFormat) {
        self.booked.retain(|(e, _)| *e != epoch);
        if self.booked.len() == BOOKED {
            self.booked.remove(0);
        }
        self.booked.push((epoch, format));
    }

    /// `epoch`'s config never came: it decodes as the newest epoch that has one.
    pub(crate) fn assume(&mut self, epoch: u8) {
        if let Some(&(_, f)) = self.booked.last() {
            self.book(epoch, f);
        }
    }

    /// `epoch`'s format, `None` before its config arrived.
    pub(crate) fn format(&self, epoch: u8) -> Option<VideoFormat> {
        self.booked
            .iter()
            .rev()
            .find_map(|(e, f)| (*e == epoch).then_some(*f))
    }

    /// A frame of `epoch` may go to the decoder: no format moves, or its epoch's is known.
    pub(crate) fn ready(&self, epoch: u8) -> bool {
        !self.formats || self.format(epoch).is_some()
    }
}

/// Under format epochs, mark the first frame handed out whose epoch decodes as another format
/// than the frame before it, and make that format the live one.
pub(crate) fn mark(shared: &super::super::ClientShared, f: &mut crate::session::Frame) {
    let Some(format) = ({
        let anchor = shared.anchor.lock().unwrap_or_else(|e| e.into_inner());
        anchor.formats.then(|| anchor.format(f.epoch)).flatten()
    }) else {
        return;
    };
    let mut live = shared
        .video_format
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if *live != format {
        *live = format;
        f.new_format = Some(format);
        tracing::info!(
            epoch = f.epoch,
            codec = crate::hud::codec_label(format.codec),
            bit_depth = format.bit_depth,
            transfer = format.color.transfer,
            chroma = format.chroma_format,
            "the host changed the video format — rebuilding the decoder"
        );
    }
}

/// The desktop audio stream's format, moved by an `AudioConfig` at its `first_seq`. The audio
/// datagram names its codec by tag but carries no epoch, so the sequence anchors the change.
#[derive(Debug, Default)]
pub(crate) struct AudioAnchor {
    /// `FEATURE_AUDIO_EPOCH` is in force.
    pub(crate) on: bool,
    /// The format packets are handed out in; `None` before the handshake.
    pub(crate) live: Option<crate::audio::plane::PlaneFormat>,
    /// The next format and the first sequence it covers.
    pending: Option<(u32, crate::audio::plane::PlaneFormat)>,
}

/// What becomes of one desktop audio packet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AudioAdmit {
    /// Hand it on as it is.
    Pass,
    /// Hand it on marked: it opens this format.
    New(crate::audio::plane::PlaneFormat),
    /// Its codec is not the live format's and no config for it has come yet.
    Drop,
}

impl AudioAnchor {
    /// An `AudioConfig` for the desktop stream arrived.
    pub(crate) fn config(&mut self, cfg: &crate::quic::v2::msg::AudioConfig) {
        let format = crate::audio::plane::PlaneFormat::from_welcome(
            cfg.codec,
            cfg.rate_hz,
            cfg.bits,
            cfg.channels,
            u32::from(cfg.frame_us),
            cfg.layout,
        );
        self.pending = Some((cfg.first_seq, format));
    }

    /// Packet `seq` arrived carrying `codec` (`AUDIO_CODEC_*`, from its tag). It passes only
    /// into a format of its tag's family: `0xD4` cannot tell AC-3 from E-AC-3, and need not.
    pub(crate) fn admit(&mut self, seq: u32, codec: u8) -> AudioAdmit {
        if !self.on {
            return AudioAdmit::Pass;
        }
        if let Some((first, format)) = self.pending {
            if seq.wrapping_sub(first) < u32::MAX / 2 {
                self.pending = None;
                self.live = Some(format);
                return if family(format.codec) == family(codec) {
                    AudioAdmit::New(format)
                } else {
                    AudioAdmit::Drop
                };
            }
        }
        match self.live {
            Some(live) if family(live.codec) != family(codec) => AudioAdmit::Drop,
            _ => AudioAdmit::Pass,
        }
    }
}

/// The plane a codec rides, named by one codec on it: AC-3 stands for both bitstream codecs.
/// An unknown codec is Opus.
fn family(codec: u8) -> u8 {
    match codec {
        crate::quic::AUDIO_CODEC_PCM => crate::quic::AUDIO_CODEC_PCM,
        c if crate::audio::bitstream::is_bitstream(c) => crate::quic::AUDIO_CODEC_AC3,
        _ => crate::quic::AUDIO_CODEC_OPUS,
    }
}

/// Point the mode slot at `mode`. A moved priced picture tells the pump, which resets
/// mode-scoped ABR state.
pub(super) fn apply(slot: &Mutex<Mode>, events: &PumpTx, (mode, priced): Delivery) {
    let mut m = slot.lock().unwrap_or_else(|e| e.into_inner());
    if *m != mode {
        *m = mode;
        tracing::info!(?mode, "host delivered mode switch");
    }
    if let Some(priced) = priced {
        let _ = events.send(PumpEvent::Mode(priced));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{CODEC_H264, CODEC_HEVC};

    fn cfg(epoch: u8, width: u32) -> StreamConfig {
        StreamConfig {
            epoch,
            mode: Mode {
                width,
                height: 720,
                refresh_hz: 60,
            },
            ..StreamConfig::default()
        }
    }

    /// The config and the epoch's first frame race; the mode moves when the second of them lands.
    #[test]
    fn the_mode_moves_when_config_and_frame_have_both_arrived() {
        let mut a = EpochAnchor::default();
        assert_eq!(a.frame(0), None);
        assert_eq!(a.config(cfg(1, 1280)), None, "no frame of epoch 1 yet");
        assert_eq!(a.frame(1).map(|(m, _)| m.width), Some(1280));
        assert_eq!(a.frame(1), None, "applied once");

        assert_eq!(a.frame(2), None, "a frame ahead of its config");
        assert_eq!(a.config(cfg(2, 1920)).map(|(m, _)| m.width), Some(1920));
    }

    /// A config for an epoch whose frames never came is replaced by the next one.
    #[test]
    fn a_skipped_epoch_never_moves_the_mode() {
        let mut a = EpochAnchor::default();
        assert_eq!(a.config(cfg(3, 800)), None);
        assert_eq!(a.config(cfg(4, 1024)), None);
        assert_eq!(a.frame(4).map(|(m, _)| m.width), Some(1024));
    }

    /// Under format epochs a frame waits until its epoch's config is in; without them it never
    /// waits.
    #[test]
    fn a_frame_waits_for_its_epochs_format_only_under_format_epochs() {
        let mut a = EpochAnchor::default();
        assert!(a.ready(5), "nothing moves the format");
        a.formats = true;
        assert!(!a.ready(5));
        let mut c = cfg(5, 1280);
        c.codec = CODEC_H264;
        a.config(c);
        assert!(a.ready(5));
        assert_eq!(a.format(5).map(|f| f.codec), Some(CODEC_H264));
        assert!(!a.ready(6), "the next epoch waits for its own");
    }

    /// Old epochs fall out; a re-sent config replaces its epoch's booking.
    #[test]
    fn bookings_keep_the_newest_epochs() {
        let mut a = EpochAnchor::default();
        let f = |codec| VideoFormat {
            codec,
            ..VideoFormat::default()
        };
        for e in 0..=BOOKED as u8 {
            a.book(e, f(CODEC_HEVC));
        }
        assert_eq!(a.format(0), None, "evicted");
        a.book(3, f(CODEC_H264));
        assert_eq!(a.format(3).map(|f| f.codec), Some(CODEC_H264));
        assert_eq!(a.booked.len(), BOOKED);
    }

    /// The first frame handed out in a new format is marked, once, and the format goes live;
    /// without format epochs nothing is marked.
    #[test]
    fn the_first_frame_in_a_new_format_is_marked_once() {
        let shared = super::super::super::ClientShared::new(Mode::default());
        let hevc = VideoFormat {
            codec: CODEC_HEVC,
            ..VideoFormat::default()
        };
        *shared.video_format.lock().unwrap() = hevc;
        {
            let mut a = shared.anchor.lock().unwrap();
            a.book(0, hevc);
            let mut c = cfg(1, 1280);
            c.codec = CODEC_H264;
            a.config(c);
        }
        let marked = |epoch| {
            let mut f = crate::session::Frame {
                data: Vec::new().into(),
                frame_index: 0,
                pts_ns: 0,
                flags: 0,
                epoch,
                stream: 0,
                complete: true,
                part: None,
                received_ns: 0,
                new_format: None,
            };
            mark(&shared, &mut f);
            f.new_format.map(|f| f.codec)
        };
        assert_eq!(marked(1), None, "no format epochs");
        shared.anchor.lock().unwrap().formats = true;
        assert_eq!(marked(0), None);
        assert_eq!(marked(1), Some(CODEC_H264));
        assert_eq!(marked(1), None, "marked once");
        assert_eq!(shared.video_format.lock().unwrap().codec, CODEC_H264);
    }

    /// An audio config moves the format at its first sequence; a packet of another codec before
    /// it is dropped, never decoded as the old one.
    #[test]
    fn an_audio_config_moves_the_format_at_its_first_seq() {
        use crate::quic::{AUDIO_CODEC_AC3, AUDIO_CODEC_EAC3, AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM};
        let pcm =
            crate::audio::plane::PlaneFormat::from_welcome(AUDIO_CODEC_PCM, 96_000, 24, 2, 2000, 0);
        let mut a = AudioAnchor {
            on: true,
            live: Some(pcm),
            ..AudioAnchor::default()
        };
        assert_eq!(a.admit(9, AUDIO_CODEC_PCM), AudioAdmit::Pass);
        assert_eq!(
            a.admit(10, AUDIO_CODEC_OPUS),
            AudioAdmit::Drop,
            "no config yet"
        );
        a.config(&crate::quic::v2::msg::AudioConfig {
            codec: AUDIO_CODEC_OPUS,
            rate_hz: 48_000,
            channels: 2,
            first_seq: 10,
            ..Default::default()
        });
        assert_eq!(
            a.admit(9, AUDIO_CODEC_PCM),
            AudioAdmit::Pass,
            "before first_seq"
        );
        let AudioAdmit::New(f) = a.admit(11, AUDIO_CODEC_OPUS) else {
            panic!("the first packet at or after first_seq opens the format")
        };
        assert_eq!((f.codec, f.rate_hz), (AUDIO_CODEC_OPUS, 48_000));
        assert_eq!(a.admit(12, AUDIO_CODEC_OPUS), AudioAdmit::Pass);
        assert_eq!(
            a.admit(13, AUDIO_CODEC_PCM),
            AudioAdmit::Drop,
            "the old codec is gone"
        );
        // `0xD4` admits as AC-3 into an E-AC-3 format; Opus does not.
        a.config(&crate::quic::v2::msg::AudioConfig {
            codec: AUDIO_CODEC_EAC3,
            channels: 6,
            first_seq: 20,
            ..Default::default()
        });
        assert_eq!(a.admit(19, AUDIO_CODEC_OPUS), AudioAdmit::Pass);
        let AudioAdmit::New(f) = a.admit(20, AUDIO_CODEC_AC3) else {
            panic!("the bitstream tag opens the E-AC-3 format")
        };
        assert_eq!(f.codec, AUDIO_CODEC_EAC3);
        assert_eq!(a.admit(21, AUDIO_CODEC_AC3), AudioAdmit::Pass);
        assert_eq!(a.admit(22, AUDIO_CODEC_OPUS), AudioAdmit::Drop);
        assert_eq!(a.admit(23, AUDIO_CODEC_PCM), AudioAdmit::Drop);
        a.on = false;
        assert_eq!(
            a.admit(14, AUDIO_CODEC_PCM),
            AudioAdmit::Pass,
            "off: nothing moves"
        );
    }

    /// The pump hears an epoch whose encoded picture moved, at its mode's refresh, and nothing
    /// for one that encodes what the last one did, whatever its mode.
    #[test]
    fn a_moved_picture_tells_the_pump() {
        let mut a = EpochAnchor {
            priced: cfg(0, 1280).mode,
            ..EpochAnchor::default()
        };
        let slot = Mutex::new(cfg(0, 1280).mode);
        let (tx, rx) = std::sync::mpsc::channel();
        let scaled = |epoch, width| StreamConfig {
            picture: Some((640, 360)),
            ..cfg(epoch, width)
        };
        a.frame(0);
        apply(&slot, &tx, a.config(cfg(0, 1280)).unwrap());
        assert_eq!(rx.try_iter().count(), 0, "the handshake's own picture");
        apply(&slot, &tx, a.config(scaled(0, 1280)).unwrap());
        let priced = Mode {
            width: 640,
            height: 360,
            refresh_hz: 60,
        };
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [PumpEvent::Mode(priced)]);
        a.frame(1);
        apply(&slot, &tx, a.config(scaled(1, 1920)).unwrap());
        assert_eq!(rx.try_iter().count(), 0, "a new mode at the same picture");
        assert_eq!(slot.lock().unwrap().width, 1920);
    }
}
