//! The Android side of the stats overlay: a handle on the connector's shared window
//! ([`punktfunk_core::hud::Stats`]) plus what only this client knows, the decoder it resolved and
//! the presenter's cadence readout. The window does the windowing and the formatting; this
//! forwards stamps in the shapes the decode paths hold them. Pure `std` plus the core, so it
//! builds on the host workspace too.

use punktfunk_core::hud::Stats;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct VideoStats {
    hud: Arc<Stats>,
    /// The codec's resolved `AMediaCodec` name, and whether it advertised `FEATURE_LowLatency`.
    decoder: Mutex<Option<(String, bool)>>,
    /// The presenter's last 1 s cadence window: off-mode present intervals (‰) and frames the
    /// compositor coalesced onto one vsync. Gauges; the presenter owns the window.
    judder_permille: AtomicU32,
    coalesced: AtomicU64,
    /// The last window as numbers (`nativeVideoStatsSample`), kept beside the lines each
    /// `nativeVideoStatsLines` call formats, so the two views read one window. Empty until then.
    sample: Mutex<Vec<f32>>,
}

/// A realtime stamp as the core takes it. A stamp that never happened reads 0.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn ns(v: i128) -> u64 {
    v.clamp(0, i128::from(u64::MAX)) as u64
}

impl VideoStats {
    pub fn new(hud: Arc<Stats>) -> VideoStats {
        VideoStats {
            hud,
            decoder: Mutex::new(None),
            judder_permille: AtomicU32::new(0),
            coalesced: AtomicU64::new(0),
            sample: Mutex::new(Vec::new()),
        }
    }

    pub fn keep_sample(&self, sample: Vec<f32>) {
        *crate::session::lock_recover(&self.sample) = sample;
    }

    /// The last window's numbers; empty before the first formatted window.
    pub fn sample(&self) -> Vec<f32> {
        crate::session::lock_recover(&self.sample).clone()
    }

    /// Whether the overlay wants samples. The decode paths skip their clock reads while not.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn enabled(&self) -> bool {
        self.hud.enabled()
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_cadence(&self, judder_permille: u32, coalesced: u64) {
        self.judder_permille
            .store(judder_permille, Ordering::Relaxed);
        self.coalesced.store(coalesced, Ordering::Relaxed);
    }

    pub fn judder_permille(&self) -> u32 {
        self.judder_permille.load(Ordering::Relaxed)
    }

    pub fn coalesced(&self) -> u64 {
        self.coalesced.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn set_decoder(&self, name: &str, low_latency: bool) {
        *self
            .decoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((name.to_owned(), low_latency));
    }

    /// `c2.qti.avc.decoder · low-latency`, or `""` before the decode thread resolved one.
    pub fn decoder_label(&self) -> String {
        match &*self
            .decoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some((name, true)) => format!("{name} · low-latency"),
            Some((name, false)) => name.clone(),
            None => String::new(),
        }
    }

    /// One decoded frame: counted, stamped capture → decoded, plus its received → decoded span.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_decoded(&self, pts_ns: u64, decoded_ns: i128, decode_us: Option<u64>) {
        self.hud.note_decoded(pts_ns, ns(decoded_ns));
        if let Some(us) = decode_us {
            self.hud.note_decode_us(us, false);
        }
    }

    /// One frame on glass. `released_ns` is the presenter's submit (`0` = none paired): the core
    /// splits `display` into pace and latch from it, and latch is this platform's OS floor.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_displayed(
        &self,
        pts_ns: u64,
        decoded_ns: i128,
        released_ns: i128,
        displayed_ns: i128,
    ) {
        self.hud
            .note_displayed(pts_ns, ns(decoded_ns), ns(released_ns), ns(displayed_ns));
        let latch = displayed_ns - released_ns;
        if released_ns > 0 && latch > 0 && latch < 10_000_000_000 {
            self.hud.note_os_floor_us((latch / 1000) as u64);
        }
    }

    /// Frames released without rendering: newest-wins pacing and held-off drops.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_skipped(&self, n: u64) {
        self.hud.note_skipped(n.min(u64::from(u32::MAX)) as u32, 0);
    }

    /// Parked AUs dropped before feeding: the decoder fell behind. Counts into `skipped` too.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn note_skipped_overflow(&self, n: u64) {
        self.hud.note_skipped(0, n.min(u64::from(u32::MAX)) as u32);
    }
}
