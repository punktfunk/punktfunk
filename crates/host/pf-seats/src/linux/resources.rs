//! What must be up while a seat's unit runs, and what each change of the unit does to it.
//!
//! A seat's unit needs a few things around it: its copied files, the controllers lent to it,
//! its `/dev/uinput` stand-in, the pad broker serving its user and, on a person's seat, the
//! list of devices that are present. Each is a [`Resource`].
//!
//! [`action`] is the whole rule: one answer for each kind of account, each resource and each
//! [`UnitChange`]. Every match is exhaustive, so a new resource or change fails to compile until
//! someone decides it. [`actions`] lists what one change asks for, in the order it runs.
//! `Inner::apply_change` in `mod.rs` performs that list, and nothing else there starts or stops
//! one of these resources.
//!
//! Pure. The test `the_action_table` pins every cell.

use super::SeatAccount;

/// Something that must be up while a seat's unit runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resource {
    /// The seat's trust copy and library copy. For the owner's row also its environment file
    /// and the box's library copy.
    Copies,
    /// The desk pads lent to the seat.
    LentPads,
    /// A made seat's `/dev/uinput` stand-in, which the daemon binary serves.
    UinputProxy,
    /// The pad broker serving the seat's user.
    PadBroker,
    /// The devices present on a person's seat.
    Presence,
}

/// When a resource starts and stops, relative to the unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Order {
    /// Started before the unit starts, stopped after the unit stops.
    BeforeUnit,
    /// Started once the unit is up, stopped before the unit stops.
    AfterUnit,
}

/// A change of a seat's unit, as the backend caused it or found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnitChange {
    /// The owner's row was adopted, or was found when the supervisor started.
    OwnerKnown,
    /// A start is bringing the unit up.
    Starting,
    /// The unit was found up or starting, and the pad broker doesn't serve its user yet.
    FoundUp,
    /// The unit's state was read: after a stop, and each time the service looks at the seat.
    Observed { running: bool },
    /// The unit stopped, or was found down.
    Stopped,
    /// A delete stopped the unit.
    Deleting,
    /// A delete removed the seat's files.
    Deleted,
}

/// What a change does to one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Start,
    /// Gives the lent pads to the seat (`true`) or parks them.
    SetPadsLive(bool),
    Stop,
    Nothing,
}

/// The resources a seat on `account` has, in the order they start.
pub(super) fn resources(account: SeatAccount) -> &'static [Resource] {
    use Resource::{Copies, LentPads, PadBroker, Presence, UinputProxy};
    match account {
        SeatAccount::Made => &[Copies, LentPads, UinputProxy, PadBroker],
        SeatAccount::Owner => &[Copies, PadBroker, LentPads],
        SeatAccount::Person => &[Copies, LentPads, Presence, PadBroker],
    }
}

/// When `resource` starts relative to the unit. The pad broker serves the owner's user for as
/// long as the owner's row exists, whether its unit runs or not.
pub(super) fn order_of(account: SeatAccount, resource: Resource) -> Order {
    match resource {
        Resource::Copies | Resource::LentPads | Resource::UinputProxy => Order::BeforeUnit,
        Resource::PadBroker => match account {
            SeatAccount::Owner => Order::BeforeUnit,
            SeatAccount::Made | SeatAccount::Person => Order::AfterUnit,
        },
        Resource::Presence => Order::AfterUnit,
    }
}

/// What `change` does to `resource` on a seat of `account`.
pub(super) fn action(account: SeatAccount, resource: Resource, change: UnitChange) -> Action {
    use SeatAccount::{Made, Owner, Person};
    use UnitChange::{Deleted, Deleting, FoundUp, Observed, OwnerKnown, Starting, Stopped};
    match resource {
        Resource::Copies => match (account, change) {
            (Made | Owner | Person, Starting) | (Owner, OwnerKnown) => Action::Start,
            (Made | Person, OwnerKnown)
            | (Made | Owner | Person, FoundUp | Observed { .. } | Stopped | Deleting | Deleted) => {
                Action::Nothing
            }
        },
        Resource::LentPads => match (account, change) {
            (Made | Owner | Person, Starting) => Action::SetPadsLive(true),
            (Made | Owner | Person, Observed { running }) => Action::SetPadsLive(running),
            (Made | Owner | Person, Deleted) => Action::Stop,
            (Made | Owner | Person, OwnerKnown | FoundUp | Stopped | Deleting) => Action::Nothing,
        },
        Resource::UinputProxy => match (account, change) {
            (Made, Starting | FoundUp) => Action::Start,
            (Made, Stopped | Deleting) => Action::Stop,
            (Made, OwnerKnown | Observed { .. } | Deleted)
            | (Owner | Person, OwnerKnown | Starting | FoundUp | Observed { .. })
            | (Owner | Person, Stopped | Deleting | Deleted) => Action::Nothing,
        },
        Resource::PadBroker => match (account, change) {
            (Owner, OwnerKnown | Starting) | (Made | Person, Starting | FoundUp) => Action::Start,
            (Owner, Deleting) | (Made | Person, Stopped | Deleting) => Action::Stop,
            (Owner, FoundUp | Observed { .. } | Stopped | Deleted)
            | (Made | Person, OwnerKnown | Observed { .. } | Deleted) => Action::Nothing,
        },
        Resource::Presence => match (account, change) {
            (Person, Stopped | Deleting) => Action::Stop,
            (Person, OwnerKnown | Starting | FoundUp | Observed { .. } | Deleted)
            | (Made | Owner, OwnerKnown | Starting | FoundUp | Observed { .. })
            | (Made | Owner, Stopped | Deleting | Deleted) => Action::Nothing,
        },
    }
}

/// What `change` asks of the resources of one `order`, on a seat of `account`, in the order it
/// runs: starts in [`resources`] order, stops in reverse.
pub(super) fn actions(
    account: SeatAccount,
    order: Order,
    change: UnitChange,
) -> Vec<(Resource, Action)> {
    let mut acts: Vec<_> = (resources(account).iter().copied())
        .filter(|&it| order_of(account, it) == order)
        .map(|it| (it, action(account, it, change)))
        .filter(|&(_, action)| action != Action::Nothing)
        .collect();
    if matches!(
        change,
        UnitChange::Stopped | UnitChange::Deleting | UnitChange::Deleted
    ) {
        acts.reverse();
    }
    acts
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{Nothing as N, SetPadsLive, Start as S, Stop as X};

    const CHANGES: [UnitChange; 8] = [
        UnitChange::OwnerKnown,
        UnitChange::Starting,
        UnitChange::FoundUp,
        UnitChange::Observed { running: true },
        UnitChange::Observed { running: false },
        UnitChange::Stopped,
        UnitChange::Deleting,
        UnitChange::Deleted,
    ];

    /// Every kind of account × resource × change. Each row names the fix it holds, by commit.
    /// Columns: OwnerKnown, Starting, FoundUp, Observed running, Observed not running, Stopped,
    /// Deleting, Deleted. `S` starts, `X` stops, `N` does nothing.
    #[test]
    fn the_action_table() {
        use Resource::{Copies, LentPads, PadBroker, Presence, UinputProxy};
        use SeatAccount::{Made, Owner, Person};
        let (on, off) = (SetPadsLive(true), SetPadsLive(false));
        #[rustfmt::skip]
        let table: [(SeatAccount, Resource, [Action; 8], &str); 15] = [
            (Made, Copies, [N, S, N, N, N, N, N, N],
                "written under the copy lock at a start (1d1586d0b5); \
                 a deleted seat's copies stay deleted (5158358f8d)"),
            (Owner, Copies, [S, S, N, N, N, N, N, N],
                "as a made seat's copies (1d1586d0b5, 5158358f8d); \
                 written again when the supervisor starts (ee925a83cd)"),
            (Person, Copies, [N, S, N, N, N, N, N, N],
                "as a made seat's copies (1d1586d0b5, 5158358f8d)"),
            (Made, LentPads, [N, on, N, on, off, N, N, X],
                "the seat gets its pads before its unit starts, and they are parked once it stops \
                 (28ced4f24); a failed start's pads are parked when the unit is next read \
                 (f83dfc7b90)"),
            (Owner, LentPads, [N, on, N, on, off, N, N, X],
                "as a made seat's pads (28ced4f24, f83dfc7b90)"),
            (Person, LentPads, [N, on, N, on, off, N, N, X],
                "as a made seat's pads (28ced4f24, f83dfc7b90)"),
            (Made, UinputProxy, [N, S, S, N, N, X, X, N],
                "a stop takes the seat's uinput devices back (f8edca511a); \
                 a failed start's proxy stops when the unit is found down (f83dfc7b90)"),
            (Owner, UinputProxy, [N; 8], "only a made seat has a uinput proxy"),
            (Person, UinputProxy, [N; 8],
                "only a made seat has a uinput proxy: a person's seat gets its pads from the pad \
                 broker"),
            (Made, PadBroker, [N, S, S, N, N, X, X, N],
                "the pad broker refuses the user of a stopped seat (f8edca511a)"),
            (Owner, PadBroker, [S, S, N, N, N, N, X, N],
                "served from adoption on, and again after a supervisor restart (ee925a83cd); \
                 still served across a stop (f8edca511a)"),
            (Person, PadBroker, [N, S, S, N, N, X, X, N],
                "as a made seat's entry (f8edca511a)"),
            (Made, Presence, [N; 8], "only a person's seat has present devices"),
            (Owner, Presence, [N; 8], "only a person's seat has present devices"),
            (Person, Presence, [N, N, N, N, N, X, X, N],
                "when the session ends, every device's presence ends"),
        ];
        let mut seen = Vec::new();
        for (account, it, acts, why) in table {
            assert!(!seen.contains(&(account, it)), "{account:?} {it:?} twice");
            seen.push((account, it));
            for (change, want) in CHANGES.into_iter().zip(acts) {
                let got = action(account, it, change);
                assert_eq!(got, want, "{account:?} {it:?} {change:?}: {why}");
                // Each pair below has an arm in `Inner::apply_change`.
                let performed = matches!(
                    (it, want),
                    (_, N)
                        | (Copies, S)
                        | (LentPads, SetPadsLive(_) | X)
                        | (UinputProxy | PadBroker, S | X)
                        | (Presence, X)
                );
                assert!(
                    performed,
                    "{account:?} {it:?} {change:?}: nothing performs {want:?}"
                );
            }
        }
    }

    /// The order of each path. The pad broker serves the owner's user before the owner's pads go
    /// to it, and a made seat's or a person's user once the unit started. When a person's seat
    /// stops, the broker stops serving first, then presence ends. A made seat's proxy stops
    /// before its pads are parked.
    #[test]
    fn actions_run_in_each_path_s_order() {
        use Order::{AfterUnit, BeforeUnit};
        use Resource::{Copies, LentPads, PadBroker, Presence, UinputProxy};
        use SeatAccount::{Made, Owner, Person};
        use UnitChange::{Deleted, Deleting, OwnerKnown, Starting, Stopped};
        let starting = [Copies, LentPads, UinputProxy].map(|it| (it, action(Made, it, Starting)));
        assert_eq!(actions(Made, BeforeUnit, Starting), starting);
        assert_eq!(actions(Made, AfterUnit, Starting), [(PadBroker, S)]);
        let owner = [(Copies, S), (PadBroker, S), (LentPads, SetPadsLive(true))];
        assert_eq!(actions(Owner, BeforeUnit, Starting), owner);
        assert_eq!(actions(Owner, AfterUnit, Starting), []);
        assert_eq!(
            actions(Owner, BeforeUnit, OwnerKnown),
            [(Copies, S), (PadBroker, S)]
        );
        assert_eq!(
            actions(Person, BeforeUnit, Starting),
            [(Copies, S), (LentPads, SetPadsLive(true))]
        );
        assert_eq!(actions(Person, AfterUnit, Starting), [(PadBroker, S)]);
        let ended = [(PadBroker, X), (Presence, X)];
        assert_eq!(actions(Person, AfterUnit, Stopped), ended);
        assert_eq!(actions(Made, BeforeUnit, Deleting), [(UinputProxy, X)]);
        assert_eq!(actions(Owner, BeforeUnit, Stopped), []);
        assert_eq!(actions(Owner, BeforeUnit, Deleting), [(PadBroker, X)]);
        assert_eq!(actions(Made, BeforeUnit, Deleted), [(LentPads, X)]);
    }
}
