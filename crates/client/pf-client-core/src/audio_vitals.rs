//! Playback gauges the device callback publishes and the decode thread logs.
//!
//! PipeWire's callback runs on the graph data loop (`RT_PROCESS`): a `String`, a subscriber
//! mutex, or a write to the log ring is a priority inversion or a missed cycle. The callback
//! stores atomics here; the decode thread, already waking every frame, prints them.
//! Log field names stay `buffer_ms`, `target_ms`, `underruns`, `drift_sheds`,
//! `drift_inserts`, `plc_ms`. WASAPI's render loop is a plain thread and could log in place,
//! but publishes here too so both platforms share one line.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Callback-published counters and gauges. All `Relaxed`: each field is a self-contained reading.
#[derive(Debug, Default)]
pub struct PlaybackVitals {
    /// Callbacks entered, primed or not.
    pub callbacks: AtomicU64,
    /// Ring could not fill the callback. See `JitterPolicy::note_read`.
    pub underruns: AtomicU64,
    /// Policy drops: drift sheds and hard trims together.
    pub sheds: AtomicU64,
    /// Policy inserts (`JitterStep::insert_front`): one duplicated, crossfaded frame each.
    /// Deepening without this counter hides audio drifting off the picture.
    pub inserts: AtomicU64,
    /// Smoothed ring depth, ms. Drift correction reacts to this.
    pub buffer_ms: AtomicU32,
    /// Live target depth, ms: grows under underrun pressure, follows A/V sync.
    pub target_ms: AtomicU32,
    /// First-seen quantum: graph request, mapped capacity, frames written.
    /// `write_frames == 0` means not yet observed.
    pub requested_frames: AtomicU32,
    pub capacity_frames: AtomicU32,
    pub write_frames: AtomicU32,
}

impl PlaybackVitals {
    /// From the callback: one cycle. `ran_short` is a ring miss, not a shed.
    pub fn note_callback(
        &self,
        ran_short: bool,
        shed: bool,
        insert: bool,
        buffer_ms: u32,
        target_ms: u32,
    ) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        if ran_short {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if shed {
            self.sheds.fetch_add(1, Ordering::Relaxed);
        }
        if insert {
            self.inserts.fetch_add(1, Ordering::Relaxed);
        }
        self.buffer_ms.store(buffer_ms, Ordering::Relaxed);
        self.target_ms.store(target_ms, Ordering::Relaxed);
    }

    /// From the callback: store the quantum. Caller publishes on the first callback that has one.
    pub fn note_quantum(&self, requested: u32, capacity: u32, write: u32) {
        self.requested_frames.store(requested, Ordering::Relaxed);
        self.capacity_frames.store(capacity, Ordering::Relaxed);
        self.write_frames.store(write, Ordering::Relaxed);
    }

    pub fn quantum_known(&self) -> bool {
        self.write_frames.load(Ordering::Relaxed) > 0
    }

    /// Log snapshot. Each field is one Relaxed load; a callback between loads can skew a
    /// counter by one, which a ~10 s line does not care about.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            sheds: self.sheds.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            buffer_ms: self.buffer_ms.load(Ordering::Relaxed),
            target_ms: self.target_ms.load(Ordering::Relaxed),
            requested_frames: self.requested_frames.load(Ordering::Relaxed),
            capacity_frames: self.capacity_frames.load(Ordering::Relaxed),
            write_frames: self.write_frames.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub callbacks: u64,
    pub underruns: u64,
    pub sheds: u64,
    pub inserts: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub requested_frames: u32,
    pub capacity_frames: u32,
    pub write_frames: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_gauges_overwrite() {
        let v = PlaybackVitals::default();
        assert!(!v.quantum_known());
        v.note_callback(false, false, false, 15, 15);
        v.note_callback(true, true, false, 9, 25);
        v.note_callback(true, false, false, 12, 25);
        v.note_callback(false, false, true, 12, 25);
        v.note_quantum(240, 8192, 240);
        let s = v.snapshot();
        assert_eq!(s.callbacks, 4);
        assert_eq!(s.underruns, 2);
        assert_eq!(s.sheds, 1);
        assert_eq!(s.inserts, 1);
        assert_eq!(
            (s.buffer_ms, s.target_ms),
            (12, 25),
            "gauges hold the latest reading"
        );
        assert_eq!(
            (s.requested_frames, s.capacity_frames, s.write_frames),
            (240, 8192, 240)
        );
        assert!(v.quantum_known());
    }
}
