//! Turning seats on and off on Windows as a pure plan.
//!
//! [`Recorded`] is what the Seats key holds; [`on`] and [`off`] turn it and the operator's
//! choice into the ordered [`Step`]s `enable.rs` performs, one function per step. No I/O and no
//! OS types, so the tables run on every target. The order is the contract: the firewall has its
//! final scope before Remote Desktop listens, the listener goes off before the firewall widens,
//! each change is noted before it is made, and the key goes last, so a failed step leaves
//! enough behind for the next turn-off to undo.

// Off Windows only the tests read this module.
#![cfg_attr(not(windows), allow(dead_code))]

/// What the Seats key records of the changes a turn-on made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recorded {
    /// `Enabling`: a turn-on that has not finished.
    pub enabling: bool,
    /// `ListenerWasOff` is 1: Remote Desktop didn't listen before seats.
    pub listener_was_off: bool,
    /// `AllowRdpFromNetwork`, the operator's last choice: 1 lets the network in.
    pub choice: Option<u32>,
    /// `RdpRulesOpened` names a rule seats enabled.
    pub rules_opened: bool,
    /// `RdpRuleScopes` holds each rule's scope from before seats.
    pub scopes_noted: bool,
    /// `RdpRulesNarrowed`: a turn-on narrowed the rules to this PC.
    pub narrowed_noted: bool,
}

/// One change of a turn-on or turn-off, each performed by one function of `enable.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Mark a new key `Enabling`, so it reads as off until [`Step::Finish`].
    MarkEnabling,
    InstallDriver,
    /// Note each Remote Desktop rule's scope before the first change.
    RecordScopes,
    /// Note that the rules are about to be narrowed.
    NoteNarrowed,
    /// Narrow the rules to this PC.
    NarrowScopes,
    /// Put the noted scopes back and drop the narrowing note.
    RestoreScopes,
    /// Enable the Remote Desktop rules that are off, noting them first.
    OpenRules,
    /// Disable the rules a turn-on enabled, then forget them.
    CloseRules,
    /// Store the operator's choice: `true` lets the network in.
    WriteChoice(bool),
    /// Turn the Remote Desktop listener on, noting it was off.
    AllowListener,
    /// Bring the seat listener up; a failure only costs pad audio.
    EnsureSeatListener,
    /// Pin the certificate Remote Desktop presents now.
    TrustCertificate,
    /// Drop `Enabling`: seats are on.
    Finish,
    /// Turn the Remote Desktop listener back off.
    DenyListener,
    RemoveSeatListener,
    /// Point the box's Steam shortcuts back at Steam. Refuses while one still can't go back.
    RestoreShortcuts,
    RestoreRegistration,
    DeleteKey,
}

/// Seats on with `allow_network` as the operator's choice. `recorded` is `None` for a key this
/// call creates; otherwise seats are on already or a turn-on was cut off, and this redoes it.
pub(crate) fn on(recorded: Option<Recorded>, allow_network: bool) -> Vec<Step> {
    let fresh = recorded.is_none();
    let recorded = recorded.unwrap_or_default();
    let mut steps = Vec::new();
    if fresh {
        steps.push(Step::MarkEnabling);
    }
    steps.push(Step::InstallDriver);
    // Only a key this build marked notes scopes: an older one's rules may be narrowed already.
    if (fresh || recorded.enabling) && !recorded.scopes_noted {
        steps.push(Step::RecordScopes);
    }
    if allow_network {
        if narrowed(&recorded) {
            steps.push(Step::RestoreScopes);
        }
        steps.push(Step::OpenRules);
    } else {
        steps.extend([Step::NoteNarrowed, Step::NarrowScopes]);
        if recorded.rules_opened {
            steps.push(Step::CloseRules);
        }
    }
    steps.extend([
        Step::WriteChoice(allow_network),
        // Listen last: the firewall has its final scope by now.
        Step::AllowListener,
        Step::EnsureSeatListener,
        Step::TrustCertificate,
        Step::Finish,
    ]);
    steps
}

/// Seats off: undo what `recorded` notes, the listener first, then delete the key.
pub(crate) fn off(recorded: &Recorded) -> Vec<Step> {
    let mut steps = Vec::new();
    if recorded.listener_was_off {
        steps.push(Step::DenyListener);
    }
    if recorded.rules_opened {
        steps.push(Step::CloseRules);
    }
    if narrowed(recorded) {
        steps.push(Step::RestoreScopes);
    }
    // The Seats key holds the only record of the box's Steam shortcuts: it stays until they
    // are back.
    steps.extend([
        Step::RemoveSeatListener,
        Step::RestoreShortcuts,
        Step::RestoreRegistration,
        Step::DeleteKey,
    ]);
    steps
}

/// The rules may be narrowed to this PC: a turn-on noted that it narrows them, the operator
/// chose so, or a turn-on was cut off after noting their scopes and before noting the choice.
/// A key from a build without the note falls back on the last two. Restoring a rule that was
/// not narrowed writes back the scope it has.
pub(crate) fn narrowed(recorded: &Recorded) -> bool {
    recorded.narrowed_noted
        || recorded
            .choice
            .map_or(recorded.scopes_noted, |choice| choice == 0)
}

#[cfg(test)]
mod tests {
    use super::Step::*;
    use super::*;

    /// What a finished turn-on by this build leaves for `allow_network`.
    fn finished(allow_network: bool) -> Recorded {
        Recorded {
            choice: Some(u32::from(allow_network)),
            rules_opened: allow_network,
            scopes_noted: true,
            narrowed_noted: !allow_network,
            ..Recorded::default()
        }
    }

    const TAIL: [Step; 4] = [AllowListener, EnsureSeatListener, TrustCertificate, Finish];

    fn then_tail(head: &[Step]) -> Vec<Step> {
        head.iter().chain(&TAIL).copied().collect()
    }

    #[test]
    fn a_first_turn_on_narrows_before_it_listens() {
        assert_eq!(
            on(None, false),
            then_tail(&[
                MarkEnabling,
                InstallDriver,
                RecordScopes,
                NoteNarrowed,
                NarrowScopes,
                WriteChoice(false),
            ])
        );
        assert_eq!(
            on(None, true),
            then_tail(&[
                MarkEnabling,
                InstallDriver,
                RecordScopes,
                OpenRules,
                WriteChoice(true),
            ])
        );
    }

    #[test]
    fn a_turn_on_again_switches_the_scope_either_way() {
        // To this PC only: narrow, then close the rules seats opened.
        assert_eq!(
            on(Some(finished(true)), false),
            then_tail(&[
                InstallDriver,
                NoteNarrowed,
                NarrowScopes,
                CloseRules,
                WriteChoice(false),
            ])
        );
        // To the network: put the scopes back, then open the rules.
        assert_eq!(
            on(Some(finished(false)), true),
            then_tail(&[InstallDriver, RestoreScopes, OpenRules, WriteChoice(true)])
        );
        // The same choice again changes nothing it noted.
        assert_eq!(
            on(Some(finished(true)), true),
            then_tail(&[InstallDriver, OpenRules, WriteChoice(true)])
        );
        assert_eq!(
            on(Some(finished(false)), false),
            then_tail(&[
                InstallDriver,
                NoteNarrowed,
                NarrowScopes,
                WriteChoice(false)
            ])
        );
    }

    /// A key left `Enabling` notes the scopes it lacks; one from an older build never does.
    #[test]
    fn only_an_unfinished_key_notes_scopes_on_a_turn_on_again() {
        let cut_off = Recorded {
            enabling: true,
            ..Recorded::default()
        };
        assert_eq!(
            on(Some(cut_off), true),
            then_tail(&[InstallDriver, RecordScopes, OpenRules, WriteChoice(true)])
        );
        let narrowed_then_cut = Recorded {
            enabling: true,
            scopes_noted: true,
            narrowed_noted: true,
            ..Recorded::default()
        };
        assert_eq!(
            on(Some(narrowed_then_cut), true),
            then_tail(&[InstallDriver, RestoreScopes, OpenRules, WriteChoice(true)])
        );
        let older = Recorded {
            choice: Some(0),
            ..Recorded::default()
        };
        assert_eq!(
            on(Some(older), true),
            then_tail(&[InstallDriver, RestoreScopes, OpenRules, WriteChoice(true)])
        );
    }

    const OFF_TAIL: [Step; 4] = [
        RemoveSeatListener,
        RestoreShortcuts,
        RestoreRegistration,
        DeleteKey,
    ];

    fn then_off(head: &[Step]) -> Vec<Step> {
        head.iter().chain(&OFF_TAIL).copied().collect()
    }

    #[test]
    fn a_turn_off_denies_the_listener_before_it_widens() {
        let local = Recorded {
            listener_was_off: true,
            ..finished(false)
        };
        assert_eq!(off(&local), then_off(&[DenyListener, RestoreScopes]));
        assert_eq!(off(&finished(true)), then_off(&[CloseRules]));
        assert_eq!(off(&Recorded::default()), then_off(&[]));
    }

    #[test]
    fn a_turn_off_undoes_a_cut_off_or_older_turn_on() {
        // Cut off after noting the scopes, before noting the choice.
        let cut_off = Recorded {
            enabling: true,
            scopes_noted: true,
            ..Recorded::default()
        };
        assert_eq!(off(&cut_off), then_off(&[RestoreScopes]));
        // A switch to this PC only that failed after narrowing still holds the old choice.
        let failed_switch = Recorded {
            narrowed_noted: true,
            ..finished(true)
        };
        assert_eq!(off(&failed_switch), then_off(&[CloseRules, RestoreScopes]));
        // An older build noted neither scopes nor narrowing: the choice decides.
        let older = |choice| Recorded {
            choice: Some(choice),
            listener_was_off: true,
            ..Recorded::default()
        };
        assert_eq!(off(&older(0)), then_off(&[DenyListener, RestoreScopes]));
        assert_eq!(off(&older(1)), then_off(&[DenyListener]));
    }

    #[test]
    fn a_cut_off_turn_on_that_noted_the_scopes_may_have_narrowed_them() {
        let narrowed_by = |narrowed_noted, choice, scopes_noted| {
            narrowed(&Recorded {
                narrowed_noted,
                choice,
                scopes_noted,
                ..Recorded::default()
            })
        };
        // A switch to this PC only that failed after narrowing still holds the old choice.
        assert!(narrowed_by(true, Some(1), true));
        assert!(narrowed_by(true, None, true));
        // Without the note, as an older build leaves the key, the choice decides.
        assert!(narrowed_by(false, Some(0), false));
        assert!(!narrowed_by(false, Some(1), true));
        assert!(narrowed_by(false, None, true));
        // An older key has no record and always has the choice; a bare key has neither.
        assert!(!narrowed_by(false, None, false));
    }
}
