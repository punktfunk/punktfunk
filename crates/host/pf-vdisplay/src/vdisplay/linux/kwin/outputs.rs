//! The KWin output seam: where each output operation picks its half.
//!
//! Topology, position, custom mode, re-enable and list each run in-process over
//! `kde_output_management_v2` ([`super::mgmt`]) first. When that half declines, the reason is
//! one log line and `kscreen-doctor` ([`super::kscreen`]) runs once; [`Resolved`] names the half
//! that answered. [`gate`] is the in-process half's answer before it addresses an output: a
//! connection that is bound, caught up, and new enough for the request. [`plan`] decides a
//! topology from the outputs as a half reads them ([`Row`]); the half only stages it.
//!
//! Pinned by the tables in this file's tests: the fallback over (management bound, version,
//! barrier), the topology plan over the outputs it reads.

use super::mgmt::{self, TopologyKind};
use super::{kscreen, MANAGED_PREFIX};
use crate::layout::{Placement, Rect};
use std::fmt;

/// `kde_output_configuration_v2.set_custom_modes` is `since 18`.
pub(crate) const CUSTOM_MODES_SINCE: u32 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Topology,
    Position,
    Scale,
    CustomMode,
    Reenable,
    List,
}

/// Why the in-process half declined an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Declined {
    /// No Wayland connection.
    NoWayland(String),
    /// Connected, but no management connection to use.
    Unbound(&'static str),
    /// A roundtrip missed its budget: the compositor is not answering this client.
    Barrier(&'static str),
    /// The bound management version lacks the request.
    Version { bound: u32, needs: u32 },
    /// What the operation addresses is not on the connection.
    Unresolved(String),
    /// KWin did not confirm the apply in budget; its failure reason when it sent one.
    Unconfirmed(Option<String>),
}

impl fmt::Display for Declined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Declined::NoWayland(e) => write!(f, "no Wayland connection ({e})"),
            Declined::Unbound(what) => f.write_str(what),
            Declined::Barrier(what) => {
                write!(f, "the compositor did not answer the {what} in budget")
            }
            Declined::Version { bound, needs } => {
                write!(
                    f,
                    "management is bound at v{bound}, the request needs v{needs}"
                )
            }
            Declined::Unresolved(what) => write!(f, "{what} did not resolve on this connection"),
            Declined::Unconfirmed(None) => f.write_str("KWin did not confirm the apply"),
            Declined::Unconfirmed(Some(reason)) => {
                write!(f, "KWin did not confirm the apply ({reason})")
            }
        }
    }
}

/// The half that answered an operation.
#[derive(Debug, PartialEq)]
pub(crate) enum Resolved<I, K = I> {
    InProcess(I),
    Kscreen(K),
    /// The in-process half declined, and `kscreen-doctor` did not run or did not answer.
    Neither(Declined),
}

/// The in-process half's answer before it addresses an output. `seen` is the connection's
/// bound management version and whether it caught up in budget; `None` is no connection.
pub(crate) fn gate(op: Op, seen: Option<(u32, bool)>) -> Result<(), Declined> {
    let Some((bound, caught_up)) = seen else {
        return Err(Declined::Unbound("no output-management connection"));
    };
    if !caught_up {
        return Err(Declined::Barrier("catch-up roundtrip"));
    }
    match op {
        Op::CustomMode if bound < CUSTOM_MODES_SINCE => Err(Declined::Version {
            bound,
            needs: CUSTOM_MODES_SINCE,
        }),
        _ => Ok(()),
    }
}

/// Whether `kscreen-doctor` runs after the in-process half declined `op`. Not for a custom
/// mode on a bind below v18: `addCustomMode` needs the same request, so its 6–9 spawns
/// only delay the create's recreate at 60 Hz.
fn falls_back(op: Op, why: &Declined) -> bool {
    !matches!((op, why), (Op::CustomMode, Declined::Version { .. }))
}

/// `in_process`'s answer, else `kscreen` once when [`falls_back`] allows it. The decline is
/// logged here, once: no Wayland at debug (the console lists monitors outside any session).
pub(crate) fn resolve<I, K>(
    op: Op,
    in_process: Result<I, Declined>,
    kscreen: impl FnOnce() -> Option<K>,
) -> Resolved<I, K> {
    let why = match in_process {
        Ok(r) => return Resolved::InProcess(r),
        Err(why) => why,
    };
    let fallback = falls_back(op, &why);
    match &why {
        Declined::NoWayland(_) => {
            tracing::debug!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
        Declined::Version { .. } => {
            tracing::info!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
        _ => {
            tracing::warn!(?op, reason = %why, fallback, "KWin in-process output management declined")
        }
    }
    match fallback.then(kscreen).flatten() {
        Some(r) => Resolved::Kscreen(r),
        None => Resolved::Neither(why),
    }
}

/// One output as a half reads it, for [`plan`].
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Row {
    pub name: Option<String>,
    pub enabled: bool,
    /// KWin's output priority; 1 is the primary. `None` when the half does not report it.
    pub priority: Option<u32>,
    /// Rect in KWin's logical space; `None` without a current mode.
    pub rect: Option<Rect>,
    /// Current mode as `WxH@Hz`, empty when unknown: what a teardown restores.
    pub mode: String,
    /// The output this create just made.
    pub ours: bool,
}

/// A topology apply, as indices into the rows it was planned from.
#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    pub take_primary: bool,
    /// The rows to disable (`Exclusive` only).
    pub disable: Vec<usize>,
    /// Where ours goes; `None` keeps KWin's arrangement.
    pub origin: Option<Placement>,
    /// Every other row in the priority order to send from 2: lit ones first, then dark.
    pub others: Vec<usize>,
}

impl Plan {
    /// `(name, mode)` of each row to disable, for the journal.
    pub fn disabled(&self, rows: &[Row]) -> Vec<(String, String)> {
        self.disable
            .iter()
            .filter_map(|&i| Some((rows[i].name.clone()?, rows[i].mode.clone())))
            .collect()
    }
}

/// Decide the topology for `kind` over `rows`, with the heads in `keep` kept lit.
///
/// Primary unless `Extend` or another managed output already holds it: a second exclusive
/// session joins as a secondary. An output with our name is this slot's predecessor
/// mid-supersede, not a sibling; deferring to it would hand primary to whatever KWin
/// promotes when it goes. Ours takes the origin [`crate::layout::origin_for`] gives beside
/// the outputs that stay lit: KWin parks a new output right of the row and never
/// re-normalizes, and Plasma places popups off a lone screen whose origin is not zero.
/// Rows without ours (an unreadable `kscreen-doctor -j`) place nothing.
pub(crate) fn plan(rows: &[Row], kind: TopologyKind, keep: &[String]) -> Plan {
    let ours = rows.iter().find(|r| r.ours);
    let our_name = ours.and_then(|r| r.name.as_deref());
    let managed = |r: &Row| {
        r.name
            .as_deref()
            .is_some_and(|n| n.starts_with(MANAGED_PREFIX))
    };
    let sibling_is_primary = rows.iter().any(|r| {
        r.enabled && r.priority == Some(1) && !r.ours && r.name.as_deref() != our_name && managed(r)
    });
    let disable: Vec<usize> = match kind {
        TopologyKind::Exclusive => (0..rows.len())
            .filter(|&i| {
                let r = &rows[i];
                let darkens =
                    |n: &str| crate::monitors::darkens(n, r.enabled, managed(r), r.ours, keep);
                r.name.as_deref().is_some_and(darkens)
            })
            .collect(),
        TopologyKind::Primary | TopologyKind::Extend => Vec::new(),
    };
    let dark = |i: usize| !rows[i].enabled || disable.contains(&i);
    let lit: Vec<Rect> = (0..rows.len())
        .filter(|&i| !rows[i].ours && !dark(i))
        .filter_map(|i| rows[i].rect)
        .collect();
    let mut others: Vec<usize> = (0..rows.len()).filter(|&i| !rows[i].ours).collect();
    others.sort_by_key(|&i| (dark(i), rows[i].priority.unwrap_or(u32::MAX)));
    Plan {
        take_primary: kind != TopologyKind::Extend && !sibling_is_primary,
        origin: ours.and_then(|r| crate::layout::origin_for(&lit, r.rect)),
        disable,
        others,
    }
}

/// Place the just-created output `our_prefix` (at `dims`) for `kind` on `sess` (the create's
/// [`mgmt::watch`]), `journal` getting the heads `Exclusive` darkens before the disable.
/// In-process answers our output's UUID; `kscreen-doctor` the kscreen address it resolved,
/// for every kind, so a later position never addresses the shared name.
pub(crate) fn topology(
    mut sess: Option<&mut mgmt::Session>,
    name: &str,
    our_prefix: &str,
    dims: (u32, u32),
    kind: TopologyKind,
    mut journal: impl FnMut(&[(String, String)]),
) -> Resolved<Option<String>, String> {
    let (w, h) = dims;
    // The heads the operator asked to keep lit through an exclusive stream (§5.5).
    let keep = crate::policy::prefs().get().keep_monitors;
    let watch = sess.as_deref_mut();
    let in_process = mgmt::apply_topology(watch, our_prefix, (w, h), kind, &keep, &mut journal);
    resolve(Op::Topology, in_process, || {
        let addr = kscreen::resolve_kscreen_addr(name, w, h);
        kscreen::apply_topology(&addr, kind, &keep, &mut journal);
        // kscreen-doctor clears no mirror source here; Extend clears it in-process.
        if kind == TopologyKind::Extend {
            mgmt::clear_replication_source(sess, our_prefix, w, h);
        }
        Some(addr)
    })
}

/// Place our output `ours` at `(x, y)`: in-process on the create's kept `watch`, else at the
/// kscreen address the create resolved.
pub(crate) fn position(
    watch: Option<&mut mgmt::Session>,
    ours: &str,
    uuid: Option<&str>,
    kscreen_addr: Option<&str>,
    x: i32,
    y: i32,
) -> Resolved<()> {
    let in_process = mgmt::set_position(watch, ours, uuid, x, y);
    resolve(Op::Position, in_process, || {
        kscreen::set_position(kscreen_addr?, x, y).then_some(())
    })
}

/// `(width, height, refresh in millihertz)` of an output's mode.
pub(crate) type ModeMhz = (u32, u32, u32);

/// Install and select `want` on our output, which sits at `at` now. Both halves answer the
/// active mode they read back; `kscreen-doctor` also the address it used. `kscreen-doctor`
/// takes and reports whole hertz.
pub(crate) fn custom_mode(
    sess: Option<&mut mgmt::Session>,
    name: &str,
    our_prefix: &str,
    at: (u32, u32),
    want: ModeMhz,
) -> Resolved<ModeMhz, (String, Option<ModeMhz>)> {
    let (w, h, mhz) = want;
    let in_process = mgmt::set_custom_mode(sess, our_prefix, at.0, at.1, w, h, mhz);
    resolve(Op::CustomMode, in_process, || {
        let addr = kscreen::resolve_kscreen_addr(name, at.0, at.1);
        let active = kscreen::set_custom_refresh(w, h, (mhz + 500) / 1000, &addr)
            .map(|(w, h, hz)| (w, h, hz.saturating_mul(1000)));
        Some((addr, active))
    })
}

/// Re-enable `heads` (`(name, "WxH@Hz")`) at their modes.
pub(crate) fn reenable(heads: &[(String, String)]) -> Resolved<()> {
    resolve(Op::Reenable, mgmt::reenable_outputs(heads), || {
        kscreen::reenable_outputs_kscreen(heads);
        Some(())
    })
}

/// Every head KWin reports. A miss of both halves is an error, not "no monitors":
/// `monitors::resolve` treats it as one, so a pinned capture monitor refuses to start.
/// With KWin gone it fails at once, and a journal entry waits for the next KWin.
pub(crate) fn list() -> anyhow::Result<Vec<crate::monitors::PhysicalMonitor>> {
    match resolve(Op::List, mgmt::list_monitors(), kscreen::list_monitors) {
        Resolved::InProcess(heads) | Resolved::Kscreen(heads) => Ok(heads),
        Resolved::Neither(why) => Err(anyhow::anyhow!(
            "kscreen-doctor -j did not answer either (KWin not running the session, not \
             installed, crashed, or killed at its budget): \
             KWin output management declined: {why}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each operation runs in-process on a connection that is bound, caught up and new enough
    /// for it, and otherwise runs `kscreen-doctor` exactly once, except a custom mode below
    /// v18, which `kscreen-doctor` cannot install either.
    #[test]
    fn each_operation_runs_in_process_then_kscreen_once() {
        use Op::*;
        let ok = Resolved::<(), ()>::InProcess(());
        let kscreen = Resolved::Kscreen(());
        let unbound = Resolved::Neither(Declined::Unbound("no output-management connection"));
        let v17 = Resolved::Neither(Declined::Version {
            bound: 17,
            needs: 18,
        });
        // (operation, (bound version, caught up), kscreen answers) → (answer, kscreen runs)
        let rows = [
            (Topology, Some((22, true)), true, (&ok, false)),
            (Topology, None, true, (&kscreen, true)),
            (Topology, Some((22, false)), true, (&kscreen, true)),
            (Topology, None, false, (&unbound, true)),
            (Position, Some((22, true)), false, (&ok, false)),
            (Position, None, true, (&kscreen, true)),
            (CustomMode, Some((18, true)), true, (&ok, false)),
            (CustomMode, Some((17, true)), true, (&v17, false)),
            (CustomMode, Some((17, true)), false, (&v17, false)),
            (CustomMode, Some((22, false)), true, (&kscreen, true)),
            (Reenable, Some((13, true)), true, (&ok, false)),
            (Reenable, Some((13, false)), true, (&kscreen, true)),
            (List, Some((3, true)), false, (&ok, false)),
            (List, None, true, (&kscreen, true)),
            (List, None, false, (&unbound, true)),
        ];
        for (op, seen, answers, (want, runs)) in rows {
            let mut ran = 0;
            let got = resolve(op, gate(op, seen), || {
                ran += 1;
                answers.then_some(())
            });
            assert_eq!(&got, want, "{op:?} {seen:?}");
            assert_eq!(ran, usize::from(runs), "{op:?} {seen:?}: kscreen runs");
        }
    }

    /// A decline past the gate (our output missing, an unconfirmed apply) falls back the same.
    #[test]
    fn a_decline_after_the_gate_falls_back_once() {
        for why in [
            Declined::Unresolved("our virtual output".into()),
            Declined::Unconfirmed(Some("mode not found".into())),
        ] {
            let got = resolve(Op::Topology, Err::<(), _>(why.clone()), || Some("7"));
            assert_eq!(got, Resolved::Kscreen("7"), "{why}");
            let got = resolve(Op::Topology, Err::<(), _>(why.clone()), || None::<()>);
            assert_eq!(got, Resolved::Neither(why));
        }
    }

    /// The version gate is the protocol's: v18 carries `set_custom_modes`, v17 does not.
    #[test]
    fn only_custom_modes_need_v18() {
        assert_eq!(
            gate(Op::CustomMode, Some((17, true))),
            Err(Declined::Version {
                bound: 17,
                needs: 18
            })
        );
        for op in [
            Op::Topology,
            Op::Position,
            Op::Scale,
            Op::Reenable,
            Op::List,
        ] {
            assert_eq!(gate(op, Some((1, true))), Ok(()), "{op:?}");
        }
    }

    fn head(name: &str, enabled: bool, priority: u32, x: i32) -> Row {
        Row {
            name: Some(name.to_string()),
            enabled,
            priority: Some(priority),
            rect: Some(Rect {
                x,
                y: 0,
                w: 1920,
                h: 1080,
            }),
            mode: "1920x1080@60".to_string(),
            ours: false,
        }
    }

    fn ours_at(x: i32) -> Row {
        Row {
            ours: true,
            ..head("Virtual-punktfunk-1", true, 3, x)
        }
    }

    /// The topology decision over what a half reads: who takes primary, what goes dark,
    /// where ours lands.
    #[test]
    fn the_plan_follows_the_topology_rules() {
        use TopologyKind::*;
        let desk = head("eDP-1", true, 1, 0);
        let sibling = head("Virtual-punktfunk-7", true, 1, 1920);
        let predecessor = head("Virtual-punktfunk-1", true, 1, 1920);
        let at = |x| Some(Placement { x, y: 0 });
        let kept = ["edp-1".to_string()];
        // (outputs, kind, the desk kept lit) → (take primary, disabled, origin)
        let rows = [
            // Exclusive darkens the desk, then ours is alone and takes the origin.
            (
                vec![desk.clone(), ours_at(1920)],
                Exclusive,
                false,
                (true, vec!["eDP-1"], at(0)),
            ),
            (
                vec![desk.clone(), ours_at(1920)],
                Primary,
                false,
                (true, vec![], None),
            ),
            (
                vec![desk.clone(), ours_at(1920)],
                Extend,
                false,
                (false, vec![], None),
            ),
            // A stored origin on top of a lit screen moves ours right of the row.
            (
                vec![desk.clone(), ours_at(0)],
                Extend,
                false,
                (false, vec![], at(1920)),
            ),
            // Another session's output holds primary: ours joins as a secondary.
            (
                vec![desk.clone(), sibling.clone(), ours_at(3840)],
                Exclusive,
                false,
                (false, vec!["eDP-1"], None),
            ),
            (
                vec![sibling.clone(), ours_at(3840)],
                Primary,
                false,
                (false, vec![], None),
            ),
            // Our name on another output is this slot's predecessor, not a sibling.
            (
                vec![predecessor.clone(), ours_at(3840)],
                Exclusive,
                false,
                (true, vec![], None),
            ),
            // A kept monitor stays lit, and ours stays off it.
            (
                vec![desk.clone(), ours_at(0)],
                Exclusive,
                true,
                (true, vec![], at(1920)),
            ),
            (
                vec![desk.clone(), ours_at(0)],
                Exclusive,
                false,
                (true, vec!["eDP-1"], at(0)),
            ),
        ];
        for (outputs, kind, keep_desk, (primary, dark, origin)) in rows {
            let keep = if keep_desk { &kept[..] } else { &[] };
            let plan = plan(&outputs, kind, keep);
            let disabled = plan.disabled(&outputs);
            let names: Vec<&str> = disabled.iter().map(|(n, _)| n.as_str()).collect();
            let row = format!("{kind:?} over {outputs:?}, keep {keep:?}");
            assert_eq!(plan.take_primary, primary, "{row}");
            assert_eq!(names, dark, "{row}");
            assert_eq!(plan.origin, origin, "{row}");
        }
    }

    /// Priorities from 2 go to the lit outputs in their order, then the dark ones: KWin
    /// 6.0–6.5 refuses an order that leaves one out. The journal gets each mode.
    #[test]
    fn the_priority_order_puts_dark_outputs_last() {
        let outputs = vec![
            head("DP-2", false, 2, 3840),
            head("Virtual-punktfunk-7", true, 3, 1920),
            ours_at(5760),
            head("eDP-1", true, 1, 0),
        ];
        let plan_of = |rows: &[Row]| plan(rows, TopologyKind::Exclusive, &[]);
        let plan = plan_of(&outputs);
        assert_eq!(plan.others, vec![1, 3, 0], "sibling lit, then the two dark");
        let unread = plan_of(&[]);
        assert_eq!(
            (unread.take_primary, unread.origin),
            (true, None),
            "nothing to place by"
        );
        assert_eq!(
            plan.disabled(&outputs),
            vec![("eDP-1".to_string(), "1920x1080@60".to_string())]
        );
    }
}
