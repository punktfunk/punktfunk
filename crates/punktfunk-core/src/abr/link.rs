//! The link rate `L` the host paces every frame at, as this client learns it: both ends'
//! ports before the first frame, then what the bring-up ramp measured, then the walls the
//! session finds. A measured wall beats the ports — a 1 GbE adapter on a 100 Mbit/s cable
//! walls at 100 — and the ports or the floor beat a ramp that stopped at what the stream
//! needed, until tail loss says a slower hop sits under them.

use super::probe::Ramped;
use crate::quic::LinkFacts;

/// `L` with no evidence at all: a fifth under 1 GbE.
pub(crate) const LINK_FLOOR_KBPS: u32 = 800_000;
/// A mark from tail loss takes this share off `L`…
pub(super) const NOTCH_DIV: u32 = 8;
/// …down to this many notches (about a third of it).
pub(super) const NOTCH_MAX: u8 = 8;
/// Windows without tail loss that give one notch back: half a minute.
pub(super) const NOTCH_CALM_WINDOWS: u32 = 40;

/// Where `L` came from, for the line a player reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSource {
    /// Both ends' Ethernet ports, the slower.
    Ports,
    /// The ramp, or the session's own walls.
    Measured,
    /// Nothing said anything: 800 Mbit/s.
    Floor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LinkRate {
    /// The slower of both Ethernet ports; `None` when either is Wi-Fi or silent.
    ports: Option<u32>,
    /// A wall the session measured; newest wins.
    wall: Option<u32>,
    /// The wall the bring-up ramp met, or what it proved once tail loss refuted the ports or
    /// the floor:
    /// where `L` goes back to when the session's wall goes.
    ramp: Option<u32>,
    /// What the ramp delivered without finding a wall: a floor under the link.
    proven: u32,
    floor: u32,
    /// Notches tail marks took off `L`, and the calm windows since the last.
    notches: u8,
    calm: u32,
}

impl Default for LinkRate {
    fn default() -> Self {
        LinkRate {
            ports: None,
            wall: None,
            ramp: None,
            proven: 0,
            floor: LINK_FLOOR_KBPS,
            notches: 0,
            calm: 0,
        }
    }
}

impl LinkRate {
    /// Both ends' ports. Wi-Fi sets nothing: a PHY rate is not a capacity.
    pub(crate) fn set_ports(&mut self, host: LinkFacts, client: LinkFacts) {
        let wired = |f: LinkFacts| {
            (f.kind == crate::transport::IFACE_KIND_ETHERNET && f.mbps > 0)
                .then(|| f.mbps.saturating_mul(1_000))
        };
        self.ports = wired(host).zip(wired(client)).map(|(h, c)| h.min(c));
    }

    /// What the bring-up ramp came to.
    pub(crate) fn ramped(&mut self, r: Ramped) {
        match r {
            Ramped::Wall { delivered_kbps } if delivered_kbps > 0 => {
                self.ramp = Some(delivered_kbps)
            }
            Ramped::Wall { .. } => {}
            Ramped::NoWall { proven_kbps } => self.proven = self.proven.max(proven_kbps),
        }
    }

    /// The session found a wall: a mark pair latched or re-latched it.
    pub(crate) fn wall(&mut self, kbps: u32) {
        if kbps > 0 {
            self.wall = Some(kbps);
        }
    }

    /// A settled lift carried the wall up to `kbps`.
    pub(crate) fn lifted(&mut self, kbps: u32) {
        self.wall = Some(self.wall.map_or(kbps, |w| w.max(kbps)));
    }

    /// The session's wall went: back to what the ramp measured, else what it proved and the
    /// ports say.
    pub(crate) fn dropped_cap(&mut self) {
        self.wall = None;
    }

    /// Tail loss two windows running with no frame lost: a queue on the path is filling.
    /// `L` on the ports or the floor alone retreats to what the ramp proved, else it comes
    /// down a notch before a frame is. The bitrate is the controller's.
    pub(crate) fn tail_mark(&mut self) {
        if !self.retreat() {
            self.notches = (self.notches + 1).min(NOTCH_MAX);
        }
        self.calm = 0;
    }

    /// Tail loss in a window where a frame died: `L` on the ports or the floor alone retreats
    /// to what the ramp proved, and the calm count starts over. The controller's cut and mark
    /// own the rate.
    pub(crate) fn tail_loss(&mut self) {
        self.retreat();
        self.calm = 0;
    }

    /// A port or the floor is a bound, not a capacity: with `L` on either alone, the rate the
    /// ramp proved takes the ramp wall's place for the session, so a dropped wall never
    /// returns to it. False when nothing was proven or `L` already rests on a measurement.
    fn retreat(&mut self) -> bool {
        let refuted = self.proven > 0
            && self.wall.or(self.ramp).is_none()
            && self.ports.unwrap_or(self.floor) > self.proven;
        if refuted {
            self.ramp = Some(self.proven);
        }
        refuted
    }

    /// A window without tail loss.
    pub(crate) fn calm_window(&mut self) {
        self.calm += 1;
        if self.calm >= NOTCH_CALM_WINDOWS && self.notches > 0 {
            self.notches -= 1;
            self.calm = 0;
        }
    }

    pub(crate) fn kbps(&self) -> u32 {
        let base = self
            .wall
            .or(self.ramp)
            .unwrap_or_else(|| self.proven.max(self.ports.unwrap_or(self.floor)));
        (0..self.notches).fold(base, |k, _| k - k / NOTCH_DIV)
    }

    /// Both ends' ports are known: a bound before anything is measured.
    pub(crate) fn wired(&self) -> bool {
        self.ports.is_some()
    }

    pub(crate) fn source(&self) -> LinkSource {
        match (self.wall.or(self.ramp), self.ports) {
            _ if self.notches > 0 => LinkSource::Measured,
            (Some(_), _) => LinkSource::Measured,
            (None, Some(p)) if p >= self.proven => LinkSource::Ports,
            (None, None) if self.proven <= self.floor => LinkSource::Floor,
            _ => LinkSource::Measured,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{IFACE_KIND_ETHERNET as ETH, IFACE_KIND_WIFI as WIFI};

    fn facts(kind: u8, mbps: u32) -> LinkFacts {
        LinkFacts { kind, mbps }
    }

    /// A 2.5 GbE host and a 1 GbE client are a 1 Gbit/s link before anything is measured;
    /// a stream-bound ramp does not lower it; a measured wall does, a settled lift raises
    /// it, and dropping it goes back to the ports.
    #[test]
    fn ports_then_walls_then_ports_again() {
        let mut l = LinkRate::default();
        assert_eq!((l.kbps(), l.source()), (LINK_FLOOR_KBPS, LinkSource::Floor));
        l.set_ports(facts(ETH, 2_500), facts(ETH, 1_000));
        assert!(l.wired());
        assert_eq!((l.kbps(), l.source()), (1_000_000, LinkSource::Ports));
        l.ramped(Ramped::NoWall {
            proven_kbps: 120_000,
        });
        assert_eq!(l.kbps(), 1_000_000);
        l.wall(640_000);
        assert_eq!((l.kbps(), l.source()), (640_000, LinkSource::Measured));
        l.lifted(720_000);
        assert_eq!(l.kbps(), 720_000);
        l.wall(560_000);
        assert_eq!(l.kbps(), 560_000, "the newest wall wins");
        l.dropped_cap();
        assert_eq!(l.kbps(), 1_000_000);
    }

    /// Each tail mark takes an eighth off `L`; half a minute of calm gives one back.
    #[test]
    fn tail_marks_notch_the_link_down_and_calm_gives_it_back() {
        let mut l = LinkRate::default();
        l.set_ports(facts(ETH, 1_000), facts(ETH, 1_000));
        l.tail_mark();
        assert_eq!((l.kbps(), l.source()), (875_000, LinkSource::Measured));
        l.tail_mark();
        assert_eq!(l.kbps(), 765_625);
        for _ in 0..39 {
            l.calm_window();
        }
        assert_eq!(l.kbps(), 765_625);
        l.calm_window();
        assert_eq!(l.kbps(), 875_000);
        for _ in 0..20 {
            l.tail_mark();
        }
        assert_eq!(l.kbps(), 343_611, "eight notches at most");
    }

    /// Tail loss under 2.5 GbE ports alone retreats `L` to what a stream-bound ramp proved:
    /// a port is a bound, not the link. The next mark notches that, calm gives it back, and
    /// the ports never return. With nothing proven a mark only notches.
    #[test]
    fn a_tail_mark_on_port_facts_alone_retreats_to_the_proven_rate() {
        let fresh = |proven_kbps| {
            let mut l = LinkRate::default();
            l.set_ports(facts(ETH, 2_500), facts(ETH, 2_500));
            l.ramped(Ramped::NoWall { proven_kbps });
            l
        };
        let mut l = fresh(0);
        l.tail_mark();
        assert_eq!(l.kbps(), 2_187_500, "nothing proven");
        let mut l = fresh(180_000);
        l.tail_loss();
        assert_eq!(l.kbps(), 180_000, "a dead frame's tail loss");
        let mut l = fresh(180_000);
        assert_eq!((l.kbps(), l.source()), (2_500_000, LinkSource::Ports));
        l.tail_mark();
        assert_eq!((l.kbps(), l.source()), (180_000, LinkSource::Measured));
        l.tail_mark();
        assert_eq!(l.kbps(), 157_500);
        for _ in 0..2 * NOTCH_CALM_WINDOWS {
            l.calm_window();
        }
        assert_eq!(l.kbps(), 180_000);
        l.wall(400_000);
        l.dropped_cap();
        assert_eq!(
            l.kbps(),
            180_000,
            "a dropped wall never returns to the ports"
        );
    }

    /// With no ports, the floor is as much a guess: tail loss retreats it the same way.
    #[test]
    fn a_tail_mark_on_the_floor_alone_retreats_to_the_proven_rate() {
        let mut l = LinkRate::default();
        l.ramped(Ramped::NoWall {
            proven_kbps: 180_000,
        });
        assert_eq!((l.kbps(), l.source()), (LINK_FLOOR_KBPS, LinkSource::Floor));
        l.tail_mark();
        assert_eq!((l.kbps(), l.source()), (180_000, LinkSource::Measured));
    }

    /// A ramp that met a wall measured the link already: a tail mark only notches it.
    #[test]
    fn a_tail_mark_on_a_ramp_wall_only_notches() {
        let mut l = LinkRate::default();
        l.set_ports(facts(ETH, 2_500), facts(ETH, 2_500));
        l.ramped(Ramped::NoWall {
            proven_kbps: 180_000,
        });
        l.ramped(Ramped::Wall {
            delivered_kbps: 640_000,
        });
        l.tail_mark();
        assert_eq!(l.kbps(), 560_000);
    }

    /// Wi-Fi at either end sets no port: the floor stands until the ramp proves more, and
    /// a ramp wall is the link.
    #[test]
    fn wifi_facts_set_nothing() {
        let mut l = LinkRate::default();
        l.set_ports(facts(ETH, 1_000), facts(WIFI, 1_200));
        assert_eq!((l.kbps(), l.source()), (800_000, LinkSource::Floor));
        l.ramped(Ramped::NoWall {
            proven_kbps: 900_000,
        });
        assert_eq!((l.kbps(), l.source()), (900_000, LinkSource::Measured));
        l.ramped(Ramped::Wall {
            delivered_kbps: 240_000,
        });
        assert_eq!(l.kbps(), 240_000);
    }

    /// A session wall that goes leaves the ramp's wall, not the floor: a 20 Mbit/s path the
    /// ramp met stays one when a re-probe lifts the session's cap.
    #[test]
    fn a_dropped_session_wall_falls_back_to_the_ramps() {
        let mut l = LinkRate::default();
        l.ramped(Ramped::Wall {
            delivered_kbps: 26_000,
        });
        l.wall(2_000);
        assert_eq!(l.kbps(), 2_000);
        l.dropped_cap();
        assert_eq!((l.kbps(), l.source()), (26_000, LinkSource::Measured));
    }
}
