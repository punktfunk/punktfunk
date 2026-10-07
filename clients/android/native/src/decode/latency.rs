//! Decode-latency bookkeeping: realtime clock + decoded-pts / user-flags stat recording.

use punktfunk_core::client::NativeClient;
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::time::Duration;

pub(crate) use punktfunk_core::client::now_realtime_ns;

/// HUD `decoded` point for one dequeued output frame, keyed by the echoed `presentationTimeUs`:
/// hand the frame and its `decode` span (received→decoded, single-clock local, ≥ 0) to
/// [`crate::stats::VideoStats::note_decoded`]. The pts keys the receipt stamp in `in_flight`;
/// entries older than it are evicted (decode order == input order here — low-latency, no
/// B-frames — so anything before it was dropped inside the codec or stamped before a flush).
/// `decoded_ns` is the availability instant: the dequeue (sync loop) or the output callback's
/// stamp (async loop). Returns the receipt stamp it paired (if any) so the caller can split the
/// `decode` stage further (feed wait vs codec-pure) without re-walking the map.
pub(super) fn note_decoded_pts(
    client: &NativeClient,
    measure_decode: bool,
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    pts_us: u64,
    decoded_ns: i128,
) -> Option<i128> {
    let received_ns = take_by_pts(in_flight, pts_us);
    let decode_us = received_ns.map(|r| ((decoded_ns - r).max(0) / 1000) as u64);
    // Adaptive bitrate: the `decode` stage (received→decoded, single-clock local) IS the decoder-
    // backlog signal — the only bottleneck the host-side network signals can't see (a fast LAN
    // feeding a slower mobile decoder). Report it whenever the controller is armed, regardless of
    // the HUD; `report_decode_us` is a cheap accumulate the pump windows.
    if measure_decode {
        if let Some(us) = decode_us {
            client.report_decode_us(us.min(u32::MAX as u64) as u32);
        }
    }
    // Overlay only while it is visible (a measure-only caller enters here for the ABR report
    // alone). `pts_us` is the truncated capture pts we queued: ×1000 is within 1 µs of it.
    if stats.enabled() {
        stats.note_decoded(pts_us * 1000, decoded_ns, decode_us);
    }
    received_ns
}

/// The value parked for `pts_us` in a pts-ordered queue, popping it and every older entry.
/// Decode order == input order (low-latency, no B-frames), so an older entry was dropped inside
/// the codec or parked before a flush; a newer one stays for its own output. `None` on a miss.
pub(super) fn take_by_pts<V: Copy>(q: &mut VecDeque<(u64, V)>, pts_us: u64) -> Option<V> {
    while let Some(&(p, v)) = q.front() {
        if p > pts_us {
            return None;
        }
        q.pop_front();
        if p == pts_us {
            return Some(v);
        }
    }
    None
}

/// The AU `user_flags` for a decoded output, keyed by the echoed `presentationTimeUs`. Recovery
/// signalling (FLAG_SOF IDR marker / RECOVERY_ANCHOR / RECOVERY_POINT) rides the AU's flags, which are
/// only in scope at feed time — so the feed side parks `(pts_us, flags)` here and the present side
/// looks them up to fold [`ReanchorGate::on_decoded`]. A miss (probe filler, or an entry aged past
/// the cap) reads `0` — no recovery flags, decoded normally.
pub(super) fn take_flags(map: &mut VecDeque<(u64, u32)>, pts_us: u64) -> u32 {
    take_by_pts(map, pts_us).unwrap_or(0)
}

/// p50/max of an unsorted µs sample vec, in ms — the HUD's per-stage summary, shared by both
/// presenters. `(0, 0)` when empty.
pub(super) fn p50_max_ms(mut v: Vec<u64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    (
        v[v.len() / 2] as f64 / 1000.0,
        v[v.len() - 1] as f64 / 1000.0,
    )
}

/// The `received` point for one arriving AU: the core's reassembly stamp, which keys the
/// in-flight map the decode stage pairs against. The connector already noted receipt for the
/// overlay; draining the timings here matches each 0xCF to its frame.
pub(super) fn note_received_frame(client: &NativeClient, frame: &Frame) -> i128 {
    // Reassembly completion, NOT the pull instant: stamping at the pull would fold the hand-off
    // queue wait into the network figure. 0 = older core.
    let received_ns = if frame.received_ns > 0 {
        frame.received_ns as i128
    } else {
        now_realtime_ns()
    };
    while client.next_host_timing(Duration::ZERO).is_ok() {}
    received_ns
}

#[cfg(test)]
mod tests {
    use super::take_by_pts;
    use std::collections::VecDeque;

    #[test]
    fn take_by_pts_evicts_older_pops_the_match_and_holds_newer() {
        let mut q: VecDeque<(u64, i128)> = [(10, 1), (20, 2), (30, 3), (40, 4)].into();
        assert_eq!(take_by_pts(&mut q, 30), Some(3));
        assert_eq!(q, VecDeque::from([(40, 4)]));
        assert_eq!(
            take_by_pts(&mut q, 35),
            None,
            "a miss keeps the newer entry"
        );
        assert_eq!(q, VecDeque::from([(40, 4)]));
        assert_eq!(take_by_pts(&mut q, 50), None);
        assert!(q.is_empty());
    }
}
