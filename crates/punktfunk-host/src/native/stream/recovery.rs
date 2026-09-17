//! Rate and recovery control between frames: the FEC-driven encoder-rate re-derivation, the
//! client's bitrate requests (in place, or an encoder rebuild), and the keyframe/RFI coalescing
//! with its recovery-cadence diagnosis.

use super::pipeline::open_session_encoder;
use super::state::{announce_pipeline_gap, Inflight, StreamState};
use super::*;

impl StreamState {
    /// Adaptive FEC moved: re-derive the encoder rate inside the unchanged wire budget.
    pub(super) fn on_fec_moved(&mut self) {
        if self.budget_identity {
            return;
        }
        let fec_now = self.fec_target.load(Ordering::Relaxed);
        if fec_now == self.last_fec {
            return;
        }
        let prev = self.enc_derive(self.last_fec).enc_kbps(self.bitrate_kbps);
        let want = self.enc_derive(fec_now).enc_kbps(self.bitrate_kbps);
        self.last_fec = fec_now;
        if want != prev && self.enc.reconfigure_bitrate(want as u64 * 1000) {
            tracing::debug!(
                fec_pct = fec_now,
                encoder_kbps = want,
                budget_kbps = self.bitrate_kbps,
                "adaptive FEC moved — encoder rate re-derived within the wire budget"
            );
        }
    }

    /// The client's latest bitrate ask: reconfigure in place when the encoder can, else rebuild
    /// it at the new rate, and tell the ceiling what the encoder made of it.
    ///
    /// The ask arrives already resolved against the ceiling — the control task spends it there so
    /// the ack and the encoder never disagree. Clamping again here would make an ack the client
    /// already holds a promise the encoder was never given.
    pub(super) fn on_bitrate_request(&mut self) {
        self.settle_applied_rate();
        let mut want_kbps = None;
        while let Ok(k) = self.bitrate_rx.try_recv() {
            want_kbps = Some(k);
        }
        self.enc
            .set_send_spread_us(self.send_spread_us.load(Ordering::Relaxed));
        let Some(new_kbps) = want_kbps.filter(|&k| k != self.bitrate_kbps) else {
            return;
        };
        let ed = self.enc_now();
        if self
            .enc
            .reconfigure_bitrate(ed.enc_kbps(new_kbps) as u64 * 1000)
        {
            let applied_kbps = self
                .enc
                .applied_bitrate_bps()
                .map(|b| (b / 1000) as u32)
                .filter(|&k| k > 0)
                .map(|k| ed.applied_budget_kbps(new_kbps, k))
                .unwrap_or(new_kbps);
            tracing::info!(
                from_kbps = self.bitrate_kbps,
                to_kbps = applied_kbps,
                requested_kbps = new_kbps,
                "encoder bitrate reconfigured in place (adaptive bitrate — no IDR)"
            );
            self.note_applied_rate(new_kbps, applied_kbps);
            if applied_kbps < self.bitrate_kbps {
                self.behind_score = 0;
            }
            self.counters.note_bitrate(applied_kbps);
            self.bitrate_kbps = applied_kbps;
            self.live_bitrate.store(applied_kbps, Ordering::Relaxed);
            return;
        }
        let hz = interval_hz(self.interval);
        let rebuild_t0 = std::time::Instant::now();
        match open_session_encoder(
            &self.plan,
            &*self.capturer,
            &self.frame,
            (self.negotiated.width, self.negotiated.height),
            hz,
            |_, _| ed.enc_kbps(new_kbps) as u64 * 1000,
            self.bit_depth,
            self.au_seq,
        ) {
            Ok((new_enc, reframe)) => {
                self.adopt_reframe(reframe);
                let applied_kbps = new_enc
                    .applied_bitrate_bps()
                    .map(|b| (b / 1000) as u32)
                    .filter(|&k| k > 0)
                    .map(|k| ed.applied_budget_kbps(new_kbps, k))
                    .unwrap_or(new_kbps);
                tracing::info!(
                    from_kbps = self.bitrate_kbps,
                    to_kbps = applied_kbps,
                    requested_kbps = new_kbps,
                    "encoder rebuilt at new bitrate (adaptive bitrate)"
                );
                self.enc = new_enc;
                self.note_applied_rate(new_kbps, applied_kbps);
                self.counters.note_bitrate(applied_kbps);
                self.bitrate_kbps = applied_kbps;
                self.live_bitrate.store(applied_kbps, Ordering::Relaxed);
                self.inflight.clear();
                self.last_au_at = std::time::Instant::now();
                self.encoder_resets = 0;
                self.last_forced_idr = Some(std::time::Instant::now());
                self.behind_score = 0;
                self.depth_frames = 0;
                self.ahead_run = 0;
                announce_pipeline_gap(
                    &self.gap_tx,
                    rebuild_t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                );
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), to_kbps = new_kbps,
                    "bitrate-change encoder rebuild failed — keeping the current rate");
                self.note_applied_rate(new_kbps, self.bitrate_kbps);
            }
        }
    }

    /// The rate the encoder settled on, read back after the fact.
    ///
    /// An encoder that applies a retarget on its own thread — the Windows IDD
    /// driver, whose bitrate control is a queued message with no reply —
    /// answers a frame later, so the read taken beside the request cannot see a
    /// decline. Only a retarget is read back this way, and only a rate below
    /// the session's own is acted on: an encoder that answers in place cannot
    /// trip it, because the session rate came from this same read-back.
    fn settle_applied_rate(&mut self) {
        if !self.retargeted {
            return;
        }
        let ed = self.enc_now();
        let Some(applied) = self
            .enc
            .applied_bitrate_bps()
            .map(|b| (b / 1000) as u32)
            .filter(|&k| k > 0)
            .map(|k| ed.applied_budget_kbps(self.bitrate_kbps, k))
            .filter(|&k| k < self.bitrate_kbps)
        else {
            return;
        };
        tracing::info!(
            from_kbps = self.bitrate_kbps,
            to_kbps = applied,
            "the encoder settled below the rate it was given — the session follows it"
        );
        self.note_applied_rate(self.bitrate_kbps, applied);
        self.counters.note_bitrate(applied);
        self.bitrate_kbps = applied;
        self.live_bitrate.store(applied, Ordering::Relaxed);
    }

    /// What the encoder made of `want`: the ceiling learns it, and a client that
    /// was promised `want` is corrected.
    ///
    /// A failed rebuild reports the rate it kept, so a refusal that costs no
    /// encoder at all still teaches the ceiling.
    fn note_applied_rate(&mut self, want: u32, applied: u32) {
        self.retargeted = true;
        self.encoder_ceiling
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_applied(want, applied);
        if applied < want {
            let _ = self.retarget_tx.send((applied, AckReason::EncoderLimit));
        }
    }

    /// Publish capture health, then fold every pending recovery ask (a recovered source stall,
    /// keyframe requests, `/status` force-IDR, RFI ranges) into at most one encoder action: an
    /// RFI when the encoder can anchor, else one IDR per cooldown.
    pub(super) fn on_recovery_requests(&mut self) {
        let mut want_kf = false;
        // Staged recovery closed on real source frames (WP14): the client held the last image
        // through the hole, so the next AU is an IDR, owed in-flight records are invalid, and
        // the measured local outage is announced so the straddling window is not scored as
        // congestion.
        if self.health_published_at.elapsed() >= std::time::Duration::from_millis(500) {
            self.health_published_at = std::time::Instant::now();
            *self
                .capture_health
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = self.capturer.health();
        }
        if let Some(outage) = self.capturer.take_recovered_outage() {
            let outage_ms = outage.as_millis().min(u32::MAX as u128) as u32;
            tracing::info!(
                outage_ms,
                "capture recovered from a source stall — forcing an IDR, announcing the gap"
            );
            want_kf = true;
            self.inflight.clear();
            self.last_forced_idr = Some(std::time::Instant::now());
            announce_pipeline_gap(&self.gap_tx, outage_ms);
        }
        while self.keyframe.try_recv().is_ok() {
            want_kf = true;
        }
        if self.force_idr.swap(false, Ordering::Relaxed) {
            want_kf = true;
        }
        let mut rfi_range: Option<(u32, u32)> = None;
        while let Ok((first, last)) = self.rfi.try_recv() {
            rfi_range = Some(match rfi_range {
                Some((pf, pl)) => (pf.min(first), pl.max(last)),
                None => (first, last),
            });
        }
        if self.plan.codec == crate::encode::Codec::PyroWave && (want_kf || rfi_range.is_some()) {
            tracing::debug!(
                want_kf,
                ?rfi_range,
                "PyroWave session: recovery request ignored (all-intra — next frame is the recovery)"
            );
            want_kf = false;
            rfi_range = None;
        }
        // An RFI the encoder declined is proof that recovery needs an IDR — the anchor
        // itself was lost, or every surviving reference is tainted. Not an echo.
        let mut rfi_declined = false;
        if !want_kf {
            if let Some((first, last)) = rfi_range {
                let width = last.wrapping_sub(first);
                if width > punktfunk_core::packet::RFI_MAX_RANGE {
                    tracing::debug!(first, last, width, "RFI range too wide — keyframe instead");
                    want_kf = true;
                } else if self.enc.caps().supports_rfi
                    && self.enc.invalidate_ref_frames(first as i64, last as i64)
                {
                    self.last_rfi = Some(std::time::Instant::now());
                } else {
                    want_kf = true;
                    rfi_declined = true;
                    self.counters.link.note_rfi_declined();
                }
            }
        }
        if want_kf {
            self.force_keyframe(rfi_declined);
        }
    }

    /// One forced IDR per cooldown. A stream whose recovery marks the client can lift on heals
    /// over ~0.5 s (2 s window); every other stream's only repair is the IDR, so the window is
    /// short — swallow the round-trip echo, re-issue a lost IDR promptly. An IDR the client
    /// re-asks after is one it lost or could not use; each such IDR doubles the cooldown
    /// ([`idr_cooldown`]) so the repair stops feeding the overload that defeats it.
    fn force_keyframe(&mut self, rfi_declined: bool) {
        const IDR_COOLDOWN_INTRA: std::time::Duration = std::time::Duration::from_secs(2);
        const IDR_COOLDOWN_FULL: std::time::Duration = std::time::Duration::from_millis(750);
        let base = if self.enc.caps().intra_refresh_recovery {
            IDR_COOLDOWN_INTRA
        } else {
            IDR_COOLDOWN_FULL
        };
        let now = std::time::Instant::now();
        match self
            .kf_gate
            .decide(now, base, self.last_forced_idr, self.last_rfi, rfi_declined)
        {
            KeyframeVerdict::Coalesced { cooldown, unhealed } => {
                // In-flight IDR has not repaired the client yet — do not RFI-anchor over that damage.
                self.enc.distrust_references();
                tracing::debug!(
                    cooldown_ms = cooldown.as_millis() as u64,
                    unhealed,
                    "keyframe request coalesced — within the IDR cooldown; RFI anchor trust \
                     withdrawn until the IDR repairs the client"
                );
            }
            KeyframeVerdict::RfiEcho { swallowed } => {
                // Do not distrust: the recovery frame is still in flight.
                tracing::debug!(
                    swallowed,
                    "keyframe request coalesced — echo of an RFI-recovered loss"
                );
            }
            KeyframeVerdict::Force {
                cooldown,
                unhealed,
                rfi_unhealed,
            } => {
                tracing::debug!(
                    rfi_unhealed,
                    unhealed,
                    cooldown_ms = cooldown.as_millis() as u64,
                    "forcing keyframe (client decode recovery)"
                );
                if rfi_unhealed {
                    self.enc.distrust_references();
                }
                self.enc.request_keyframe();
                self.counters.link.note_idr();
                self.last_forced_idr = Some(now);
                if unhealed == IDR_STORM {
                    tracing::warn!(
                        unhealed,
                        cooldown_ms = cooldown.as_millis() as u64,
                        "forced IDRs are not healing the client — each landed and a fresh \
                         keyframe request followed. The client is losing the repair itself: a \
                         client that cannot drain the stream (decoder, receive buffer) or a link \
                         below the bitrate does this, and it is NOT a host display disturbance. \
                         IDR cooldown backed off. Lower the bitrate or set it to Automatic; the \
                         client log's 'receive backlog stopped draining' (queue_depth) decides"
                    );
                }
                // A re-issue is the host's own cooldown, not a disturbance with a period.
                if unhealed == 0 {
                    if let Some(period) = self.recovery_cadence.note(now) {
                        self.diagnose_recovery_cadence(period);
                    }
                }
            }
        }
    }

    /// Forced keyframes have become periodic: say which side is at fault, from the client's
    /// delivery count and the wire re-key history — never a guess at the display.
    fn diagnose_recovery_cadence(&self, period: std::time::Duration) {
        let client_rx = self.client_packets_received.load(Ordering::Relaxed);
        let sent = self.sent;
        if client_rx == 0 {
            tracing::error!(
                period_s = format!("{:.1}", period.as_secs_f64()),
                frames_sent = sent,
                "THE VIDEO DATA PLANE IS NOT REACHING THE CLIENT — it reports 0 \
                 packets received all session while this host has sent the frames \
                 counted here, so the picture is black and every keyframe we force is \
                 wasted. The control plane is healthy (this report arrived on it), so \
                 the session looks alive: audio, input and the library keep working. \
                 READ THE 'data plane bound' LINE ABOVE — it says which leg failed, \
                 and this line cannot. `punched=false`: the client's hole-punch never \
                 arrived, so inbound UDP to this host's per-session data port is \
                 blocked — open it (the ports are ephemeral, so the rule must be \
                 program-scoped, not port-scoped). `punched=true`: inbound is FINE and \
                 the failure is on the return leg — compare that line's `local=` \
                 source address against the host address this client dialed, because \
                 its data socket is connected and its kernel silently drops video from \
                 any other source. If those match, the datagrams left this host \
                 correctly and the client either never received them (a hop on the \
                 path) or received them and could not open them: this counter is \
                 incremented AFTER decrypt and replay checks, so a session whose every \
                 datagram failed to open reports exactly this same zero"
            );
        } else if matches_client_recovery_cooldown(period) {
            if client_rx == u32::MAX {
                tracing::warn!(
                    period_s = format!("{:.1}", period.as_secs_f64()),
                    frames_sent = sent,
                    "client keyframe recoveries land on a client software cooldown, \
                     but this client is too old to report whether any video reached \
                     it — so this is EITHER a client that cannot sustain the stream \
                     and is shedding a standing receive queue, OR a client that has \
                     received nothing at all and is re-asking on its no-video timer. \
                     They are opposite faults; the host cannot tell them apart from \
                     the period. Its log does: 'receive backlog stopped draining' \
                     (with queue_depth) means the first, 'no video received … into \
                     the session' means the second. Upgrading the client makes this \
                     line decide on its own"
                );
            } else {
                tracing::warn!(
                    period_s = format!("{:.1}", period.as_secs_f64()),
                    client_packets_received = client_rx,
                    "client keyframe recoveries match the client's jump-to-live \
                     cooldown, and it confirms video IS arriving — the CLIENT cannot \
                     sustain the stream and is shedding a standing receive queue \
                     (check its log for 'receive backlog stopped draining' with \
                     queue_depth, and for a decode rung that demoted); a slower \
                     decode path or a link below the bitrate does this, and it is NOT \
                     a host display disturbance"
                );
            }
        } else if self.wire_rekeys.load(Ordering::Relaxed) > 0 {
            tracing::warn!(
                period_s = format!("{:.1}", period.as_secs_f64()),
                wire_rekeys = self.wire_rekeys.load(Ordering::Relaxed),
                "client keyframe recoveries are METRONOMIC on a session whose wire \
                 MTU had to be re-keyed mid-stream — a constrained path (VPN/overlay \
                 adapter, lowered NIC MTU) black-holing full-size video is the prime \
                 suspect, NOT a host/display disturbance; see the 'wire MTU' lines \
                 above, and pin PUNKTFUNK_WIRE_MTU to skip the lossy discovery window \
                 on this path"
            );
        } else if cfg!(windows) {
            tracing::warn!(
                period_s = format!("{:.1}", period.as_secs_f64()),
                "client keyframe recoveries are METRONOMIC — a periodic host/display \
                 disturbance (display-topology churn, display-poller software, \
                 virtual-display timing) is the likely cause, not random network \
                 loss; correlate with 'slow display-descriptor poll' / 'display \
                 descriptor changed' / 'IDD-push capture stall' lines"
            );
        } else {
            tracing::warn!(
                period_s = format!("{:.1}", period.as_secs_f64()),
                "client keyframe recoveries are METRONOMIC — a timer, not random loss. \
                 'audio egress' with late=0 in the same window clears this process; then \
                 read the 'wire egress' line for that window (outq_max_kb, tx_dropped, \
                 carrier_changes = this box's kernel or NIC) and any 'network changed' \
                 line at the same instant (DHCP renewal, route or link flap). Both clean \
                 = the path or the client: the client's per-second stats decide"
            );
        }
    }
}

/// Rebuild the encoder in place and drop owed in-flight AUs. `false` = no in-place reset.
pub(super) fn reset_stalled_encoder(
    enc: &mut Box<dyn crate::encode::Encoder>,
    inflight: &mut Inflight,
) -> bool {
    if !enc.reset() {
        return false;
    }
    inflight.clear();
    enc.request_keyframe();
    true
}

/// The ladder rungs whose actuator this loop owns because the encoder or the display manager
/// does. `EncoderReset` is [`reset_stalled_encoder`] plus a bounded wait for the first access
/// unit — the rung's whole cost, one IDR included. `DriverCycle` reaps the WUDFHost and reloads
/// the adapter (seconds, the display black for the cycle); its `Applied` ends the capturer so
/// the pipeline rebuild reopens SET_ENCODE and the ring against the fresh host.
pub(super) fn run_loop_stage(
    stage: pf_frame::recovery::Stage,
    enc: &mut Box<dyn crate::encode::Encoder>,
    inflight: &mut Inflight,
) -> pf_frame::recovery::StageOutcome {
    use pf_frame::recovery::{Stage, StageOutcome, ENCODER_RESET_FIRST_AU};
    match stage {
        Stage::EncoderReset => {
            let t0 = std::time::Instant::now();
            if !reset_stalled_encoder(enc, inflight) {
                return StageOutcome::Failed;
            }
            let first_au = enc.ready_aus(t0 + ENCODER_RESET_FIRST_AU).map(|n| n > 0);
            tracing::warn!(
                cost_ms = t0.elapsed().as_millis() as u64,
                first_au,
                "recovery: encoder reset applied — one IDR plus the first-AU wait"
            );
            StageOutcome::Applied
        }
        #[cfg(target_os = "windows")]
        Stage::DriverCycle => {
            let t0 = std::time::Instant::now();
            match crate::vdisplay::driver::force_driver_cycle() {
                Ok(()) => {
                    tracing::warn!(
                        cost_ms = t0.elapsed().as_millis() as u64,
                        "recovery: driver cycle — adapter reloaded, display black for the cycle; \
                         the session rebuilds against the fresh WUDFHost"
                    );
                    StageOutcome::Applied
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "recovery: driver cycle failed");
                    StageOutcome::Failed
                }
            }
        }
        _ => StageOutcome::Unsupported,
    }
}

/// A recovery frame's flight: encode, wire, decode. A request inside it echoes the loss the
/// frame repairs; one past it says the frame landed and did not.
const RECOVERY_FLIGHT: std::time::Duration = std::time::Duration::from_millis(300);

/// Longest wait between forced IDRs, whatever the backoff. 3 s of a broken picture is the
/// ceiling a user tolerates over a storm of repairs that never take.
const IDR_COOLDOWN_MAX: std::time::Duration = std::time::Duration::from_secs(3);

/// Consecutive unhealed IDRs that name the state in the log, once.
const IDR_STORM: u32 = 3;

/// A request this long after the previous one opens a new episode: the run of unhealed IDRs,
/// the swallowed RFI echoes and the previous request all belong to the old one.
const KF_EPISODE_RESET: std::time::Duration = std::time::Duration::from_secs(1);

/// Requests the [`RECOVERY_FLIGHT`] echo hedge swallows before the next one forces an IDR.
const RFI_ECHO_MAX_SWALLOWED: u32 = 2;

/// `base` doubled per unhealed IDR, capped at [`IDR_COOLDOWN_MAX`].
fn idr_cooldown(base: std::time::Duration, unhealed: u32) -> std::time::Duration {
    base.saturating_mul(1u32 << unhealed.min(2))
        .min(IDR_COOLDOWN_MAX)
}

/// What one keyframe request gets ([`KeyframeGate::decide`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyframeVerdict {
    /// Inside the IDR cooldown: nothing sent, RFI anchor trust withdrawn.
    Coalesced {
        cooldown: std::time::Duration,
        unhealed: u32,
    },
    /// Echo of a loss an in-flight RFI repairs: nothing sent.
    RfiEcho { swallowed: u32 },
    /// Send the IDR. `unhealed` = consecutive IDRs the client re-asked after, this one
    /// included; `rfi_unhealed` = an RFI echo was swallowed and the client still asks.
    Force {
        cooldown: std::time::Duration,
        unhealed: u32,
        rfi_unhealed: bool,
    },
}

/// Episode state behind [`StreamState::force_keyframe`]: which requests coalesce, and how far
/// the IDR cooldown has backed off. A request past the last IDR's [`RECOVERY_FLIGHT`] means
/// that IDR landed and did not heal; each such IDR doubles the cooldown ([`idr_cooldown`]);
/// a cooldown no such request lands in resets the run, as does a [`KF_EPISODE_RESET`] gap.
#[derive(Default)]
pub(super) struct KeyframeGate {
    last_request: Option<std::time::Instant>,
    rfi_echo_swallowed: u32,
    unhealed: u32,
}

impl KeyframeGate {
    pub(super) fn decide(
        &mut self,
        now: std::time::Instant,
        base: std::time::Duration,
        last_idr: Option<std::time::Instant>,
        last_rfi: Option<std::time::Instant>,
        rfi_declined: bool,
    ) -> KeyframeVerdict {
        let new_episode = self
            .last_request
            .is_none_or(|t| now.duration_since(t) > KF_EPISODE_RESET);
        if new_episode {
            self.rfi_echo_swallowed = 0;
            self.unhealed = 0;
        }
        let prev_request = self.last_request.replace(now).filter(|_| !new_episode);
        let cooldown = idr_cooldown(base, self.unhealed);
        let since_idr = |t: Option<std::time::Instant>| t.map(|t| now.saturating_duration_since(t));
        if since_idr(last_idr).is_some_and(|d| d < cooldown) {
            return KeyframeVerdict::Coalesced {
                cooldown,
                unhealed: self.unhealed,
            };
        }
        if !rfi_declined
            && since_idr(last_rfi).is_some_and(|d| d < RECOVERY_FLIGHT)
            && self.rfi_echo_swallowed < RFI_ECHO_MAX_SWALLOWED
        {
            self.rfi_echo_swallowed += 1;
            return KeyframeVerdict::RfiEcho {
                swallowed: self.rfi_echo_swallowed,
            };
        }
        let missed = last_idr
            .zip(prev_request)
            .is_some_and(|(idr, req)| req.saturating_duration_since(idr) >= RECOVERY_FLIGHT);
        self.unhealed = if missed { self.unhealed + 1 } else { 0 };
        let rfi_unhealed = std::mem::take(&mut self.rfi_echo_swallowed) > 0;
        KeyframeVerdict::Force {
            cooldown: idr_cooldown(base, self.unhealed),
            unhealed: self.unhealed,
            rfi_unhealed,
        }
    }
}

/// ±10 % of [`FLUSH_COOLDOWN`]: a software cooldown is the most periodic thing in the system.
fn matches_client_flush_cadence(period: std::time::Duration) -> bool {
    let flush = punktfunk_core::client::FLUSH_COOLDOWN;
    period.abs_diff(flush) < flush / 10
}

/// ±10 % of [`NO_VIDEO_RETRY`]. Opposite fault from flush cadence; only the delivery count tells which.
fn matches_client_no_video_cadence(period: std::time::Duration) -> bool {
    let no_video = punktfunk_core::client::NO_VIDEO_RETRY;
    period.abs_diff(no_video) < no_video / 10
}

fn matches_client_recovery_cooldown(period: std::time::Duration) -> bool {
    matches_client_flush_cadence(period) || matches_client_no_video_cadence(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drives [`KeyframeGate`] like the encode loop: a request every `step` from `t`, each
    /// answered by the verdict, a forced IDR stamping `last_idr`. Returns the forced IDRs'
    /// `(offset_ms, unhealed, cooldown)`.
    fn storm(
        gate: &mut KeyframeGate,
        base: Duration,
        t0: Instant,
        last_idr: &mut Option<Instant>,
        from_ms: u64,
        to_ms: u64,
        step_ms: u64,
    ) -> Vec<(u64, u32, Duration)> {
        let mut forced = Vec::new();
        let mut ms = from_ms;
        while ms < to_ms {
            let now = t0 + Duration::from_millis(ms);
            if let KeyframeVerdict::Force {
                cooldown, unhealed, ..
            } = gate.decide(now, base, *last_idr, None, false)
            {
                *last_idr = Some(now);
                forced.push((ms, unhealed, cooldown));
            }
            ms += step_ms;
        }
        forced
    }

    /// A client that asks again after every IDR has landed is not healed by IDRs: the
    /// cooldown doubles to the cap and holds there; a cooldown it stays quiet through, or an
    /// episode gap, puts the next loss back on the base cooldown.
    #[test]
    fn an_idr_the_client_reasks_after_backs_the_cooldown_off_to_the_cap() {
        let base = Duration::from_millis(750);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut gate = KeyframeGate::default();
        let mut last_idr = None;
        // Requests every 230 ms for 10 s: the first forces, the rest land past the flight.
        let forced = storm(&mut gate, base, t0, &mut last_idr, 0, 10_000, 230);
        let shape: Vec<_> = forced.iter().map(|&(_, u, c)| (u, c)).collect();
        assert_eq!(
            &shape[..4],
            &[(0, base), (1, ms(1500)), (2, ms(3000)), (3, ms(3000))],
            "double, then hold at the cap"
        );
        assert!(shape[4..]
            .iter()
            .all(|&(u, c)| u >= IDR_STORM && c == ms(3000)));
        assert!(
            forced.len() < 10_000 / 750 / 2,
            "{} IDRs in 10 s is the storm, not the backoff",
            forced.len()
        );
        // Quiet through a whole cooldown after the last IDR: the next loss starts over.
        let (last_ms, ..) = *forced.last().unwrap();
        let quiet = t0 + ms(last_ms + 3_500);
        assert!(matches!(
            gate.decide(quiet, base, last_idr, None, false),
            KeyframeVerdict::Force { unhealed: 0, cooldown, .. } if cooldown == base
        ));
        last_idr = Some(quiet);
        // One late re-ask inside the cooldown, then a long healthy silence: the old episode's
        // request must not turn the next unrelated loss into an unhealed IDR.
        assert!(matches!(
            gate.decide(quiet + ms(500), base, last_idr, None, false),
            KeyframeVerdict::Coalesced { unhealed: 0, .. }
        ));
        assert!(matches!(
            gate.decide(quiet + Duration::from_secs(600), base, last_idr, None, false),
            KeyframeVerdict::Force { unhealed: 0, cooldown, .. } if cooldown == base
        ));
        // The intra-refresh window is capped too, never 8 s.
        assert_eq!(idr_cooldown(Duration::from_secs(2), 3), IDR_COOLDOWN_MAX);
    }

    /// An echo inside an RFI's flight is swallowed twice and then forces with the anchor
    /// distrusted; an echo inside an IDR's flight is the same loss, not an unhealed IDR.
    #[test]
    fn echoes_inside_a_recovery_flight_are_the_same_loss() {
        let base = Duration::from_millis(750);
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut gate = KeyframeGate::default();
        let rfi = Some(t0);
        assert_eq!(
            gate.decide(t0 + ms(100), base, None, rfi, false),
            KeyframeVerdict::RfiEcho { swallowed: 1 }
        );
        assert_eq!(
            gate.decide(t0 + ms(200), base, None, rfi, false),
            KeyframeVerdict::RfiEcho { swallowed: 2 }
        );
        assert!(matches!(
            gate.decide(t0 + ms(250), base, None, rfi, false),
            KeyframeVerdict::Force {
                rfi_unhealed: true,
                unhealed: 0,
                ..
            }
        ));
        // A declined RFI is proof, never an echo.
        let mut gate = KeyframeGate::default();
        assert!(matches!(
            gate.decide(t0 + ms(100), base, None, rfi, true),
            KeyframeVerdict::Force {
                rfi_unhealed: false,
                ..
            }
        ));
        // IDR at t0, echo at +200 ms (inside the flight), next loss after the cooldown.
        let idr = Some(t0);
        assert!(matches!(
            gate.decide(t0 + ms(200), base, idr, None, false),
            KeyframeVerdict::Coalesced { .. }
        ));
        assert!(matches!(
            gate.decide(t0 + ms(800), base, idr, None, false),
            KeyframeVerdict::Force { unhealed: 0, .. }
        ));
    }

    /// The 2026-08-13 field log's exact reading — `period_s=2.0` — must be attributed to the
    /// client's backlog shedding, not to a host display disturbance. The whole point of routing
    /// on the shared constant is that this stays true if the cooldown is ever retuned, so the
    /// test derives its cases from `FLUSH_COOLDOWN` instead of hardcoding two seconds.
    #[test]
    fn a_recovery_cadence_on_the_clients_cooldown_is_not_blamed_on_the_display() {
        let flush = punktfunk_core::client::FLUSH_COOLDOWN;
        assert!(matches_client_flush_cadence(flush), "the field reading");
        assert!(matches_client_flush_cadence(flush + flush / 20));
        assert!(matches_client_flush_cadence(flush - flush / 20));

        assert!(!matches_client_flush_cadence(flush / 2));
        assert!(!matches_client_flush_cadence(flush * 2));
        assert!(!matches_client_flush_cadence(flush + flush / 5));
        assert!(!matches_client_flush_cadence(std::time::Duration::ZERO));
    }

    #[test]
    fn the_two_client_cooldowns_are_distinguishable_and_both_excluded_from_display_blame() {
        let flush = punktfunk_core::client::FLUSH_COOLDOWN;
        let no_video = punktfunk_core::client::NO_VIDEO_RETRY;
        assert_ne!(
            flush, no_video,
            "identical cooldowns make the host's verdict a coin flip"
        );
        // Neither may fall inside the other's ±10% band, or the period stops discriminating.
        assert!(!matches_client_flush_cadence(no_video));
        assert!(!matches_client_no_video_cadence(flush));
        // Both are client software cooldowns: never the metronomic display-disturbance branch.
        assert!(matches_client_recovery_cooldown(flush));
        assert!(matches_client_recovery_cooldown(no_video));
        // A real periodic disturbance still reaches that branch.
        assert!(!matches_client_recovery_cooldown(flush * 3));
        assert!(!matches_client_recovery_cooldown(std::time::Duration::ZERO));
    }
}
