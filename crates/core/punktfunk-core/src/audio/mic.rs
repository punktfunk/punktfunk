//! Microphone uplink encode for `0xCB`: 10 ms mono Opus frames at 48 kHz. Each client owns its
//! capture API; this is the frame loop they share.

use std::collections::VecDeque;

/// One 10 ms mono frame at 48 kHz. The host decodes any Opus frame up to 120 ms.
pub const FRAME_SAMPLES: usize = 480;

/// Self-heal threshold in queued frames (~60 ms): a backlog past this is standing mic delay.
const BACKLOG_MAX_FRAMES: usize = 6;
/// What a self-heal keeps: the newest ~20 ms, one audible blip.
const BACKLOG_KEEP_FRAMES: usize = 2;

/// Encoder, sample ring and `seq`, kept across capture reopens so the host sees one stream.
/// The counters are for the caller's log line.
pub struct MicEncoder {
    enc: opus::Encoder,
    ring: VecDeque<f32>,
    pcm: Vec<f32>,
    out: Vec<u8>,
    seq: u32,
    self_heal: bool,
    /// Frames handed to `send`.
    pub sent: u64,
    /// Frames shed by the backlog self-heal.
    pub stale: u64,
    /// Frames dropped unencoded while muted.
    pub muted_frames: u64,
    /// Loudest |sample| encoded since the caller last zeroed it; a muted frame zeroes it.
    pub peak: f32,
}

impl MicEncoder {
    /// 48 kbps VOIP with in-band FEC at an assumed 10 % loss: the uplink is fire-and-forget, so
    /// FEC is the only redundancy the host decoder has. `self_heal` sheds a backlog past ~60 ms
    /// to the newest 20 ms; only a capture that queues across threads can build one.
    pub fn new(self_heal: bool) -> Result<MicEncoder, opus::Error> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)?;
        for (setting, r) in [
            ("bitrate", enc.set_bitrate(opus::Bitrate::Bits(48_000))),
            ("inband_fec", enc.set_inband_fec(true)),
            ("packet_loss_perc", enc.set_packet_loss_perc(10)),
        ] {
            if let Err(e) = r {
                tracing::warn!(setting, error = %e, "mic encoder setting not applied");
            }
        }
        Ok(MicEncoder {
            enc,
            ring: VecDeque::with_capacity(FRAME_SAMPLES * 4),
            pcm: vec![0f32; FRAME_SAMPLES],
            out: vec![0u8; 4000],
            seq: 0,
            self_heal,
            sent: 0,
            stale: 0,
            muted_frames: 0,
            peak: 0.0,
        })
    }

    /// The Opus encoder, for tuning past the shared settings.
    pub fn encoder_mut(&mut self) -> &mut opus::Encoder {
        &mut self.enc
    }

    /// Queue captured mono samples.
    pub fn push(&mut self, samples: impl IntoIterator<Item = f32>) {
        self.ring.extend(samples);
    }

    /// Encode every whole queued frame and hand each packet to `send(seq, pts_ns, packet)`.
    /// Muted frames are dropped unencoded and `seq` does not advance: the host reads a seq jump
    /// as loss, where a mute is a pause.
    pub fn drain(&mut self, muted: bool, mut send: impl FnMut(u32, u64, &[u8])) {
        if self.self_heal && self.ring.len() > BACKLOG_MAX_FRAMES * FRAME_SAMPLES {
            let excess = self.ring.len() - BACKLOG_KEEP_FRAMES * FRAME_SAMPLES;
            self.ring.drain(..excess);
            self.stale += (excess / FRAME_SAMPLES) as u64;
        }
        while self.ring.len() >= FRAME_SAMPLES {
            if muted {
                self.ring.drain(..FRAME_SAMPLES);
                self.muted_frames += 1;
                self.peak = 0.0;
                continue;
            }
            for (dst, src) in self.pcm.iter_mut().zip(self.ring.drain(..FRAME_SAMPLES)) {
                *dst = src;
                self.peak = self.peak.max(src.abs());
            }
            match self.enc.encode_float(&self.pcm, &mut self.out) {
                Ok(len) => {
                    let pts = crate::client::now_realtime_ns().max(0) as u64;
                    send(self.seq, pts, &self.out[..len]);
                    self.seq = self.seq.wrapping_add(1);
                    self.sent += 1;
                }
                Err(e) => tracing::debug!(error = %e, "mic encode"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seqs(mic: &mut MicEncoder, muted: bool) -> Vec<u32> {
        let mut out = Vec::new();
        mic.drain(muted, |seq, _, pkt| {
            assert!(!pkt.is_empty());
            out.push(seq);
        });
        out
    }

    /// Whole frames only; a mute drops frames without spending a seq.
    #[test]
    fn a_mute_is_a_pause_not_a_loss() {
        let mut mic = MicEncoder::new(false).unwrap();
        mic.push(vec![0.1; FRAME_SAMPLES * 2 + 100]);
        assert_eq!(seqs(&mut mic, false), [0, 1]);
        mic.push(vec![0.1; FRAME_SAMPLES * 3]);
        assert!(seqs(&mut mic, true).is_empty());
        assert_eq!(mic.muted_frames, 3);
        mic.push(vec![0.1; FRAME_SAMPLES - 100]);
        assert_eq!(seqs(&mut mic, false), [2]);
        assert_eq!(mic.sent, 3);
    }

    /// Past ~60 ms queued, the self-heal jumps to the newest 20 ms; without it every frame goes.
    #[test]
    fn the_self_heal_sheds_a_standing_backlog() {
        let mut healing = MicEncoder::new(true).unwrap();
        healing.push(vec![0.1; FRAME_SAMPLES * 10]);
        assert_eq!(seqs(&mut healing, false).len(), BACKLOG_KEEP_FRAMES);
        assert_eq!(healing.stale, 8);
        healing.push(vec![0.1; FRAME_SAMPLES * BACKLOG_MAX_FRAMES]);
        assert_eq!(seqs(&mut healing, false).len(), BACKLOG_MAX_FRAMES);

        let mut plain = MicEncoder::new(false).unwrap();
        plain.push(vec![0.1; FRAME_SAMPLES * 10]);
        assert_eq!(seqs(&mut plain, false).len(), 10);
        assert_eq!(plain.stale, 0);
    }
}
