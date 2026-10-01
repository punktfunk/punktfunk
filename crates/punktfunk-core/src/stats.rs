//! Live counters for the frame-pacing / quality logic and the web UI.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Monotonic ns since an arbitrary process-wide epoch. Probe arrival stamps are
/// only ever differenced on this machine; a wall-clock step mid-burst would
/// corrupt the interval, so `pts_ns`'s CLOCK_REALTIME basis is wrong here.
/// Floored at 1 so a stamp never collides with the 0 = "unset" sentinel.
pub(crate) fn now_monotonic_ns() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64).max(1)
}

/// Immutable snapshot, copied across the C ABI as `PunktfunkStats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    /// WouldBlock on send: the kernel send buffer was full. Distinct from
    /// `packets_dropped` (recv-side reassembler rejects).
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    /// Recovered shards that later arrived: reordering reconstructed the block
    /// from parity while the originals were still in flight. Loss estimators
    /// must net this out (`recovered - late`, see [`window_loss_ppm`](crate::quic::window_loss_ppm))
    /// or reordering reads as loss. Not mirrored into the C-ABI `PunktfunkStats`.
    pub fec_late_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// DATA-shard payload delivered to the video reassembler — no headers, FEC
    /// parity, probe filler, or audio. `bytes_received` includes FEC redundancy,
    /// so a utilization gate built from it inflates on lossy links. Not mirrored
    /// into the C-ABI `PunktfunkStats`.
    pub media_bytes_received: u64,
    /// Wire packets / plaintext bytes carrying [`FLAG_PROBE`](crate::packet::FLAG_PROBE).
    /// `bytes_received` counts every accepted datagram, so a speed-test numerator
    /// built from it inherits in-flight video. Not mirrored into the C-ABI
    /// `PunktfunkStats` (probe measurements surface via `ProbeOutcome`).
    pub probe_packets_received: u64,
    pub probe_bytes_received: u64,
    /// First / last probe-packet arrival (monotonic ns, see [`now_monotonic_ns`];
    /// 0 = none since the last arm). Difference is the client receive interval:
    /// the host send window closes while the path is still draining, so host
    /// duration overstates the link. Zeroed on arm (`Session::reset_probe_arrivals`).
    pub probe_first_arrival_ns: u64,
    pub probe_last_arrival_ns: u64,
    /// Inter-arrival gaps between probe packets, bucketed at [`PROBE_GAP_BUCKET_US`]; the
    /// last bucket holds every longer gap. Zeroed with the arrival stamps.
    pub probe_gap_buckets: [u32; PROBE_GAP_BUCKETS],
    /// Probe packets that arrived behind a later one.
    pub probe_reorders: u32,
}

/// Thirty-two buckets of 100 µs: a percentile to a tenth of a millisecond, and an array
/// `Default` still derives.
pub const PROBE_GAP_BUCKETS: usize = 32;
pub const PROBE_GAP_BUCKET_US: u32 = 100;

pub fn probe_gap_bucket(gap_us: u64) -> usize {
    ((gap_us / u64::from(PROBE_GAP_BUCKET_US)) as usize).min(PROBE_GAP_BUCKETS - 1)
}

/// The `q`-quantile of bucketed gaps as its bucket's upper edge, µs; `0` with no samples.
pub fn probe_gap_percentile(buckets: &[u32; PROBE_GAP_BUCKETS], q: f64) -> u32 {
    let total: u64 = buckets.iter().map(|&b| u64::from(b)).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total as f64 * q) as u64).min(total - 1);
    let mut seen = 0u64;
    for (i, &b) in buckets.iter().enumerate() {
        seen += u64::from(b);
        if seen > rank {
            return (i as u32 + 1) * PROBE_GAP_BUCKET_US;
        }
    }
    PROBE_GAP_BUCKETS as u32 * PROBE_GAP_BUCKET_US
}

/// Atomic accumulators owned by a [`Session`](crate::session::Session). Snapshot
/// to [`Stats`] for readers. `Relaxed` is enough: these never synchronize other
/// memory. Probe arrival stamps are slots, not counters, and are read well after
/// the last write.
#[derive(Default)]
pub struct StatsCounters {
    pub frames_submitted: AtomicU64,
    pub frames_completed: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub packets_sent: AtomicU64,
    pub packets_received: AtomicU64,
    pub packets_dropped: AtomicU64,
    pub packets_send_dropped: AtomicU64,
    pub fec_recovered_shards: AtomicU64,
    pub fec_late_shards: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub media_bytes_received: AtomicU64,
    pub probe_packets_received: AtomicU64,
    pub probe_bytes_received: AtomicU64,
    pub probe_first_arrival_ns: AtomicU64,
    pub probe_last_arrival_ns: AtomicU64,
    pub probe_gap_buckets: [std::sync::atomic::AtomicU32; PROBE_GAP_BUCKETS],
    /// The previous probe packet's arrival, for the gap; `0` = none since the last arm.
    pub probe_prev_arrival_ns: AtomicU64,
    /// `(frame_index << 16 | shard_index) + 1` of the last probe packet; `0` = none.
    pub probe_last_key: AtomicU64,
    pub probe_reorders: AtomicU64,
}

impl StatsCounters {
    #[inline]
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Stats {
        let l = Ordering::Relaxed;
        Stats {
            frames_submitted: self.frames_submitted.load(l),
            frames_completed: self.frames_completed.load(l),
            frames_dropped: self.frames_dropped.load(l),
            packets_sent: self.packets_sent.load(l),
            packets_received: self.packets_received.load(l),
            packets_dropped: self.packets_dropped.load(l),
            packets_send_dropped: self.packets_send_dropped.load(l),
            fec_recovered_shards: self.fec_recovered_shards.load(l),
            fec_late_shards: self.fec_late_shards.load(l),
            bytes_sent: self.bytes_sent.load(l),
            bytes_received: self.bytes_received.load(l),
            media_bytes_received: self.media_bytes_received.load(l),
            probe_packets_received: self.probe_packets_received.load(l),
            probe_bytes_received: self.probe_bytes_received.load(l),
            probe_first_arrival_ns: self.probe_first_arrival_ns.load(l),
            probe_last_arrival_ns: self.probe_last_arrival_ns.load(l),
            probe_gap_buckets: std::array::from_fn(|i| self.probe_gap_buckets[i].load(l)),
            probe_reorders: self.probe_reorders.load(l).min(u64::from(u32::MAX)) as u32,
        }
    }
}
