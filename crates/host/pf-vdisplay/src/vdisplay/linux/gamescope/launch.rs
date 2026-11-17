//! The SteamOS takeover's restart verdict: what one look at the restarted
//! `gamescope-session.target` tells the connect to do.
//!
//! The shell (`gamescope.rs`, `create_managed_session_steamos`) restarts the target under a
//! budget, then looks at it until [`step`] stops waiting. A flags verdict is a one-way latch for
//! the host's life (`discovery.rs`, `note_spawn_flags_lost`), so it is only ever taken on a
//! gamescope that started after the restart: the previous session's stock compositor is no
//! evidence about ours. Built on every platform so the table runs without systemd.
//!
//! Design: `design/pf-vdisplay-host-overhaul.md` §3 D4 in the planning repo.

/// How the target's restart stands at this look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verb {
    /// `systemctl restart` returned success.
    Done,
    /// The restart outlived its budget. systemd still runs the job; what it brings up is late.
    StillRunning,
    /// systemd refused, the helper did not run, the unit died, or the box was handed back.
    Failed(String),
    /// The restart finished and the wait for its node ran out.
    TimedOut,
}

/// The unit's gamescope node at this look, by when its owner started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Node {
    None,
    /// Started before the restart, or its start is unknown: not proven ours.
    Older,
    /// Started at or after the restart.
    Younger,
}

impl Node {
    /// A published node by its owner's start and the restart's, both in clock ticks since boot.
    pub(crate) fn aged(started: Option<u64>, restarted_at: Option<u64>) -> Node {
        match (started, restarted_at) {
            (Some(started), Some(at)) if started >= at => Node::Younger,
            _ => Node::Older,
        }
    }
}

/// Whether the node's gamescope carries the flags the drop-in passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flags {
    Present,
    Missing,
    /// Not looked at, or its argv could not be read.
    Unverifiable,
}

/// Why the box goes back to its own session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Why {
    Failed(String),
    StillRunning,
    /// No node of the restarted session within the wait.
    NoNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Stream this node.
    Use,
    /// Hand the box back and fail the connect.
    HandBack(Why),
    /// Our gamescope ran without our flags: latch the loss, hand back, fail the connect.
    LatchLost,
    /// Look again.
    Wait,
}

/// One look's verdict. Only a younger node is used or latches on.
pub(crate) fn step(verb: &Verb, node: Node, flags: Flags) -> Verdict {
    match (verb, node, flags) {
        (Verb::Failed(why), _, _) => Verdict::HandBack(Why::Failed(why.clone())),
        (Verb::StillRunning, _, _) => Verdict::HandBack(Why::StillRunning),
        (_, Node::Younger, Flags::Missing) => Verdict::LatchLost,
        (_, Node::Younger, _) => Verdict::Use,
        (Verb::TimedOut, _, _) => Verdict::HandBack(Why::NoNode),
        (Verb::Done, _, _) => Verdict::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODES: [Node; 3] = [Node::None, Node::Older, Node::Younger];
    const FLAGS: [Flags; 3] = [Flags::Present, Flags::Missing, Flags::Unverifiable];

    fn verbs() -> [Verb; 4] {
        [
            Verb::Done,
            Verb::StillRunning,
            Verb::Failed("systemctl exited with exit status: 1".into()),
            Verb::TimedOut,
        ]
    }

    /// Each row names what it pins. A restart that is not done is never polled for 30 s; the
    /// previous session's node never latches the host's flags off.
    #[test]
    fn the_restart_follows_its_table() {
        use {Flags::*, Node as N, Verb as V, Verdict::*, Why as W};
        let exit = || "systemctl exited with exit status: 1".to_string();
        let failed = V::Failed(exit());
        let rows: &[(&Verb, Node, Flags, Verdict)] = &[
            // The restart's budget ran out: hand back now, no node wait.
            (
                &V::StillRunning,
                N::None,
                Unverifiable,
                HandBack(W::StillRunning),
            ),
            (
                &V::StillRunning,
                N::Younger,
                Present,
                HandBack(W::StillRunning),
            ),
            // systemd said no: its reason is the connect's error.
            (&failed, N::None, Unverifiable, HandBack(W::Failed(exit()))),
            // Done, nothing published yet.
            (&V::Done, N::None, Unverifiable, Wait),
            // The previous session's stock gamescope: never used, never latched.
            (&V::Done, N::Older, Missing, Wait),
            (&V::Done, N::Older, Present, Wait),
            // Ours, without our flags: the latch.
            (&V::Done, N::Younger, Missing, LatchLost),
            // Ours, with them, or argv unreadable (silence is not a loss).
            (&V::Done, N::Younger, Present, Use),
            (&V::Done, N::Younger, Unverifiable, Use),
            // The wait ran out: only the old session, or nothing, published.
            (&V::TimedOut, N::None, Unverifiable, HandBack(W::NoNode)),
            (&V::TimedOut, N::Older, Missing, HandBack(W::NoNode)),
            // A node on the last look still counts.
            (&V::TimedOut, N::Younger, Present, Use),
            (&V::TimedOut, N::Younger, Missing, LatchLost),
        ];
        for (verb, node, flags, want) in rows {
            assert_eq!(
                step(verb, *node, *flags),
                *want,
                "{verb:?} {node:?} {flags:?}"
            );
        }
    }

    /// Over every input: only a younger node is used or latches, and a restart that is not
    /// done always hands back.
    #[test]
    fn only_a_younger_node_is_used_or_latched() {
        for verb in verbs() {
            for node in NODES {
                for flags in FLAGS {
                    let got = step(&verb, node, flags);
                    if matches!(got, Verdict::Use | Verdict::LatchLost) {
                        assert_eq!(node, Node::Younger, "{verb:?} {flags:?} -> {got:?}");
                    }
                    if matches!(verb, Verb::StillRunning | Verb::Failed(_)) {
                        assert!(matches!(got, Verdict::HandBack(_)), "{verb:?} -> {got:?}");
                    }
                }
            }
        }
    }

    /// A node whose owner's start is unknown is not proven ours; one that started on the
    /// restart's own tick is.
    #[test]
    fn a_node_is_younger_only_by_its_owners_start() {
        assert_eq!(Node::aged(Some(1_000), Some(900)), Node::Younger);
        assert_eq!(Node::aged(Some(900), Some(900)), Node::Younger);
        assert_eq!(Node::aged(Some(899), Some(900)), Node::Older);
        assert_eq!(Node::aged(None, Some(900)), Node::Older);
        assert_eq!(Node::aged(Some(1_000), None), Node::Older);
    }
}
