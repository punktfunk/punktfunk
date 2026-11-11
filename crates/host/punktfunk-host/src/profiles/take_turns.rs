//! Take-turns profiles on Windows: a profile bound to a local Windows account that takes the one
//! console in turn (`design/windows-take-turns.md`). One at a time, so no Remote Desktop, no
//! Server, no concurrency — it is the box's single interactive console, switched between accounts
//! the way Windows' own *Switch user* does.
//!
//! The decision — play here, switch the console to this account, or tell the player to sign in
//! once — is a pure function of what Windows reports, so it is tested on every target. The
//! Windows-only shell ([`crate::windows::console`]) reads the sessions and runs the switch.
//! The account picker's rule lives here too: a seat on an existing account picks from the same
//! list.

/// What Windows reports about a take-turns profile's account, as the switch needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountState {
    /// The account still exists on this PC.
    pub exists: bool,
    /// Its session is the console right now, so it is already on screen.
    pub on_console: bool,
    /// It has a non-console local session (active or disconnected) to attach to the console,
    /// and this is its WTS id. `None` means it has not signed in since boot.
    pub switchable_session: Option<u32>,
}

impl AccountState {
    /// The absent account: every non-Windows host, and a Windows account that is gone.
    pub(crate) fn absent() -> AccountState {
        AccountState {
            exists: false,
            on_console: false,
            switchable_session: None,
        }
    }
}

/// What the console must do before a take-turns profile can play.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConsoleAction {
    /// The console is already this account; the connect plays here.
    Here,
    /// Attach this session to the console, then the service relaunches the host into it.
    Switch(u32),
    /// The account has no session to attach; it must sign in on the PC once (cold sign-in is
    /// not built — `design/windows-take-turns.md` §6).
    SignInFirst,
    /// The bound Windows account is gone.
    Gone,
}

/// The action for an account in this state. Pure: the Windows shell reads [`AccountState`], this
/// decides.
pub(crate) fn decide(state: &AccountState) -> ConsoleAction {
    if !state.exists {
        return ConsoleAction::Gone;
    }
    if state.on_console {
        return ConsoleAction::Here;
    }
    match state.switchable_session {
        Some(id) => ConsoleAction::Switch(id),
        None => ConsoleAction::SignInFirst,
    }
}

/// A take-turns profile's seat as a client reads it. `console_busy` is true when a stream is live
/// on the console as a *different* account: a switch would yank it, so the seat reads `occupied`
/// and the host refuses the switch rather than cut someone's game (the no-visible-glitch bar).
/// `switching` is a switch to this account in flight (the marker, within its timeout).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnSeat {
    /// On screen now: dial.
    Ready,
    /// A switch is under way: show the line and wait.
    Switching,
    /// Off screen, ready to switch in: the client's `wake` triggers it.
    Switchable,
    /// Signed in nowhere: tell the player to sign in once.
    SignIn,
    /// Someone else holds the console: don't switch under them.
    Occupied,
    /// The account is gone.
    Gone,
}

/// The seat state for a take-turns profile. `switching` short-circuits a `Switch` so the card
/// reads "Switching to …" through the host relaunch instead of inviting a second wake.
pub(crate) fn turn_seat(action: &ConsoleAction, console_busy: bool, switching: bool) -> TurnSeat {
    match action {
        ConsoleAction::Gone => TurnSeat::Gone,
        ConsoleAction::SignInFirst => TurnSeat::SignIn,
        ConsoleAction::Here => TurnSeat::Ready,
        ConsoleAction::Switch(_) if switching => TurnSeat::Switching,
        ConsoleAction::Switch(_) if console_busy => TurnSeat::Occupied,
        ConsoleAction::Switch(_) => TurnSeat::Switchable,
    }
}

/// The session a `wake` attaches to the console: only a seat that reads `Switchable` switches,
/// so a wake never cuts a live stream or repeats a switch in flight.
pub(crate) fn wake_switch(action: &ConsoleAction, seat: &TurnSeat) -> Option<u32> {
    match (action, seat) {
        (ConsoleAction::Switch(session), TurnSeat::Switchable) => Some(*session),
        _ => None,
    }
}

/// `account`'s action and seat from what Windows reports now. The card and the wake both read
/// this, so they can't disagree. Blocking (WTS).
pub(crate) fn seat_now(account: &str) -> (ConsoleAction, TurnSeat) {
    let action = decide(&account_state(account));
    // A stream on the console as another account: a switch would cut it.
    let console_busy = console_account().is_some() && crate::telemetry::session_status::count() > 0;
    let switching = switch_in_flight(
        switch_marker().as_ref().map(|(a, t)| (a.as_str(), *t)),
        account,
        crate::clock::unix_secs(),
    );
    let seat = turn_seat(&action, console_busy, switching);
    (action, seat)
}

/// How long a switch marker counts as in flight before a fresh `wake` may try again: the console
/// change plus the host relaunch. Past this, the marker is stale (the switch never took) and the
/// card goes back to `Switchable`.
pub(crate) const SWITCH_TIMEOUT_SECS: i64 = 60;

/// Whether a switch marker written at `marked_unix` for `marked_account` is still in flight for a
/// connect asking `asked_account`, at `now_unix`. A marker for another account, or one past
/// [`SWITCH_TIMEOUT_SECS`], is not.
pub(crate) fn switch_in_flight(
    marker: Option<(&str, i64)>,
    asked_account: &str,
    now_unix: i64,
) -> bool {
    match marker {
        Some((account, at)) => {
            account.eq_ignore_ascii_case(asked_account)
                && now_unix.saturating_sub(at) <= SWITCH_TIMEOUT_SECS
        }
        None => false,
    }
}

/// The box's current console account, lowercased by the caller when it compares. `None` off
/// Windows and at boot / the sign-in screen.
#[cfg(windows)]
pub(crate) fn console_account() -> Option<String> {
    super::windows_console_user()
}
#[cfg(not(windows))]
pub(crate) fn console_account() -> Option<String> {
    None
}

/// What Windows reports about `account`'s session right now. Absent everywhere else.
#[cfg(windows)]
pub(crate) fn account_state(account: &str) -> AccountState {
    crate::windows::console::account_state(account)
}
#[cfg(not(windows))]
pub(crate) fn account_state(_account: &str) -> AccountState {
    AccountState::absent()
}

/// Attach `account`'s `session_id` to the console and mark the switch in flight. The service's
/// console-change relaunch then brings the host up in that session. Windows only.
#[cfg(windows)]
pub(crate) fn switch_to(account: &str, session_id: u32) -> anyhow::Result<()> {
    crate::windows::console::switch_to(account, session_id)
}
#[cfg(not(windows))]
pub(crate) fn switch_to(_account: &str, _session_id: u32) -> anyhow::Result<()> {
    anyhow::bail!("take-turns profiles are Windows only")
}

/// The switch marker (account, written-unix) if one is set, else `None`.
#[cfg(windows)]
pub(crate) fn switch_marker() -> Option<(String, i64)> {
    crate::windows::console::read_switch_marker()
}
#[cfg(not(windows))]
pub(crate) fn switch_marker() -> Option<(String, i64)> {
    None
}

/// A local Windows account as the pickers list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalAccount {
    pub name: String,
    pub sid: String,
}

/// Whether a new profile may bind `account`, an enabled local account: one account, one profile.
/// A seats ledger row holds it by name, an adopted row by SID too, and a take-turns profile by
/// a name in `bound`. punktfunk's own seat accounts stay out when the ledger didn't load.
pub(crate) fn bindable(
    account: &LocalAccount,
    ledger: &[pf_seats::Seat],
    bound: &[String],
) -> bool {
    let name = account.name.to_lowercase();
    !is_seat_account(&name)
        && !bound.iter().any(|b| b.to_lowercase() == name)
        && !ledger.iter().any(|seat| {
            seat.account.to_lowercase() == name
                || seat.adopted_sid.as_deref() == Some(account.sid.as_str())
        })
}

/// punktfunk's own seat accounts by name, `pf_seat<n>` on Windows.
fn is_seat_account(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("pf_seat") || n.starts_with("pf-seat")
}

/// `Get-LocalUser` lines of `name<TAB>sid`; a line missing either is dropped.
pub(crate) fn parse_local_accounts(stdout: &str) -> Vec<LocalAccount> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, sid) = line.split_once('\t')?;
            let (name, sid) = (name.trim(), sid.trim());
            (!name.is_empty() && !sid.is_empty()).then(|| LocalAccount {
                name: name.to_string(),
                sid: sid.to_string(),
            })
        })
        .collect()
}

/// The enabled local Windows accounts. Empty off Windows.
#[cfg(windows)]
pub(crate) fn local_accounts() -> Vec<LocalAccount> {
    crate::windows::console::list_local_accounts()
}
#[cfg(not(windows))]
pub(crate) fn local_accounts() -> Vec<LocalAccount> {
    Vec::new()
}

/// The enabled local account named `name` in any case, as Windows spells it. Blocking.
pub(crate) fn local_account(name: &str) -> Option<LocalAccount> {
    let name = name.trim().to_lowercase();
    local_accounts()
        .into_iter()
        .find(|a| a.name.to_lowercase() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(exists: bool, on_console: bool, session: Option<u32>) -> AccountState {
        AccountState {
            exists,
            on_console,
            switchable_session: session,
        }
    }

    #[test]
    fn a_gone_account_is_gone_whatever_else() {
        assert_eq!(decide(&state(false, true, Some(3))), ConsoleAction::Gone);
        assert_eq!(decide(&AccountState::absent()), ConsoleAction::Gone);
    }

    #[test]
    fn on_the_console_plays_here() {
        assert_eq!(decide(&state(true, true, None)), ConsoleAction::Here);
    }

    #[test]
    fn a_signed_in_account_switches_a_cold_one_signs_in() {
        assert_eq!(
            decide(&state(true, false, Some(4))),
            ConsoleAction::Switch(4)
        );
        assert_eq!(
            decide(&state(true, false, None)),
            ConsoleAction::SignInFirst
        );
    }

    #[test]
    fn a_busy_console_holds_a_switchable_account_off() {
        let switch = ConsoleAction::Switch(4);
        assert_eq!(turn_seat(&switch, false, false), TurnSeat::Switchable);
        assert_eq!(turn_seat(&switch, true, false), TurnSeat::Occupied);
        // A switch in flight shows the wait even while the old console is still busy.
        assert_eq!(turn_seat(&switch, true, true), TurnSeat::Switching);
    }

    #[test]
    fn a_wake_switches_only_a_switchable_seat() {
        let switch = ConsoleAction::Switch(4);
        let wake = |busy, switching| wake_switch(&switch, &turn_seat(&switch, busy, switching));
        assert_eq!(wake(false, false), Some(4));
        assert_eq!(wake(true, false), None);
        assert_eq!(wake(false, true), None);
        let here = ConsoleAction::Here;
        assert_eq!(wake_switch(&here, &turn_seat(&here, false, false)), None);
    }

    #[test]
    fn here_and_signin_ignore_busy_and_switching() {
        assert_eq!(turn_seat(&ConsoleAction::Here, true, true), TurnSeat::Ready);
        assert_eq!(
            turn_seat(&ConsoleAction::SignInFirst, true, true),
            TurnSeat::SignIn
        );
    }

    #[test]
    fn a_switch_marker_is_in_flight_only_for_its_account_within_the_timeout() {
        assert!(switch_in_flight(Some(("alex", 100)), "ALEX", 130));
        assert!(switch_in_flight(Some(("alex", 100)), "alex", 160));
        assert!(!switch_in_flight(
            Some(("alex", 100)),
            "alex",
            100 + SWITCH_TIMEOUT_SECS + 1
        ));
        assert!(!switch_in_flight(Some(("joe", 100)), "alex", 110));
        assert!(!switch_in_flight(None, "alex", 110));
    }

    /// One account, one profile: a ledger row holds its account by name and an adopted row by
    /// SID as well, a take-turns profile by name, all in any case. `pf_seat` names never list.
    #[test]
    fn an_account_a_profile_or_seat_holds_is_not_bindable() {
        let parsed = parse_local_accounts(
            "Bärbel Ott\tS-1-5-21-1-2-3-1001\r\nBen\tS-1-5-21-1-2-3-1002\r\n\
             pf_seat1\tS-1-5-21-1-2-3-1003\r\nAlex\tS-1-5-21-1-2-3-1004\r\n\
             Noone\r\n\tS-1-5-21-1-2-3-1005\r\n\r\n",
        );
        let names: Vec<&str> = parsed.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Bärbel Ott", "Ben", "pf_seat1", "Alex"]);
        let row = |account: &str, sid: Option<&str>| -> pf_seats::Seat {
            serde_json::from_value(serde_json::json!({
                "id": "01".repeat(16), "name": "Kid", "account": account, "display_slot": 12,
                "native_port": 9778, "mgmt_port": 47991, "runtime": { "state": "stopped" },
                "adopted_sid": sid,
            }))
            .unwrap()
        };
        let free = |ledger: &[pf_seats::Seat], bound: &[String]| -> Vec<&str> {
            parsed
                .iter()
                .filter(|a| bindable(a, ledger, bound))
                .map(|a| a.name.as_str())
                .collect()
        };
        assert_eq!(free(&[], &[]), ["Bärbel Ott", "Ben", "Alex"]);
        // A renamed account keeps its SID, so its adopted row still holds it.
        let ledger = [
            row("BEN", None),
            row("Alexander", Some("S-1-5-21-1-2-3-1004")),
        ];
        assert_eq!(free(&ledger, &[]), ["Bärbel Ott"]);
        assert_eq!(free(&[], &["BÄRBEL OTT".into()]), ["Ben", "Alex"]);
    }
}
