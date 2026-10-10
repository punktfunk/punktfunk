//! The `punktfunk/2` media clock: time since the session began, on the host's monotonic clock.
//!
//! The host stamps media in Unix nanoseconds, as every capture backend does. Each stamp leaving
//! on the wire is mapped onto this clock at the edge, from a fresh paired read of both clocks:
//! a stamp `age` old becomes `elapsed − age`. A wall-clock step therefore moves no stamp on the
//! wire. On the wire a stamp is `origin + elapsed` at whole microseconds, so a client reads it as
//! Unix-scaled time, the same as before, and the media header's `pts_us` is `elapsed` itself.

use crate::quic::{
    wall_clock_ns, AUDIO_MAGIC, AUDIO_PCM_MAGIC, AUDIO_RED_MAGIC, HOST_TIMING_MAGIC,
    PAD_AUDIO_MAGIC,
};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// The oldest a stamp can be when it leaves. Past this it straddled a wall-clock step.
const MAX_AGE_NS: i128 = 1_000_000_000;
/// Video frames whose wire pts are remembered for their `HostTiming`: ~0.5 s at 120 Hz.
const RECENT: usize = 64;

/// One session's media clock. The host holds it; a client only ever sees its stamps.
#[derive(Debug)]
pub struct SessionClock {
    origin_ns: u64,
    start: Instant,
    video: Mutex<Video>,
}

#[derive(Debug, Default)]
struct Video {
    last_us: u64,
    /// `(host ns, wire ns)`, newest last.
    recent: VecDeque<(u64, u64)>,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionClock {
    /// Starts now. [`SessionClock::origin_ns`] goes out in the `ServerHello`.
    pub fn new() -> SessionClock {
        SessionClock {
            origin_ns: wall_clock_ns(),
            start: Instant::now(),
            video: Mutex::default(),
        }
    }

    /// The Unix instant, ns, that wire time `origin + 0` stands for.
    pub fn origin_ns(&self) -> u64 {
        self.origin_ns
    }

    fn elapsed_us(&self, host_ns: u64) -> u64 {
        let age = (wall_clock_ns() as i128 - host_ns as i128).clamp(0, MAX_AGE_NS);
        ((self.start.elapsed().as_nanos() as i128 - age).max(0) / 1000) as u64
    }

    /// A host stamp, Unix ns, in wire form.
    pub fn to_wire(&self, host_ns: u64) -> u64 {
        self.origin_ns + self.elapsed_us(host_ns) * 1000
    }

    /// A video frame's wire pts. Every packet of a frame, and its `HostTiming`, get the same
    /// value; a frame never goes out stamped before the one ahead of it.
    pub fn video_to_wire(&self, host_ns: u64) -> u64 {
        let mut v = self.video.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&(_, wire)) = v.recent.iter().rev().find(|(h, _)| *h == host_ns) {
            return wire;
        }
        let us = self.elapsed_us(host_ns).max(v.last_us);
        v.last_us = us;
        let wire = self.origin_ns + us * 1000;
        if v.recent.len() == RECENT {
            v.recent.pop_front();
        }
        v.recent.push_back((host_ns, wire));
        wire
    }

    /// Rewrites the host stamp in an outgoing `punktfunk/1`-encoded datagram to wire form:
    /// desktop and pad audio, and `HostTiming`, which names its frame by the video pts.
    pub fn retime_datagram(&self, d: &mut [u8]) {
        let (at, video) = match d.first() {
            Some(&(AUDIO_MAGIC | AUDIO_RED_MAGIC | AUDIO_PCM_MAGIC)) => (5, false),
            Some(&PAD_AUDIO_MAGIC) => (7, false),
            Some(&HOST_TIMING_MAGIC) => (1, true),
            _ => return,
        };
        let Some(field) = d.get_mut(at..at + 8) else {
            return;
        };
        let host_ns = u64::from_le_bytes(field.try_into().expect("8 bytes"));
        let wire = if video {
            self.video_to_wire(host_ns)
        } else {
            self.to_wire(host_ns)
        };
        field.copy_from_slice(&wire.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{
        decode_audio_datagram, decode_host_timing_datagram, decode_pad_audio_datagram,
        encode_audio_datagram, encode_host_timing_datagram, encode_pad_audio_datagram, HostTiming,
    };

    #[test]
    fn a_stamp_keeps_its_age_on_the_wire() {
        let c = SessionClock::new();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let now = wall_clock_ns();
        let wire = c.to_wire(now - 5_000_000);
        let wire_now = c.origin_ns() + c.start.elapsed().as_nanos() as u64;
        let age = wire_now as i64 - wire as i64;
        assert!((4_000_000..7_000_000).contains(&age), "age {age}");
        assert_eq!((wire - c.origin_ns()) % 1000, 0, "whole microseconds");
    }

    #[test]
    fn a_wall_clock_step_does_not_reach_the_wire() {
        let c = SessionClock::new();
        // A stamp an hour away is a step, not an age: it leaves as now, never an hour off.
        let ahead = c.to_wire(wall_clock_ns() + 3_600_000_000_000);
        let behind = c.to_wire(wall_clock_ns() - 3_600_000_000_000);
        let now = c.origin_ns() + c.start.elapsed().as_nanos() as u64;
        assert!(now.abs_diff(ahead) < 5_000_000);
        assert!(now.abs_diff(behind) <= MAX_AGE_NS as u64 + 5_000_000);
    }

    #[test]
    fn video_never_runs_backwards_and_host_timing_matches_it() {
        let c = SessionClock::new();
        let now = wall_clock_ns();
        let a = c.video_to_wire(now);
        // Stamped before the previous frame: it leaves level with it, not behind.
        let b = c.video_to_wire(now - 500_000_000);
        assert!(b >= a);
        assert_eq!(
            c.video_to_wire(now),
            a,
            "every packet of a frame gets one value"
        );
        let mut d = encode_host_timing_datagram(&HostTiming {
            pts_ns: now,
            host_us: 3,
            stages: None,
        });
        c.retime_datagram(&mut d);
        assert_eq!(decode_host_timing_datagram(&d).unwrap().pts_ns, a);
    }

    #[test]
    fn audio_stamps_are_retimed_in_place() {
        let c = SessionClock::new();
        let now = wall_clock_ns();
        let mut d = encode_audio_datagram(7, now, b"opus");
        c.retime_datagram(&mut d);
        let (seq, pts, opus) = decode_audio_datagram(&d).unwrap();
        assert_eq!((seq, opus), (7, b"opus".as_slice()));
        assert!(pts.abs_diff(c.to_wire(now)) < 1_000_000);
        let mut p = encode_pad_audio_datagram(1, 2, 9, now, b"x");
        c.retime_datagram(&mut p);
        let f = decode_pad_audio_datagram(&p).unwrap();
        assert_eq!((f.pad, f.kind, f.seq), (1, 2, 9));
        assert_eq!((f.pts_ns - c.origin_ns()) % 1000, 0);
    }
}
