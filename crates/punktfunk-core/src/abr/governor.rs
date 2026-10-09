//! Equal shares of one path, for the host that can see every session on it.
//!
//! A client's controller reads one session's window and cannot tell its own
//! wall from a sibling's queue. The host can: sessions from one client address
//! form a group, and [`shares`] divides what the group is getting between the
//! Automatic ones. A fixed-rate session takes what it is set to and is never
//! told anything; an idle one lends what it is not using and takes it back
//! when it asks.
//!
//! A share is a ceiling, not a target. The client still earns every step up to
//! it through its own evidence, and nothing here moves a share by less than
//! the band. One address is one NAT and not proof of one bottleneck, so the
//! arithmetic is wrong in the safe direction: a member that is getting what it
//! asked for is never cut, whatever a sibling is doing on its own air.
//!
//! The host calls this on every delivery report and the simulator calls it on
//! its own clock. One implementation, two callers — which is what lets the
//! simulator show that the host's loop above the client's does not oscillate.

use super::controller::FLOOR_KBPS;
use std::time::{Duration, Instant};

/// How often an up-move may go out. Down-moves apply as soon as the evidence
/// does; a share that climbed every report window would be the client's own
/// growth law again, one round-trip behind it.
pub const SHARE_CLOCK: Duration = Duration::from_secs(5);

/// How often a standing ceiling breathes a band, so a share cannot outlive
/// the crowding that taught it (L1).
///
/// Twelve times the share clock, because this one has no evidence behind it:
/// a group pressed against its shares is delivering exactly what it asks for,
/// and the only way to find out whether the path grew is to ask for more and
/// see. Every ask costs a rebuild and a window of queue, so it is asked about
/// as often as a learned cap re-probes a standing limit.
pub const SHARE_LIFT_CLOCK: Duration = Duration::from_secs(60);

/// The two clocks an up-move rides. A cut needs neither.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clocks {
    /// [`SHARE_CLOCK`] fired: a session may be told about room a sibling left
    /// it.
    pub room: bool,
    /// [`SHARE_LIFT_CLOCK`] fired: a standing ceiling may breathe.
    pub lift: bool,
}

/// A share moves only when it differs from the standing one by more than a
/// tenth. Under that the two controllers trade the same kilobits back and
/// forth for the life of the session.
const SHARE_BAND_DIV: u32 = 10;

/// Delivered within a sixteenth of what the host put on the wire: the path is
/// carrying what this session offers it.
///
/// Tighter than the band on purpose, and that is what settles the two loops.
/// A session riding one band above its fair share already reads as short, so
/// the budget stays pinned at what arrives and the share it would be handed
/// is inside the band of the one it holds. Loosen this past the band and the
/// pair saws between a notch over and a notch under all session.
const SHORT_DIV: u32 = 16;

/// A share of `0` releases the ceiling: the group is gone, or this session is
/// alone on the path again.
pub const NO_SHARE_KBPS: u32 = 0;

/// The host's send counter and the client's receive counter as they stood at
/// the last delivery report, so the next one gives a rate for the same window.
///
/// The report is the boundary: the client closes one per report window, and
/// both figures are cumulative, so the pair of diffs describes one stretch of
/// link rather than two overlapping ones. A window the client discarded owes no
/// report, which makes the next one long rather than wrong.
pub struct ShareWindow {
    at: Instant,
    egress_bytes: u64,
    packets_received: u64,
    /// The window that closed before this one carried the stream: its egress
    /// was over [`FLOOR_KBPS`].
    streaming: bool,
}

impl ShareWindow {
    pub fn new(now: Instant, egress_bytes: u64) -> Self {
        ShareWindow {
            at: now,
            egress_bytes,
            packets_received: 0,
            streaming: false,
        }
    }

    /// `(offered, delivered)` over the window that just closed, kbps, and
    /// whether the session was already streaming when it opened.
    ///
    /// Delivered is the client's packet count in this session's wire packets:
    /// the datagram size both ends agreed on, which is what the host has
    /// without asking for a byte count it never sends. The third figure is
    /// [`Member::streaming`], which says whether the pair is a reading of the
    /// path at all.
    pub fn close(
        &mut self,
        now: Instant,
        egress_bytes: u64,
        packets_received: u64,
        wire_bytes: u64,
    ) -> (u32, u32, bool) {
        let ms = now.duration_since(self.at).as_millis().max(1) as u64;
        let kbps = |bytes: u64| u32::try_from(bytes * 8 / ms).unwrap_or(u32::MAX);
        let offered = kbps(egress_bytes.saturating_sub(self.egress_bytes));
        let arrived = packets_received.saturating_sub(self.packets_received);
        let opened_streaming = self.streaming;
        *self = ShareWindow {
            at: now,
            egress_bytes,
            packets_received,
            streaming: offered >= FLOOR_KBPS,
        };
        (
            offered,
            kbps(arrived.saturating_mul(wire_bytes)),
            opened_streaming,
        )
    }
}

/// One session of a group, as the host knows it.
///
/// Every field is something the host has without asking the client: the
/// encoder target it set, the delivered rate its report windows come to,
/// and whether it is repeating a keepalive instead of encoding motion.
#[derive(Clone, Copy, Debug)]
pub struct Member {
    /// `false` = an explicit bitrate. Never clamped, never acked.
    pub automatic: bool,
    /// Encoder target, kbps.
    pub current_kbps: u32,
    /// Wire rate the host actually put out for this session, kbps. A pipeline
    /// still coming up offers almost nothing, and a delivery report short of
    /// nothing is not the path talking.
    pub offered_kbps: u32,
    /// What reached the client over the last report window, kbps. `None`
    /// before this session's first delivery report: a zero there would read
    /// as a path refusing everything it was offered.
    pub delivered_kbps: Option<u32>,
    /// The host is sending keepalive repeats: this session is not asking for
    /// its share.
    pub idle: bool,
    /// The share this session was last told. `None` = it has never had one.
    pub share_kbps: Option<u32>,
    /// The window before this one carried the stream too ([`ShareWindow`]).
    /// The window a stream starts in is short of what left the host by
    /// everything still in flight at its boundary, which is not the path
    /// refusing anything.
    pub streaming: bool,
}

/// The path refused some of what this session offered it.
///
/// A session that has not put the floor rate through for two windows running
/// says nothing about the path: the pipeline is still coming up, a window
/// carrying the audio reservation and one frame is a shortfall of noise, and
/// the window a stream starts in is short by everything still in flight at its
/// boundary.
fn short(m: &Member) -> bool {
    m.streaming
        && !m.idle
        && m.offered_kbps >= FLOOR_KBPS
        && m.delivered_kbps
            .is_some_and(|d| d < m.offered_kbps - m.offered_kbps / SHORT_DIV)
}

/// Shares for a group, in the members' own order. `Some(kbps)` is a ceiling to
/// send; `None` leaves the standing one alone.
///
/// `path_kbps` is the most this group has been seen to carry between them —
/// caller-kept, because a session that has gone still is not measuring the
/// path any more and its sibling should still be told the room is there. It
/// expires on [`crowded`]: a group that is re-measuring its path now must not
/// spend a figure from before the path changed. `clocks` says which up-moves
/// may go out; without either, only cuts do.
///
/// Fewer than two members is not a group and nothing is said, which is what
/// keeps a single session's decisions byte-identical to an ungoverned build.
/// Telling a session its group has ended is the caller's: only it knows there
/// was one, and what it should hand over is [`path_kbps`] rather than a share
/// of it.
pub fn shares(members: &[Member], path_kbps: u32, clocks: Clocks) -> Vec<Option<u32>> {
    let mut out = vec![None; members.len()];
    if members.len() < 2 {
        return out;
    }
    let auto: Vec<usize> = (0..members.len())
        .filter(|&i| members[i].automatic)
        .collect();
    if auto.is_empty() {
        return out;
    }
    // Nobody short of what was put on the wire for it: nothing has bound this
    // path, so nothing here may cut. What is left worth saying is to a member
    // sitting under what the group has room for — the wall it measured beside
    // the others was their residual, and only the host can see that.
    let crowded = crowded(members);
    let Some(budget) = budget_kbps(members, path_kbps, crowded) else {
        return out;
    };
    let equal = (budget / auto.len() as u64) as u32;
    // Max-min fair: a member that wants less than an equal share takes what it
    // wants, and the rest split what it left behind.
    let mut order = auto.clone();
    order.sort_by_key(|&i| demand(&members[i], equal));
    let (mut left, mut rest) = (auto.len() as u64, budget);
    for &i in &order {
        let m = &members[i];
        let take = u64::from(demand(m, equal)).min(rest / left);
        rest -= take;
        left -= 1;
        // Lending decides what the others may have, never what the lender is
        // allowed: its ceiling stays at an equal share, so taking it back
        // costs nothing and a session pinned under its share is not read as
        // one that does not want it. Over-committed while somebody is still,
        // which is the safe direction: the path answers the moment both ask.
        let take = take.max(u64::from(equal));
        if !crowded {
            // A lift or nothing, from two rules. A standing ceiling breathes a
            // band on its clock, so it cannot outlive the crowding that taught
            // it (L1). And a session two bands under the room this group has
            // is told so: its wall was the others' residual. Two, because a
            // group's delivery wanders by about one.
            let breathe = m
                .share_kbps
                .filter(|_| clocks.lift)
                .map(|s| s + s / SHARE_BAND_DIV);
            let take = take as u32;
            let room = (clocks.room
                && take > m.current_kbps + m.current_kbps / (SHARE_BAND_DIV / 2))
                .then_some(take);
            if let Some(want) = breathe.into_iter().chain(room).max() {
                out[i] = send(m.share_kbps, want.max(FLOOR_KBPS), true);
            }
            continue;
        }
        // A member the path is carrying whole is never cut: one address is one
        // NAT, and a sibling's trouble is not evidence about its own air. One
        // that is short is the member filling the queue, and its fair share is
        // the answer.
        let keep = if short(m) {
            0
        } else {
            m.delivered_kbps.unwrap_or(0)
        };
        let share = (take as u32).max(keep).max(FLOOR_KBPS);
        out[i] = send(m.share_kbps, share, clocks.room);
    }
    out
}

/// What there is to divide. `None` = this group has no figure to divide.
///
/// Once a member has gone short it is the group's delivery right now: the one
/// wall they measure together, re-taken every window, which is what gives the
/// bound its expiry (L1). A wall one of them measured beside a sibling read
/// that sibling's residual, not the path. While nobody is short the most this
/// group has been seen to carry stands instead, so the room a session lent by
/// going still is still there when its sibling asks for it. A fixed-rate
/// session's rate comes off the top either way — it is not in the division.
fn budget_kbps(members: &[Member], path_kbps: u32, crowded: bool) -> Option<u64> {
    let now = self::path_kbps(members)?;
    let proved = if crowded { now } else { now.max(path_kbps) };
    let fixed: u64 = members
        .iter()
        .filter(|m| !m.automatic)
        .map(|m| u64::from(m.current_kbps))
        .sum();
    Some(u64::from(proved).saturating_sub(fixed))
}

/// The path is refusing some of what this group offers it, so what arrives
/// this window is a fresh reading of it.
///
/// The caller keeps [`path_kbps`] as the most it has seen, and this is when
/// that memory expires: a group that has just re-measured its path must not
/// keep spending a figure from before the path changed (L1).
pub fn crowded(members: &[Member]) -> bool {
    members.iter().any(short)
}

/// What this group is carrying between them, kbps. `None` while any member has
/// yet to say what reaches it.
///
/// A member that says nothing is not a member delivering nothing: a client
/// that has not closed its first report window has sent no count, and reading
/// its silence as zero would halve its sibling's share to cover a session
/// nobody can see. So the group is left alone until every one of them has
/// reported.
///
/// The caller keeps the last one: a session left alone on the path is told it,
/// because the wall it measured beside a sibling was that sibling's residual
/// and only the host knows the sibling has gone.
pub fn path_kbps(members: &[Member]) -> Option<u32> {
    members.iter().map(|m| m.delivered_kbps).sum()
}

/// What a member would use if the path were free. `u32::MAX` = everything it
/// can get.
///
/// A session that is idle, still, or held under its share by its own decoder
/// lends the difference; one the path is refusing bytes to is not lending, it
/// is being starved, and asks for the whole share.
fn demand(m: &Member, equal_kbps: u32) -> u32 {
    if m.idle {
        let d = m.delivered_kbps.unwrap_or(0);
        return d + d / 4;
    }
    if !short(m) && m.offered_kbps < equal_kbps {
        return m.offered_kbps + m.offered_kbps / SHARE_BAND_DIV;
    }
    u32::MAX
}

/// Whether this share is worth an ack: outside the band, and a rise only on
/// the clock.
fn send(standing: Option<u32>, share: u32, may_raise: bool) -> Option<u32> {
    let Some(old) = standing else {
        return Some(share);
    };
    if share.abs_diff(old) < old / SHARE_BAND_DIV {
        return None;
    }
    (share < old || may_raise).then_some(share)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abr::{Action, Driver, DriverConfig};
    use crate::stats::Stats;

    /// One wire packet: shard payload plus its header and seal.
    const WIRE: u64 = 1_448;

    /// Every delivery count a real [`Driver`] asks the host for over `windows`
    /// report windows, and the millisecond of the ask. Driven as the pump drives
    /// it: session counters in, actions out, a packet a millisecond arriving.
    fn delivery_reports(windows: u64) -> Vec<(u64, u64)> {
        let base = Instant::now();
        let mut d = Driver::new(
            DriverConfig {
                start_kbps: 20_000,
                ceiling_cap_kbps: None,
                stream_cap_kbps: 200_000,
                refresh_hz: 60,
                codec: crate::quic::CODEC_HEVC,
                bit_depth: 8,
                chroma_format: crate::quic::CHROMA_IDC_420,
                audio_reserved_kbps: 256,
                marks_repeats: true,
                probe: false,
                probe_target_kbps: None,
                ramp: false,
                probe_only: false,
                pin_kbps: None,
                floor_kbps: None,
            },
            base,
        );
        let mut st = Stats::default();
        let mut out = Vec::new();
        for ms in 0..windows * 760 {
            st.packets_received += 1;
            st.bytes_received += WIRE;
            d.on_stats(&st);
            if ms % 16 == 0 {
                st.frames_completed += 1;
                d.on_au(false);
            }
            for a in d.tick(base + Duration::from_millis(ms)).actions {
                if let Action::Report {
                    packets_received, ..
                } = a
                {
                    out.push((ms, packets_received));
                }
            }
        }
        out
    }

    /// The governor's own input, from the client that has to produce it: every
    /// report window closes the host's share window ([`ShareWindow`]), so the
    /// path is re-read and the group re-divided every window.
    #[test]
    fn a_governing_host_is_told_what_arrived_every_window() {
        let told = delivery_reports(12);
        assert_eq!(told.len(), 12, "a window each: {told:?}");
        assert!(
            told.windows(2).all(|p| p[1].1 > p[0].1),
            "every count is this window's own: {told:?}"
        );
    }

    /// A group with no history behind it, which is how most cases below open.
    fn shares(members: &[Member], may_raise: bool) -> Vec<Option<u32>> {
        super::shares(
            members,
            0,
            Clocks {
                room: may_raise,
                lift: may_raise,
            },
        )
    }

    fn auto(current_kbps: u32, delivered_kbps: u32) -> Member {
        Member {
            automatic: true,
            current_kbps,
            offered_kbps: current_kbps,
            delivered_kbps: Some(delivered_kbps),
            idle: false,
            share_kbps: None,
            streaming: true,
        }
    }

    /// Two Automatic sessions asking a path for more than it carries split
    /// what it does carry, and a pair already sitting near that share is left
    /// alone.
    #[test]
    fn two_automatic_sessions_split_what_the_path_delivers() {
        let g = [auto(12_000, 9_000); 2];
        assert_eq!(shares(&g, true), [Some(9_000); 2], "half of 18 Mbps each");
        let held = [Member {
            share_kbps: Some(9_500),
            ..g[0]
        }; 2];
        assert_eq!(shares(&held, true), [None; 2], "inside the band");
    }

    /// The fixed session's rate comes off the top and it is never told
    /// anything; the Automatic one gets what is left, which is the cut that
    /// drains the queue it built.
    #[test]
    fn a_fixed_rate_session_is_never_touched_and_takes_its_rate_off_the_top() {
        let fixed = Member {
            automatic: false,
            ..auto(8_000, 8_000)
        };
        let out = shares(&[auto(14_000, 10_000), fixed], true);
        assert_eq!(out[1], None, "a fixed session never gets a governor ack");
        assert_eq!(
            out[0],
            Some(10_000),
            "18 Mbps delivered less the fixed 8 leaves the Automatic one 10"
        );
    }

    /// An idle sibling lends what it is not using, and has nothing to take
    /// back: lending decides what the other may have, not what the lender is
    /// allowed.
    #[test]
    fn an_idle_session_lends_its_share_and_takes_it_back() {
        let still = Member {
            idle: true,
            offered_kbps: 300,
            ..auto(9_000, 300)
        };
        let out = shares(&[auto(20_000, 17_700), still], true);
        assert!(
            out[0].is_some_and(|k| k > 17_000),
            "the active session takes nearly the whole path: {out:?}"
        );
        assert_eq!(out[1], Some(9_000), "the lender keeps an equal share");
        // Both want it: the path says how much there is and this halves it.
        assert_eq!(shares(&[auto(20_000, 9_000); 2], true), [Some(9_000); 2]);
    }

    /// A member held under its share by its own bounds lends the difference;
    /// one the path is refusing bytes to is starved, not lending, and keeps
    /// its whole share.
    #[test]
    fn a_self_bounded_member_lends_and_a_starved_one_does_not() {
        let out = shares(&[auto(4_000, 4_000), auto(18_000, 14_000)], true);
        assert_eq!(out[0], Some(9_000), "an equal share it is not using");
        assert!(
            out[1].is_some_and(|k| k > 13_000),
            "and the sibling gets more than half: {out:?}"
        );

        let out = shares(&[auto(12_000, 4_000), auto(18_000, 14_000)], true);
        assert_eq!(out[0], Some(9_000), "half of the 18 Mbps that arrived");
    }

    /// Delivering above its share proves the path had more: the budget grows
    /// with the evidence and the member is never cut for producing it.
    #[test]
    fn a_member_delivering_above_its_share_raises_the_budget() {
        let g = [
            Member {
                share_kbps: Some(9_000),
                ..auto(14_000, 14_000)
            },
            Member {
                share_kbps: Some(9_000),
                ..auto(9_000, 4_000)
            },
        ];
        let out = shares(&g, true);
        assert!(
            out[0].is_some_and(|k| k >= 14_000),
            "the clean member keeps what it delivered: {out:?}"
        );
    }

    /// A newcomer opens on the share the path has room for rather than the
    /// rate it negotiated blind, and its sibling comes down to what is
    /// reaching it instead of cascading.
    #[test]
    fn a_newcomer_gets_a_share_without_flooring_its_sibling() {
        let sibling = Member {
            share_kbps: Some(17_000),
            ..auto(17_000, 12_000)
        };
        let out = shares(&[sibling, auto(20_000, 6_000)], true);
        assert_eq!(out, [Some(9_000), Some(9_000)], "half the path each");
    }

    /// The band and the clock: a small move never goes out, a rise waits for
    /// the clock, a cut does not.
    #[test]
    fn a_share_moves_by_more_than_the_band_and_rises_only_on_the_clock() {
        assert_eq!(send(Some(10_000), 10_900, true), None, "inside the band");
        assert_eq!(send(Some(10_000), 12_000, true), Some(12_000));
        assert_eq!(send(Some(10_000), 12_000, false), None, "a rise waits");
        assert_eq!(send(Some(10_000), 8_000, false), Some(8_000), "a cut now");
        assert_eq!(send(None, 8_000, false), Some(8_000), "the first share");
    }

    /// One session is not a group, whatever it is holding: what a session left
    /// alone is told is its caller's to say, because only the caller knows it
    /// had a group at all.
    #[test]
    fn one_session_is_not_a_group() {
        assert_eq!(shares(&[auto(20_000, 9_000)], true), [None]);
        let survivor = Member {
            share_kbps: Some(9_000),
            ..auto(9_000, 9_000)
        };
        assert_eq!(
            super::shares(&[survivor], 18_000, Clocks::default()),
            [None]
        );
    }

    /// A path carrying everything it is offered has nothing to divide: two
    /// pipelines still coming up are left entirely alone, and so is a pair
    /// climbing a link neither of them has filled.
    #[test]
    fn a_path_that_is_carrying_the_group_is_left_alone() {
        let joining = Member {
            offered_kbps: 0,
            delivered_kbps: None,
            ..auto(20_000, 0)
        };
        let opening = Member {
            offered_kbps: 130,
            delivered_kbps: Some(128),
            ..auto(4_500, 0)
        };
        assert_eq!(shares(&[auto(17_000, 17_000), joining], true), [None; 2]);
        assert_eq!(shares(&[opening, opening], true), [None; 2]);
        assert_eq!(shares(&[auto(9_000, 9_000); 2], true), [None; 2]);
    }

    /// A member whose host never asked it for a count per window says nothing
    /// about the path, and its silence is not room for its sibling to lose: the
    /// group is left alone until every one of them has reported.
    #[test]
    fn a_member_that_has_not_reported_leaves_its_group_alone() {
        let quiet = Member {
            delivered_kbps: None,
            ..auto(12_000, 0)
        };
        let starved = auto(12_000, 6_000);
        assert_eq!(shares(&[starved, quiet], true), [None; 2]);
        assert_eq!(path_kbps(&[starved, quiet]), None);
        // Both of them reporting, and the same pair is divided.
        assert_eq!(
            shares(&[starved, auto(12_000, 6_000)], true),
            [Some(6_000); 2]
        );
    }

    /// The window a stream starts in is not a reading of the path: what left
    /// the host over it is still partly in flight when the client closes it, so
    /// the shortfall is the pipeline, not the link. One more window of the same
    /// rates and the pair means something.
    #[test]
    fn the_window_a_stream_starts_in_does_not_divide_the_path() {
        let opening = Member {
            streaming: false,
            ..auto(20_000, 9_000)
        };
        assert_eq!(shares(&[opening; 2], true), [None; 2], "nothing to divide");
        assert!(!crowded(&[opening; 2]));
        let running = Member {
            streaming: true,
            ..opening
        };
        assert_eq!(shares(&[running; 2], true), [Some(9_000); 2]);
    }

    /// A ceiling an earlier crowd taught cannot outlive it: while the path
    /// carries everything offered, the standing share breathes a band on the
    /// clock, and nothing happens between clocks (L1).
    #[test]
    fn a_standing_share_breathes_while_nobody_is_short() {
        let held = Member {
            share_kbps: Some(9_000),
            ..auto(9_000, 9_000)
        };
        assert_eq!(shares(&[held; 2], true), [Some(9_900); 2]);
        assert_eq!(shares(&[held; 2], false), [None; 2]);
    }
}
