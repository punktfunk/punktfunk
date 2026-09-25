//! Bottleneck link: a capacity trace, one FIFO byte queue with a depth, a base
//! one-way delay, and the loss processes.
//!
//! Fluid per tick: the queue drains `capacity ÷ 8` bytes per millisecond and
//! anything offered past the depth is tail-dropped. Loss is drawn per frame as
//! a shard count — independently per shard, bursts from a Gilbert-Elliott
//! chain — and placed in wire order, so a burst lands inside one FEC block the
//! way a real burst does.
//!
//! Independent per shard, not a share spread evenly: what costs a frame is the
//! binomial tail, the window where three shards of thirty go at once, and a
//! model that hands every frame the mean loses none of them.

use super::Rng;
use std::collections::VecDeque;

/// Foreign traffic's session id in the shared queue.
pub(super) const CROSS: u8 = 0xFF;

/// One link's physical parameters. Every field is a rate, a time or a ratio,
/// so a scenario reads as the link it describes.
#[derive(Clone, Debug)]
pub(super) struct LinkCfg {
    /// Capacity steps `(from_ms, kbps)`, ascending. The last one holds.
    pub capacity: Vec<(u64, u32)>,
    /// Capacity wanders ± this many percent, re-drawn every `wander_ms`.
    pub wander_pct: u32,
    pub wander_ms: u64,
    /// Queue depth, in milliseconds of the current capacity.
    pub buffer_ms: u64,
    pub base_delay_ms: u64,
    /// Independent shard loss.
    pub loss_ppm: u32,
    /// Gilbert-Elliott, stepped once per frame: entry and exit odds for the
    /// bad state, and the contiguous run it drops while there.
    pub burst_in_ppm: u32,
    pub burst_out_ppm: u32,
    pub burst_shards: u32,
    /// Airtime stall: capacity 0 for the last `stall_ms` of every
    /// `stall_every_ms`, so a session never opens inside one.
    pub stall_every_ms: u64,
    pub stall_ms: u64,
    /// One-off `(at_ms, for_ms)` stalls. The sender, not the path: a send
    /// loop that loses a scheduling slice emits its backlog as one burst,
    /// which is what the rig's Wi-Fi delay steps are and the only thing that
    /// can put tens of milliseconds into a queue with no loss and no host
    /// event behind it.
    pub hiccups: Vec<(u64, u64)>,
    /// Foreign CBR traffic into the same queue.
    pub cross_kbps: u32,
}

impl Default for LinkCfg {
    fn default() -> Self {
        LinkCfg {
            capacity: vec![(0, 1_000_000)],
            wander_pct: 0,
            wander_ms: 60_000,
            buffer_ms: 50,
            base_delay_ms: 2,
            loss_ppm: 0,
            burst_in_ppm: 0,
            burst_out_ppm: 500_000,
            burst_shards: 0,
            stall_every_ms: 0,
            stall_ms: 0,
            hiccups: Vec::new(),
            cross_kbps: 0,
        }
    }
}

/// Shards one frame loses: `random` spread over the whole frame, plus one
/// contiguous run of `burst_len` from `burst_at`.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LossDraw {
    pub random: u32,
    pub burst_at: u32,
    pub burst_len: u32,
}

struct Chunk {
    session: u8,
    frame: u32,
    bytes: u64,
}

pub(super) struct Link {
    cfg: LinkCfg,
    rng: Rng,
    queue: VecDeque<Chunk>,
    depth_bytes: u64,
    /// Sub-byte remainder of one tick's drain, kept so the long-run rate is
    /// the capacity and not a rounding of it.
    drain_carry: u64,
    cross_carry: u64,
    /// Signed percent offset held until `wander_until_ms`.
    wander_off: i64,
    wander_until_ms: u64,
    ge_bad: bool,
    /// The last [`Self::nominal_kbps`], so a read of the round trip draws nothing.
    nominal_now_kbps: u32,
}

impl Link {
    pub(super) fn new(cfg: LinkCfg, seed: u64) -> Self {
        Link {
            cfg,
            rng: Rng::new(seed),
            queue: VecDeque::new(),
            depth_bytes: 0,
            drain_carry: 0,
            cross_carry: 0,
            wander_off: 0,
            wander_until_ms: 0,
            ge_bad: false,
            nominal_now_kbps: 1,
        }
    }

    pub(super) fn base_delay_ms(&self) -> u64 {
        self.cfg.base_delay_ms
    }

    /// Capacity right now: the trace step, the wander offset, and zero inside
    /// an airtime stall.
    pub(super) fn capacity_kbps(&mut self, now_ms: u64) -> u32 {
        if self.cfg.stall_every_ms > 0
            && now_ms % self.cfg.stall_every_ms >= self.cfg.stall_every_ms - self.cfg.stall_ms
        {
            return 0;
        }
        if self
            .cfg
            .hiccups
            .iter()
            .any(|&(at, ms)| now_ms >= at && now_ms < at + ms)
        {
            return 0;
        }
        self.nominal_kbps(now_ms)
    }

    /// The rate the buffer was sized for. A stall empties the air, not the
    /// queue in front of it, so the depth is not zero while one lasts.
    fn nominal_kbps(&mut self, now_ms: u64) -> u32 {
        let mut base = self.cfg.capacity[0].1;
        for &(from, kbps) in &self.cfg.capacity {
            if now_ms >= from {
                base = kbps;
            }
        }
        if self.cfg.wander_pct > 0 {
            if now_ms >= self.wander_until_ms {
                let span = 2 * self.cfg.wander_pct as u64 + 1;
                self.wander_off = self.rng.below(span) as i64 - self.cfg.wander_pct as i64;
                self.wander_until_ms = now_ms + self.cfg.wander_ms;
            }
            base = (base as i64 * (100 + self.wander_off) / 100).max(1) as u32;
        }
        self.nominal_now_kbps = base;
        base
    }

    /// A NACK's round trip right now: the base delay each way plus the queue a
    /// resend waits behind. Reads state only, so a row that never asks is unmoved.
    pub(super) fn rtt_ms(&self) -> u64 {
        2 * self.cfg.base_delay_ms + self.depth_bytes * 8 / u64::from(self.nominal_now_kbps.max(1))
    }

    /// Offer bytes to the queue; the return is what the depth refused.
    pub(super) fn offer(&mut self, now_ms: u64, session: u8, frame: u32, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let cap = self.nominal_kbps(now_ms);
        let depth_cap = (cap as u64 * self.cfg.buffer_ms / 8).max(16_384);
        let room = depth_cap.saturating_sub(self.depth_bytes);
        let taken = bytes.min(room);
        if taken > 0 {
            self.depth_bytes += taken;
            match self.queue.back_mut() {
                Some(c) if c.session == session && c.frame == frame => c.bytes += taken,
                _ => self.queue.push_back(Chunk {
                    session,
                    frame,
                    bytes: taken,
                }),
            }
        }
        bytes - taken
    }

    /// One millisecond of drain. Delivered `(session, frame, bytes)` go to
    /// `out`; cross traffic is consumed here and never reported.
    pub(super) fn tick(&mut self, now_ms: u64, out: &mut Vec<(u8, u32, u64)>) {
        if self.cfg.cross_kbps > 0 {
            self.cross_carry += self.cfg.cross_kbps as u64;
            let bytes = self.cross_carry / 8;
            self.cross_carry %= 8;
            self.offer(now_ms, CROSS, 0, bytes);
        }
        if self.queue.is_empty() {
            self.drain_carry = 0;
            return;
        }
        let cap = self.capacity_kbps(now_ms);
        self.drain_carry += cap as u64;
        let mut budget = self.drain_carry / 8;
        self.drain_carry %= 8;
        while budget > 0 {
            let Some(head) = self.queue.front_mut() else {
                break;
            };
            let take = budget.min(head.bytes);
            head.bytes -= take;
            budget -= take;
            self.depth_bytes -= take;
            let (session, frame) = (head.session, head.frame);
            if head.bytes == 0 {
                self.queue.pop_front();
            }
            if session != CROSS {
                match out.last_mut() {
                    Some(last) if last.0 == session && last.1 == frame => last.2 += take,
                    _ => out.push((session, frame, take)),
                }
            }
        }
    }

    /// Loss for one frame of `shards` wire shards: a trial per shard, plus the
    /// burst chain, which steps once per frame so a bad state spans a few
    /// frames at any frame rate.
    pub(super) fn draw_loss(&mut self, shards: u32) -> LossDraw {
        if shards == 0 {
            return LossDraw::default();
        }
        // A link with no loss process draws nothing at all: it must cost no
        // generator state, or every clean scenario moves with this model.
        let random = if self.cfg.loss_ppm == 0 {
            0
        } else {
            (0..shards)
                .filter(|_| self.rng.chance_ppm(self.cfg.loss_ppm))
                .count() as u32
        };
        self.ge_bad = if self.ge_bad {
            !self.rng.chance_ppm(self.cfg.burst_out_ppm)
        } else {
            self.rng.chance_ppm(self.cfg.burst_in_ppm)
        };
        let (burst_at, burst_len) = if self.ge_bad && self.cfg.burst_shards > 0 {
            (
                self.rng.below(shards as u64) as u32,
                self.cfg.burst_shards.min(shards),
            )
        } else {
            (0, 0)
        };
        LossDraw {
            random: random.min(shards),
            burst_at,
            burst_len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue drains at the capacity and no faster, and what it holds past
    /// its depth is refused rather than queued forever.
    #[test]
    fn the_queue_drains_at_the_capacity_and_refuses_what_it_cannot_hold() {
        let mut link = Link::new(
            LinkCfg {
                capacity: vec![(0, 8_000)],
                buffer_ms: 100,
                ..LinkCfg::default()
            },
            1,
        );
        // 8 Mbps = 1000 bytes/ms; a 100 ms buffer holds 100 000.
        assert_eq!(link.offer(0, 0, 1, 100_000), 0);
        assert_eq!(link.offer(0, 0, 1, 5_000), 5_000, "past the depth, refused");
        let mut out = Vec::new();
        for t in 0..10 {
            link.tick(t, &mut out);
        }
        let delivered: u64 = out.iter().map(|d| d.2).sum();
        assert_eq!(delivered, 10_000, "10 ms at 8 Mbps is 10 000 bytes");
    }

    /// A stall carries nothing and the backlog leaves afterwards.
    #[test]
    fn an_airtime_stall_carries_nothing_while_it_lasts() {
        let mut link = Link::new(
            LinkCfg {
                capacity: vec![(0, 8_000)],
                stall_every_ms: 1_000,
                stall_ms: 100,
                ..LinkCfg::default()
            },
            2,
        );
        link.offer(0, 0, 1, 50_000);
        let mut out = Vec::new();
        for t in 900..1_000 {
            link.tick(t, &mut out);
        }
        assert!(out.is_empty(), "the last 100 ms of every second is a stall");
        for t in 1_000..1_050 {
            link.tick(t, &mut out);
        }
        assert_eq!(out.iter().map(|d| d.2).sum::<u64>(), 50_000);
    }

    /// Independent loss spends its ppm over a run, and puts more than one
    /// shard into some of the frames — the draw a frame of two parity shards
    /// actually dies to. A link with no loss process drops nothing.
    #[test]
    fn independent_loss_spends_its_ppm_and_clusters() {
        let mut link = Link::new(
            LinkCfg {
                loss_ppm: 10_000,
                ..LinkCfg::default()
            },
            3,
        );
        let per_frame: Vec<u32> = (0..1_000).map(|_| link.draw_loss(100).random).collect();
        let lost: u32 = per_frame.iter().sum();
        assert!(
            (950..=1_050).contains(&lost),
            "1 % of 100 000 shards: {lost}"
        );
        let over_two = per_frame.iter().filter(|&&n| n > 2).count();
        assert!(over_two > 10, "a 2-shard parity pool dies {over_two} times");
        let mut clean = Link::new(LinkCfg::default(), 4);
        assert_eq!(clean.draw_loss(100).random, 0);
        assert_eq!(clean.draw_loss(100).burst_len, 0);
    }

    /// Wander stays inside its band and re-draws on its own clock.
    #[test]
    fn capacity_wander_stays_inside_its_band() {
        let mut link = Link::new(
            LinkCfg {
                capacity: vec![(0, 12_000)],
                wander_pct: 30,
                wander_ms: 100,
                ..LinkCfg::default()
            },
            5,
        );
        for t in 0..5_000 {
            let c = link.capacity_kbps(t);
            assert!((8_400..=15_600).contains(&c), "{c} kbps is outside ±30 %");
        }
    }
}
