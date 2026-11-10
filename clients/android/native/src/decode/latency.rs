//! Decode-latency bookkeeping: realtime clock + decoded-pts stat recording.

use crate::codec_format::take_by_pts;
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

/// The `received` point for one arriving AU: the core's reassembly stamp, which keys the
/// in-flight map the decode stage pairs against. The connector already noted receipt for the
/// overlay; draining the timings here matches each 0xCF to its frame.
pub(crate) fn note_received_frame(client: &NativeClient, frame: &Frame) -> i128 {
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
