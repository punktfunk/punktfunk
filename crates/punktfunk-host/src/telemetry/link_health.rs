//! One `link health` line per session per minute, at INFO.
//!
//! Every signal that explains a freeze — client loss, RFI, keyframe asks, the recovery frames
//! the encoder answered with, the adaptive-FEC and ABR bands — is a DEBUG event scattered
//! across three threads. This is their per-minute total on one greppable line, so a report
//! that arrives an hour late still has the numbers.
//!
//! [`LinkCounters`] lives on [`crate::session_status::SessionCounters`]: relaxed atomics the
//! encode, send and control paths bump, never a lock on the frame path. The control task
//! ([`crate::native::control`]) owns the clock, drains them with [`LinkCounters::take`] and
//! emits. A silent minute still emits, all zero: a reader must be able to tell *nothing
//! happened* from *logging is off*.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Intra refresh waves in a minute that make the line a WARN. Each wave is a loss the encoder
/// could not anchor over; ten is a link shedding packets, not a blip.
pub const WARN_INTRA_REFRESH: u32 = 10;
/// Client keyframe asks in a minute that make the line a WARN. The IDR cooldown coalesces, so
/// three distinct asks is a client that is not being repaired.
pub const WARN_KEYFRAME_REQ: u32 = 3;

/// How the client asked for its frames: paced, or the wake shape (a small first group and a
/// gap on every frame).
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LinkShape {
    #[default]
    Auto,
    Wake,
}

/// One minute of link health for one session. Every field is a delta for the window except
/// the bands, which are min..max over it.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkMinute {
    /// The `/status` session id, so a line ties to a row.
    pub session_id: u64,
    /// Seconds this line covers. 60, except the last line of a session.
    pub secs: u32,
    /// Client loss reports that closed here. 0 = the client sent none, which is itself a fault.
    pub windows: u32,
    /// Of `windows`, those that reported any loss.
    pub loss_windows: u32,
    pub loss_max_ppm: u32,
    pub loss_mean_ppm: u32,
    /// Windows that closed with frames parity could not repair.
    pub unrecovered: u32,
    /// Client reference-frame invalidation asks, and those the encoder refused (each refusal
    /// costs an IDR).
    pub rfi: u32,
    pub rfi_declined: u32,
    /// What the encoder answered an RFI with: a clean P against a surviving reference, or a
    /// wave that recodes the picture over ~0.5 s.
    pub anchor_p: u32,
    pub intra_refresh: u32,
    /// Frames encoded against one the client confirmed, while the link showed loss.
    pub acked_p: u32,
    /// Client decode-recovery asks, and the IDRs actually forced (the cooldown coalesces).
    pub keyframe_req: u32,
    pub idr: u32,
    pub fec_min_pct: u8,
    pub fec_max_pct: u8,
    pub abr_min_kbps: u32,
    pub abr_max_kbps: u32,
    /// Client bitrate asks below the rate the encoder was running. The host cannot see which
    /// of loss, delay or decode drove them — only that the client's controller retreated.
    pub abr_backoffs: u32,
    /// Times the encoder's rate actually moved.
    pub retargets: u32,
    /// Pipeline gaps this host announced: its own stalls, not the network's.
    pub gaps: u32,
    /// Sealed wire throughput over the window.
    pub egress_kbps: u32,
    /// The link rate the client last reported, which every frame is paced at. `0` = none.
    pub link_kbps: u32,
    /// The shape the client asked for.
    pub shape: LinkShape,
    /// Packets resent on a client's NACK.
    pub resend_pkts: u32,
    /// Shards the client never got, by where in their frame they fell: the first twelve
    /// data shards, the middle, and the last twelve with all parity.
    pub loss_head: u32,
    pub loss_mid: u32,
    pub loss_tail: u32,
    /// Packets the client's own receive buffer dropped.
    pub sock_drops: u32,
}

impl LinkMinute {
    /// Whether this minute reads as a link in trouble rather than a link with a blip.
    pub fn alarming(&self) -> bool {
        self.intra_refresh >= WARN_INTRA_REFRESH || self.keyframe_req >= WARN_KEYFRAME_REQ
    }
}

/// ppm → percent, two decimals. Same scale as the adaptive-FEC band.
fn pct(ppm: u32) -> String {
    format!("{:.2}", f64::from(ppm) / 10_000.0)
}

/// Emit `m` for `peer`. WARN when [`LinkMinute::alarming`], else INFO.
///
/// The field list is the contract — a reader greps one name across a whole journal — so the
/// two levels share one expansion rather than drifting apart.
pub fn emit(m: &LinkMinute, peer: std::net::IpAddr) {
    macro_rules! line {
        ($lvl:ident) => {
            tracing::$lvl!(
                session = m.session_id,
                peer = %peer,
                secs = m.secs,
                windows = m.windows,
                loss_windows = m.loss_windows,
                loss_max_pct = %pct(m.loss_max_ppm),
                loss_mean_pct = %pct(m.loss_mean_ppm),
                unrecovered = m.unrecovered,
                rfi = m.rfi,
                rfi_declined = m.rfi_declined,
                anchor_p = m.anchor_p,
                intra_refresh = m.intra_refresh,
                acked_p = m.acked_p,
                keyframe_req = m.keyframe_req,
                idr = m.idr,
                fec_pct = %format!("{}..{}", m.fec_min_pct, m.fec_max_pct),
                abr_kbps = %format!("{}..{}", m.abr_min_kbps, m.abr_max_kbps),
                abr_backoffs = m.abr_backoffs,
                retargets = m.retargets,
                gaps = m.gaps,
                egress_mbps = %format!("{:.1}", f64::from(m.egress_kbps) / 1000.0),
                link_mbps = m.link_kbps / 1000,
                shape = ?m.shape,
                resend_pkts = m.resend_pkts,
                loss_head = m.loss_head,
                loss_mid = m.loss_mid,
                loss_tail = m.loss_tail,
                sock_drops = m.sock_drops,
                "link health"
            )
        };
    }
    if m.alarming() {
        line!(warn);
    } else {
        line!(info);
    }
}

/// The counters threads other than the control task bump, plus the last minute `/status` serves.
///
/// Relaxed throughout: a diagnostic, never synchronisation. What the control task sees itself
/// (loss, RFI, keyframe asks, the FEC and ABR bands) stays in its own locals — an atomic for a
/// value one thread reads and writes buys nothing.
#[derive(Default)]
pub struct LinkCounters {
    /// Latched by [`crate::session_status::register`]; 0 until the video loop registers.
    session_id: AtomicU64,
    anchor_p: AtomicU32,
    intra_refresh: AtomicU32,
    acked_p: AtomicU32,
    rfi_declined: AtomicU32,
    idr: AtomicU32,
    retargets: AtomicU32,
    resend_pkts: AtomicU32,
    /// Session-cumulative sealed wire bytes, republished by the send thread every ~2 s. The
    /// control task keeps the previous read and diffs; nothing here resets.
    egress_bytes: AtomicU64,
    /// The minute `GET /status` reports for this session. `None` until the first one closes.
    last: Mutex<Option<LinkMinute>>,
}

impl LinkCounters {
    /// The `/status` id for this session's lines, once the registry has minted one.
    pub fn set_session_id(&self, id: u64) {
        self.session_id.store(id, Ordering::Relaxed);
    }

    /// That id, or `0` while the video loop has yet to register this session.
    pub fn session_id(&self) -> u64 {
        self.session_id.load(Ordering::Relaxed)
    }

    /// One recovery AU the encoder produced: a clean anchor P, or the start of an intra
    /// refresh wave. An anchor while the encoder holds `acked` references counts there, not
    /// as an RFI's answer. Called from [`crate::native::stream::encode`] per AU.
    pub fn note_recovery_au(&self, anchor_p: bool, acked: bool, wave_start: bool) {
        match (anchor_p, acked) {
            (true, true) => self.acked_p.fetch_add(1, Ordering::Relaxed),
            (true, false) => self.anchor_p.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        if wave_start {
            self.intra_refresh.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// An RFI the encoder refused, so recovery costs an IDR.
    pub fn note_rfi_declined(&self) {
        self.rfi_declined.fetch_add(1, Ordering::Relaxed);
    }

    /// Packets the send thread resent on a NACK.
    pub fn note_resent(&self, n: u32) {
        self.resend_pkts.fetch_add(n, Ordering::Relaxed);
    }

    /// One IDR actually forced, past the coalescing cooldown.
    pub fn note_idr(&self) {
        self.idr.fetch_add(1, Ordering::Relaxed);
    }

    /// The encoder's rate moved. Bumped from
    /// [`SessionCounters::note_bitrate`](crate::session_status::SessionCounters::note_bitrate),
    /// which already knows a repeat from a move.
    pub fn note_retarget(&self) {
        self.retargets.fetch_add(1, Ordering::Relaxed);
    }

    /// Session-cumulative sealed bytes, from the send thread's aggregation tick.
    pub fn publish_egress_bytes(&self, total: u64) {
        self.egress_bytes.store(total, Ordering::Relaxed);
    }

    /// Cumulative sealed bytes as last published. The control task diffs it across the window.
    pub fn egress_bytes(&self) -> u64 {
        self.egress_bytes.load(Ordering::Relaxed)
    }

    /// Drain the cross-thread counters into `m` and publish it as this session's last minute.
    /// The control task fills the rest of `m` from its own locals first.
    pub fn take(&self, m: &mut LinkMinute) {
        m.session_id = self.session_id.load(Ordering::Relaxed);
        m.anchor_p = self.anchor_p.swap(0, Ordering::Relaxed);
        m.intra_refresh = self.intra_refresh.swap(0, Ordering::Relaxed);
        m.acked_p = self.acked_p.swap(0, Ordering::Relaxed);
        m.rfi_declined = self.rfi_declined.swap(0, Ordering::Relaxed);
        m.idr = self.idr.swap(0, Ordering::Relaxed);
        m.retargets = self.retargets.swap(0, Ordering::Relaxed);
        m.resend_pkts = self.resend_pkts.swap(0, Ordering::Relaxed);
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(m.clone());
    }

    /// The last closed minute, for `GET /status`. `None` before the first one.
    pub fn last(&self) -> Option<LinkMinute> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The control task's own half of a minute: the signals nothing else sees.
///
/// Plain fields, not atomics — one thread reads and writes them. [`Self::close`] folds in the
/// cross-thread half and hands back the line to emit.
pub struct LinkWindow {
    started: std::time::Instant,
    /// Cumulative sealed bytes at the window's start; egress is the diff against it.
    egress_base: u64,
    windows: u32,
    loss_windows: u32,
    loss_ppm_max: u32,
    loss_ppm_sum: u64,
    unrecovered: u32,
    rfi: u32,
    keyframe_req: u32,
    abr_backoffs: u32,
    gaps: u32,
    /// `None` until the first band sample, so a real 0 % never reads as "unset".
    fec: Option<(u8, u8)>,
    abr: Option<(u32, u32)>,
    /// The client's link rate and shape as last reported; they carry across minutes.
    link_kbps: u32,
    shape: LinkShape,
    /// `[head, mid, tail, socket]` loss over the minute.
    positions: [u32; 4],
}

impl LinkWindow {
    pub fn new(counters: &LinkCounters) -> LinkWindow {
        LinkWindow {
            started: std::time::Instant::now(),
            egress_base: counters.egress_bytes(),
            windows: 0,
            loss_windows: 0,
            loss_ppm_max: 0,
            loss_ppm_sum: 0,
            unrecovered: 0,
            rfi: 0,
            keyframe_req: 0,
            abr_backoffs: 0,
            gaps: 0,
            fec: None,
            abr: None,
            link_kbps: 0,
            shape: LinkShape::Auto,
            positions: [0; 4],
        }
    }

    /// One feedback datagram's levels: the link rate the pacer runs at and the shape.
    pub fn note_link(&mut self, link_kbps: u32, wake: bool) {
        if link_kbps != 0 {
            self.link_kbps = link_kbps;
        }
        self.shape = if wake {
            LinkShape::Wake
        } else {
            LinkShape::Auto
        };
    }

    /// One report window's loss by position, and the client socket's own drops.
    pub fn note_positions(&mut self, head: u32, mid: u32, tail: u32, sock_drops: u32) {
        for (sum, n) in self.positions.iter_mut().zip([head, mid, tail, sock_drops]) {
            *sum = sum.saturating_add(n);
        }
    }

    /// One closed client report window. `unrecovered_run` is
    /// [`UnrecoveredRun::report`](crate::native::bitrate::UnrecoveredRun)'s verdict on it.
    pub fn note_loss(&mut self, loss_ppm: u32, unrecovered_run: u32) {
        self.windows += 1;
        self.loss_ppm_sum += u64::from(loss_ppm);
        self.loss_ppm_max = self.loss_ppm_max.max(loss_ppm);
        if loss_ppm > 0 {
            self.loss_windows += 1;
        }
        if unrecovered_run > 0 {
            self.unrecovered += 1;
        }
    }

    pub fn note_rfi(&mut self) {
        self.rfi += 1;
    }

    pub fn note_keyframe_req(&mut self) {
        self.keyframe_req += 1;
    }

    pub fn note_gap(&mut self) {
        self.gaps += 1;
    }

    /// One client bitrate ask. `live` is the rate the encoder is actually running; an ask
    /// below it is the client's controller retreating.
    pub fn note_bitrate_ask(&mut self, requested_kbps: u32, live_kbps: u32) {
        if live_kbps != 0 && requested_kbps < live_kbps {
            self.abr_backoffs += 1;
        }
    }

    /// Widen the FEC and ABR bands. Called per report window and once more at close, so even a
    /// minute with no client reports carries the rates the session was running at.
    pub fn sample_bands(&mut self, fec_pct: u8, kbps: u32) {
        let f = self.fec.get_or_insert((fec_pct, fec_pct));
        *f = (f.0.min(fec_pct), f.1.max(fec_pct));
        let a = self.abr.get_or_insert((kbps, kbps));
        *a = (a.0.min(kbps), a.1.max(kbps));
    }

    /// Close the window: fold in the cross-thread counters, reset, return the line to emit.
    /// Called on the minute tick and once more when the session ends mid-minute.
    pub fn close(&mut self, counters: &LinkCounters, fec_pct: u8, kbps: u32) -> LinkMinute {
        self.sample_bands(fec_pct, kbps);
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let bytes = counters.egress_bytes().saturating_sub(self.egress_base);
        let (fec_min_pct, fec_max_pct) = self.fec.unwrap_or_default();
        let (abr_min_kbps, abr_max_kbps) = self.abr.unwrap_or_default();
        let mut m = LinkMinute {
            secs: elapsed.round() as u32,
            windows: self.windows,
            loss_windows: self.loss_windows,
            loss_max_ppm: self.loss_ppm_max,
            loss_mean_ppm: (self.loss_ppm_sum / u64::from(self.windows.max(1))) as u32,
            unrecovered: self.unrecovered,
            rfi: self.rfi,
            keyframe_req: self.keyframe_req,
            abr_backoffs: self.abr_backoffs,
            gaps: self.gaps,
            fec_min_pct,
            fec_max_pct,
            abr_min_kbps,
            abr_max_kbps,
            egress_kbps: (bytes as f64 * 8.0 / 1000.0 / elapsed) as u32,
            link_kbps: self.link_kbps,
            shape: self.shape,
            loss_head: self.positions[0],
            loss_mid: self.positions[1],
            loss_tail: self.positions[2],
            sock_drops: self.positions[3],
            ..LinkMinute::default()
        };
        counters.take(&mut m);
        let (link_kbps, shape) = (self.link_kbps, self.shape);
        *self = LinkWindow::new(counters);
        (self.link_kbps, self.shape) = (link_kbps, shape);
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_minute_is_all_zero_and_not_alarming() {
        let c = LinkCounters::default();
        let mut m = LinkMinute {
            secs: 60,
            ..LinkMinute::default()
        };
        c.take(&mut m);
        assert_eq!(
            m,
            LinkMinute {
                secs: 60,
                ..LinkMinute::default()
            }
        );
        assert!(!m.alarming());
        // The line still exists to be read: `/status` gets it too.
        assert_eq!(c.last(), Some(m));
    }

    #[test]
    fn the_warn_thresholds_fire_at_the_documented_counts() {
        let at = |intra, kf| {
            LinkMinute {
                intra_refresh: intra,
                keyframe_req: kf,
                ..LinkMinute::default()
            }
            .alarming()
        };

        assert!(!at(WARN_INTRA_REFRESH - 1, WARN_KEYFRAME_REQ - 1));
        assert!(at(WARN_INTRA_REFRESH, 0));
        assert!(at(0, WARN_KEYFRAME_REQ));
        // Heavy loss with no recovery asks is not this line's alarm: FEC is doing its job.
        assert!(!LinkMinute {
            loss_max_ppm: 900_000,
            windows: 80,
            ..LinkMinute::default()
        }
        .alarming());
    }

    #[test]
    fn counters_reset_per_minute() {
        let c = LinkCounters::default();
        c.set_session_id(10);
        c.note_recovery_au(true, false, false);
        c.note_recovery_au(false, false, true);
        c.note_rfi_declined();
        c.note_idr();
        c.note_retarget();

        let mut first = LinkMinute::default();
        c.take(&mut first);
        assert_eq!(
            (
                first.session_id,
                first.anchor_p,
                first.intra_refresh,
                first.rfi_declined,
                first.idr,
                first.retargets
            ),
            (10, 1, 1, 1, 1, 1)
        );

        let mut second = LinkMinute::default();
        c.take(&mut second);
        assert_eq!(second.anchor_p, 0);
        assert_eq!(second.intra_refresh, 0);
        assert_eq!(second.rfi_declined, 0);
        assert_eq!(second.idr, 0);
        assert_eq!(second.retargets, 0);
        // The id is a session fact, not a count: it survives the drain.
        assert_eq!(second.session_id, 10);
    }

    #[test]
    fn egress_is_a_diff_not_a_drain() {
        let c = LinkCounters::default();
        c.publish_egress_bytes(1_000_000);
        assert_eq!(c.egress_bytes(), 1_000_000);
        let mut m = LinkMinute::default();
        c.take(&mut m);
        assert_eq!(c.egress_bytes(), 1_000_000);
    }

    #[test]
    fn ppm_renders_as_percent() {
        assert_eq!(pct(0), "0.00");
        assert_eq!(pct(5_300), "0.53");
        assert_eq!(pct(1_000_000), "100.00");
    }

    #[test]
    fn a_silent_minute_still_reports_the_rates_it_ran_at() {
        let c = LinkCounters::default();
        let mut w = LinkWindow::new(&c);
        let m = w.close(&c, 5, 13_000);
        assert_eq!(m.windows, 0);
        assert_eq!((m.fec_min_pct, m.fec_max_pct), (5, 5));
        assert_eq!((m.abr_min_kbps, m.abr_max_kbps), (13_000, 13_000));
        assert!(!m.alarming());
    }

    #[test]
    fn a_window_folds_loss_and_widens_the_bands() {
        let c = LinkCounters::default();
        let mut w = LinkWindow::new(&c);
        w.sample_bands(5, 13_000);
        w.note_loss(0, 0);
        w.note_loss(5_300, 1);
        w.note_loss(1_700, 0);
        w.sample_bands(8, 8_000);
        w.note_rfi();
        w.note_rfi();
        w.note_keyframe_req();
        w.note_gap();
        // A climb is not a backoff; an ask under the running rate is.
        w.note_bitrate_ask(20_000, 13_000);
        w.note_bitrate_ask(9_000, 13_000);
        // No live rate yet: nothing to be under.
        w.note_bitrate_ask(1, 0);

        let m = w.close(&c, 8, 8_000);
        assert_eq!(m.windows, 3);
        assert_eq!(m.loss_windows, 2);
        assert_eq!(m.loss_max_ppm, 5_300);
        assert_eq!(m.loss_mean_ppm, 2_333);
        assert_eq!(m.unrecovered, 1);
        assert_eq!(m.rfi, 2);
        assert_eq!(m.keyframe_req, 1);
        assert_eq!(m.gaps, 1);
        assert_eq!(m.abr_backoffs, 1);
        assert_eq!((m.fec_min_pct, m.fec_max_pct), (5, 8));
        assert_eq!((m.abr_min_kbps, m.abr_max_kbps), (8_000, 13_000));
    }

    #[test]
    fn closing_starts_a_fresh_window() {
        let c = LinkCounters::default();
        let mut w = LinkWindow::new(&c);
        w.note_loss(5_300, 1);
        w.note_rfi();
        w.close(&c, 5, 13_000);

        let m = w.close(&c, 5, 13_000);
        assert_eq!(m.windows, 0);
        assert_eq!(m.loss_max_ppm, 0);
        assert_eq!(m.rfi, 0);
        assert_eq!(m.unrecovered, 0);
    }

    #[test]
    fn a_session_that_ends_mid_minute_reports_what_it_had() {
        let c = LinkCounters::default();
        c.note_idr();
        let mut w = LinkWindow::new(&c);
        w.note_keyframe_req();
        // No tick has fired; this is the tail close the control task does on its way out.
        let m = w.close(&c, 5, 13_000);
        assert_eq!(m.keyframe_req, 1);
        assert_eq!(m.idr, 1);
        assert!(m.secs < 60, "a partial window reports its own length");
    }

    /// Records the level and message of every event on the thread's subscriber.
    struct Tap(std::sync::Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Tap {
        fn on_event(&self, ev: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.0
                .lock()
                .unwrap()
                .push((*ev.metadata().level(), ev.metadata().name().to_string()));
        }
    }

    /// Level `emit` chose for `m`.
    fn emitted_level(m: &LinkMinute) -> tracing::Level {
        use tracing_subscriber::layer::SubscriberExt;
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::registry().with(Tap(seen.clone()));
        tracing::subscriber::with_default(sub, || {
            emit(m, std::net::IpAddr::from([192, 168, 1, 253]))
        });
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one line a minute");
        seen[0].0
    }

    #[test]
    fn a_clean_minute_logs_at_info_and_a_bad_one_escalates() {
        let clean = LinkMinute {
            secs: 60,
            windows: 80,
            ..LinkMinute::default()
        };
        assert_eq!(emitted_level(&clean), tracing::Level::INFO);
        assert_eq!(
            emitted_level(&LinkMinute {
                intra_refresh: WARN_INTRA_REFRESH - 1,
                keyframe_req: WARN_KEYFRAME_REQ - 1,
                ..clean.clone()
            }),
            tracing::Level::INFO
        );
        assert_eq!(
            emitted_level(&LinkMinute {
                intra_refresh: WARN_INTRA_REFRESH,
                ..clean.clone()
            }),
            tracing::Level::WARN
        );
        assert_eq!(
            emitted_level(&LinkMinute {
                keyframe_req: WARN_KEYFRAME_REQ,
                ..clean
            }),
            tracing::Level::WARN
        );
    }

    #[test]
    fn egress_is_the_windows_own_bytes() {
        let c = LinkCounters::default();
        c.publish_egress_bytes(1_000_000);
        let mut w = LinkWindow::new(&c);
        c.publish_egress_bytes(1_600_000);
        // 600 KB over a sub-second window is a large rate, but never the whole session's.
        assert!(w.close(&c, 5, 13_000).egress_kbps >= 4_800);
        assert_eq!(w.close(&c, 5, 13_000).egress_kbps, 0);
    }
}
