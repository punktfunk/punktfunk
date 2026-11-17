//! What runs beside a row's unit, and what each change of the unit does to it.
//!
//! [`act`] is the whole rule: one answer per row kind, resource and edge, each an exhaustive
//! match, so a new resource or edge fails to compile until someone decides it. [`crossing`]
//! lists one side's acts in the order they run. The backend's crossing performs them, and
//! nothing else in it raises or ends a beside resource. Pure: `beside_acts_table` pins every
//! cell. Design: `seats-overhaul.md` §4.4.

use super::Own;

/// What runs beside a row's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Beside {
    /// The row's trust and library copies; the owner's environment file and the box's library.
    Copies,
    /// The desk pads lent to the row.
    Lent,
    /// The binary's uinput proxy, a made seat's `/dev/uinput`.
    Proxy,
    /// The pad broker's admission of the row's user.
    Broker,
    /// The devices present on a person's row.
    Presence,
}

/// The side of the unit a resource lives on: an outer one rises before the unit starts and
/// falls after it stops, an inner one rises once the unit started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ring {
    Outer,
    Inner,
}

/// A change of a row's unit, as the backend caused or found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    /// The owner's row adopted, or found at the supervisor's start.
    Prepared,
    /// A start brings the unit up.
    Starting,
    /// A unit found up or starting whose user the broker doesn't admit.
    Found,
    /// The pads follow whether the row runs: after a stop, and at every reconcile.
    Level { live: bool },
    /// The unit stopped, or was found down.
    Stopped,
    /// A removal stopped the unit.
    Released,
    /// A removal's files are gone.
    Gone,
}

/// What an edge does to one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Act {
    Up,
    /// The lent pads live (`true`) or parked.
    Live(bool),
    Down,
    Keep,
}

/// The resources a `kind` row has, in the order they rise.
pub(super) fn beside(kind: Own) -> &'static [Beside] {
    match kind {
        Own::Seat => &[Beside::Copies, Beside::Lent, Beside::Proxy, Beside::Broker],
        Own::Owner => &[Beside::Copies, Beside::Broker, Beside::Lent],
        Own::Person => &[
            Beside::Copies,
            Beside::Lent,
            Beside::Presence,
            Beside::Broker,
        ],
    }
}

/// The side of the unit `it` lives on. The owner's admission lives with the row, not the unit.
pub(super) fn side(kind: Own, it: Beside) -> Ring {
    match it {
        Beside::Copies | Beside::Lent | Beside::Proxy => Ring::Outer,
        Beside::Broker => match kind {
            Own::Owner => Ring::Outer,
            Own::Seat | Own::Person => Ring::Inner,
        },
        Beside::Presence => Ring::Inner,
    }
}

/// What `edge` does to `it` on a `kind` row.
pub(super) fn act(kind: Own, it: Beside, edge: Edge) -> Act {
    use Edge::{Found, Gone, Level, Prepared, Released, Starting, Stopped};
    use Own::{Owner, Person, Seat};
    match it {
        Beside::Copies => match (kind, edge) {
            (Seat | Owner | Person, Starting) | (Owner, Prepared) => Act::Up,
            (Seat | Person, Prepared)
            | (Seat | Owner | Person, Found | Level { .. } | Stopped | Released | Gone) => {
                Act::Keep
            }
        },
        Beside::Lent => match (kind, edge) {
            (Seat | Owner | Person, Starting) => Act::Live(true),
            (Seat | Owner | Person, Level { live }) => Act::Live(live),
            (Seat | Owner | Person, Gone) => Act::Down,
            (Seat | Owner | Person, Prepared | Found | Stopped | Released) => Act::Keep,
        },
        Beside::Proxy => match (kind, edge) {
            (Seat, Starting | Found) => Act::Up,
            (Seat, Stopped | Released) => Act::Down,
            (Seat, Prepared | Level { .. } | Gone)
            | (Owner | Person, Prepared | Starting | Found | Level { .. })
            | (Owner | Person, Stopped | Released | Gone) => Act::Keep,
        },
        Beside::Broker => match (kind, edge) {
            (Owner, Prepared | Starting) | (Seat | Person, Starting | Found) => Act::Up,
            (Owner, Released) | (Seat | Person, Stopped | Released) => Act::Down,
            (Owner, Found | Level { .. } | Stopped | Gone)
            | (Seat | Person, Prepared | Level { .. } | Gone) => Act::Keep,
        },
        Beside::Presence => match (kind, edge) {
            (Person, Stopped | Released) => Act::Down,
            (Person, Prepared | Starting | Found | Level { .. } | Gone)
            | (Seat | Owner, Prepared | Starting | Found | Level { .. })
            | (Seat | Owner, Stopped | Released | Gone) => Act::Keep,
        },
    }
}

/// What `edge` asks of the `ring` side of a `kind` row's unit, in the order it runs: rising in
/// [`beside`] order, falling in reverse.
pub(super) fn crossing(kind: Own, ring: Ring, edge: Edge) -> Vec<(Beside, Act)> {
    let mut acts: Vec<_> = (beside(kind).iter().copied())
        .filter(|&it| side(kind, it) == ring)
        .map(|it| (it, act(kind, it, edge)))
        .filter(|&(_, act)| act != Act::Keep)
        .collect();
    if matches!(edge, Edge::Stopped | Edge::Released | Edge::Gone) {
        acts.reverse();
    }
    acts
}

#[cfg(test)]
mod tests {
    use super::*;
    use Act::{Down as D, Keep as K, Live, Up as U};

    const EDGES: [Edge; 8] = [
        Edge::Prepared,
        Edge::Starting,
        Edge::Found,
        Edge::Level { live: true },
        Edge::Level { live: false },
        Edge::Stopped,
        Edge::Released,
        Edge::Gone,
    ];

    /// Every row kind × resource × edge, each row naming the fix it holds. Columns: Prepared,
    /// Starting, Found, Level(live), Level(parked), Stopped, Released, Gone.
    #[test]
    fn beside_acts_table() {
        let (on, off) = (Live(true), Live(false));
        #[rustfmt::skip]
        let table: [(Own, Beside, [Act; 8], &str); 15] = [
            (Own::Seat, Beside::Copies, [K, U, K, K, K, K, K, K],
                "1d1586d0b5: written under the level at a start; 5158358f8d: a removal's stay gone"),
            (Own::Owner, Beside::Copies, [U, U, K, K, K, K, K, K],
                "1d1586d0b5, 5158358f8d; ee925a83cd: written again at the supervisor's start"),
            (Own::Person, Beside::Copies, [K, U, K, K, K, K, K, K], "1d1586d0b5, 5158358f8d"),
            (Own::Seat, Beside::Lent, [K, on, K, on, off, K, K, D],
                "28ced4f24: live before the unit, parked once it stops; f83dfc7b90: a failed start's by reconcile"),
            (Own::Owner, Beside::Lent, [K, on, K, on, off, K, K, D], "28ced4f24, f83dfc7b90"),
            (Own::Person, Beside::Lent, [K, on, K, on, off, K, K, D], "28ced4f24, f83dfc7b90"),
            (Own::Seat, Beside::Proxy, [K, U, U, K, K, D, D, K],
                "f8edca511a: a stop takes the seat's pads back; f83dfc7b90: a failed start's by reconcile"),
            (Own::Owner, Beside::Proxy, [K; 8], "a made seat's alone"),
            (Own::Person, Beside::Proxy, [K; 8], "a made seat's alone: a person's pads come from the broker"),
            (Own::Seat, Beside::Broker, [K, U, U, K, K, D, D, K],
                "f8edca511a: the broker refuses a stopped seat's user"),
            (Own::Owner, Beside::Broker, [U, U, K, K, K, K, D, K],
                "ee925a83cd: tracked from adoption and after a restart; f8edca511a: kept across a stop"),
            (Own::Person, Beside::Broker, [K, U, U, K, K, D, D, K], "f8edca511a"),
            (Own::Seat, Beside::Presence, [K; 8], "a person's row's alone"),
            (Own::Owner, Beside::Presence, [K; 8], "a person's row's alone"),
            (Own::Person, Beside::Presence, [K, K, K, K, K, D, D, K],
                "every end of the session ends every presence"),
        ];
        let mut seen = Vec::new();
        for (kind, it, acts, why) in table {
            assert!(!seen.contains(&(kind, it)), "{kind:?} {it:?} twice");
            seen.push((kind, it));
            for (edge, want) in EDGES.into_iter().zip(acts) {
                assert_eq!(act(kind, it, edge), want, "{kind:?} {it:?} {edge:?}: {why}");
                let performed = matches!(
                    (it, want),
                    (_, K)
                        | (Beside::Copies, U)
                        | (Beside::Lent, Live(_) | D)
                        | (Beside::Proxy | Beside::Broker, U | D)
                        | (Beside::Presence, D)
                );
                assert!(
                    performed,
                    "{kind:?} {it:?} {edge:?}: no crossing performs {want:?}"
                );
            }
        }
    }

    /// The order each path runs in: the owner is tracked before its pads go live, a made
    /// seat and a person once the unit started; a person's stop untracks before it ends presence;
    /// a made seat's proxy goes before its pads park.
    #[test]
    fn a_crossing_runs_in_each_path_s_order() {
        use {Beside::*, Edge::*, Ring::*};
        let starting = [Copies, Lent, Proxy].map(|it| (it, act(Own::Seat, it, Starting)));
        assert_eq!(crossing(Own::Seat, Outer, Starting), starting);
        assert_eq!(crossing(Own::Seat, Inner, Starting), [(Broker, U)]);
        let owner = [(Copies, U), (Broker, U), (Lent, Live(true))];
        assert_eq!(crossing(Own::Owner, Outer, Starting), owner);
        assert_eq!(crossing(Own::Owner, Inner, Starting), []);
        assert_eq!(
            crossing(Own::Owner, Outer, Prepared),
            [(Copies, U), (Broker, U)]
        );
        assert_eq!(
            crossing(Own::Person, Outer, Starting),
            [(Copies, U), (Lent, Live(true))]
        );
        assert_eq!(crossing(Own::Person, Inner, Starting), [(Broker, U)]);
        let ended = [(Broker, D), (Presence, D)];
        assert_eq!(crossing(Own::Person, Inner, Stopped), ended);
        assert_eq!(crossing(Own::Seat, Outer, Released), [(Proxy, D)]);
        assert_eq!(crossing(Own::Owner, Outer, Stopped), []);
        assert_eq!(crossing(Own::Owner, Outer, Released), [(Broker, D)]);
        assert_eq!(crossing(Own::Seat, Outer, Gone), [(Lent, D)]);
    }
}
