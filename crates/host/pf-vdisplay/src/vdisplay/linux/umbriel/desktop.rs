//! The desk around an Umbriel virtual output: the launch's workspace, the heads `exclusive`
//! turns off, the gamescope darken, and the window list and verbs.
//!
//! Each action is a pure builder, pinned by a test, and each pick is a pure function
//! over the decoded replies. Umbriel answers `ok` for an action that matched nothing, so a
//! step that matters is read back. Workspaces are addressed by their 1-based index on an
//! output; window moves and full-screen act on the focused window, so they focus it first.

use super::ipc::{self, Action, Output, Window, Workspace};
use super::{focus_output, list_monitors};
use crate::monitors::DISABLE_BUDGET;
use crate::toplevels::{Toplevel, WindowVerb};
use crate::topology_journal::{self as journal, Undo};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};

/// `verb` on one thing: the typed request's `field`, or `verb:target` for `msg`.
fn on_one(verb: &str, field: &str, target: &str) -> Action {
    Action {
        typed: json!({"cmd": "action", "action": verb, field: target}),
        word: format!("{verb}:{target}"),
    }
}

/// `verb` on workspace `index` of `output`, its 1-based position there.
fn on_workspace(verb: &str, index: u32, output: &str) -> Action {
    Action {
        typed: json!({"cmd": "action", "action": verb, "workspace": index, "output": output}),
        word: format!("{verb}:{index}/{output}"),
    }
}

pub(crate) fn switch_action(index: u32, output: &str) -> Action {
    on_workspace("workspace-switch", index, output)
}

fn focus_action(id: &str) -> Action {
    on_one("window-focus", "window", id)
}

fn move_silent_action(index: u32, output: &str) -> Action {
    on_workspace("window-move-to-workspace-silent", index, output)
}

/// Toggles the focused window; [`window_action`] skips one already full-screen.
fn toggle_fullscreen_action() -> Action {
    const VERB: &str = "window-toggle-fullscreen";
    Action {
        typed: json!({"cmd": "action", "action": VERB}),
        word: VERB.to_string(),
    }
}

fn head_action(on: bool, name: &str) -> Action {
    let verb = if on {
        "output-enable"
    } else {
        "output-disable"
    };
    on_one(verb, "output", name)
}

fn dpms_action(on: bool, name: &str) -> Action {
    on_one(if on { "dpms-on" } else { "dpms-off" }, "output", name)
}

/// The index of the workspace `output` shows.
pub(crate) fn active_index(spaces: &[Workspace], output: &str) -> Option<u32> {
    spaces
        .iter()
        .find(|s| s.active && s.output.as_deref() == Some(output))
        .map(|s| s.index)
}

/// `(claimed, restore)` for a launch on `output`, as 1-based indexes on it.
///
/// `want` is the index an earlier session claimed, honoured while the output still has it.
/// Otherwise an unoccupied workspace of the output: the one it shows when that is free, else
/// the lowest. `None` when every workspace there holds a window.
fn pick_launch_workspace(
    spaces: &[Workspace],
    output: &str,
    want: Option<i64>,
) -> Option<(i64, i64)> {
    let restore = active_index(spaces, output)?;
    let head = || {
        spaces
            .iter()
            .filter(|s| s.output.as_deref() == Some(output))
    };
    let index = want
        .and_then(|w| u32::try_from(w).ok())
        .filter(|w| head().any(|s| s.index == *w))
        .or_else(|| {
            head()
                .filter(|s| !s.occupied && s.window_count == 0)
                .min_by_key(|s| (!s.active, s.index))
                .map(|s| s.index)
        })?;
    Some((index.into(), restore.into()))
}

/// Workspace this launch gets on head `name`, as `(claimed, restore)`, switched to and read
/// back. `None` when the head's workspaces cannot be read or the switch did not take: the
/// launch then opens where the head already looks.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let spaces = ipc::workspaces().ok()?;
    let (index, restore) = pick_launch_workspace(&spaces, name, want)?;
    if index != restore {
        let switched = focus_workspace(name, index).and_then(|()| {
            let now = active_index(&ipc::workspaces()?, name);
            if now.map(i64::from) != Some(index) {
                bail!("{name} still shows workspace {now:?}");
            }
            Ok(())
        });
        if let Err(e) = switched {
            tracing::warn!(
                workspace = index, output = %name, error = %format!("{e:#}"),
                "umbriel: workspace switch did not take — this launch opens beside whatever the \
                 streamed head is already showing"
            );
            return None;
        }
    }
    Some((index, restore))
}

/// Show workspace `index` on `output`. Umbriel also moves keyboard focus there.
pub(crate) fn focus_workspace(output: &str, index: i64) -> Result<()> {
    let index = u32::try_from(index).with_context(|| format!("{index} is not a workspace"))?;
    ipc::msg(&switch_action(index, output))
}

/// Carry every window on `prev` onto `new`'s shown workspace, then focus again what was
/// focused. Destroying `prev` would hand them to the first other enabled output, which may be
/// the operator's screen.
pub(crate) fn adopt_windows(prev: &str, new: &str) {
    let (Ok(windows), Ok(spaces)) = (ipc::windows(), ipc::workspaces()) else {
        return;
    };
    let moving: Vec<&Window> = windows
        .iter()
        .filter(|w| w.output.as_deref() == Some(prev))
        .collect();
    let Some(index) = active_index(&spaces, new).filter(|_| !moving.is_empty()) else {
        return;
    };
    for w in &moving {
        if let Err(e) = move_window(&w.id, index, new) {
            tracing::warn!(
                window = %w.id, from = %prev, to = %new, error = %format!("{e:#}"),
                "umbriel: window not carried onto the replacement head"
            );
        }
    }
    if let Some(focused) = windows.iter().find(|w| w.focused) {
        let _ = ipc::msg(&focus_action(&focused.id));
    }
    tracing::info!(
        from = %prev, to = %new, windows = moving.len(),
        "umbriel: carried the streamed windows onto the replacement head"
    );
}

fn move_window(id: &str, index: u32, output: &str) -> Result<()> {
    ipc::msg(&focus_action(id))?;
    ipc::msg(&move_silent_action(index, output))
}

fn head<'a>(outputs: &'a [Output], name: &str) -> Option<&'a Output> {
    outputs.iter().find(|o| o.name == name)
}

/// Poll until `name` reads `enabled == want` or [`DISABLE_BUDGET`] runs out.
fn wait_head(name: &str, want: bool) -> bool {
    let deadline = Instant::now() + DISABLE_BUDGET;
    loop {
        if ipc::outputs().is_ok_and(|o| head(&o, name).is_some_and(|h| h.enabled == want)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Turn off the heads an `exclusive` session darkens ([`crate::monitors::disable_for_exclusive`]),
/// journaling the plan onto `pending` before the first goes dark. `keep_monitors` stays lit and
/// a managed sibling is never touched. A config reload re-enables them; that is not watched.
pub(crate) fn disable_other_heads(ours: &str, pending: &mut Vec<u64>) {
    let keep = crate::policy::prefs().get().keep_monitors;
    crate::monitors::disable_for_exclusive(
        "umbriel",
        list_monitors(),
        ours,
        &keep,
        disable_head,
        focus_output,
        |plan| pending.push(journal::push_heads("umbriel", plan)),
    );
}

fn disable_head(name: &str) -> Result<()> {
    ipc::msg(&head_action(false, name))?;
    if wait_head(name, false) {
        return Ok(());
    }
    bail!("umbriel accepted output-disable for {name} but the output stayed enabled")
}

/// `output-enable` and `dpms-on` for each of `names`: an exclusive disable or a gamescope
/// darken, whichever this was. The heads still disabled after the read-back. A head Umbriel no
/// longer lists was unplugged meanwhile.
fn restore_heads(names: &[String]) -> Vec<String> {
    let mut dark = Vec::new();
    for name in names {
        let Ok(outputs) = ipc::outputs() else {
            dark.push(name.clone());
            continue;
        };
        let Some(h) = head(&outputs, name) else {
            continue;
        };
        if h.enabled && h.powered {
            continue;
        }
        let back =
            ipc::msg(&head_action(true, name)).and_then(|()| ipc::msg(&dpms_action(true, name)));
        match back {
            Ok(()) if wait_head(name, true) => {
                tracing::info!(output = %name, "umbriel: re-enabled the output the stream turned off")
            }
            Ok(()) => dark.push(name.clone()),
            Err(e) => {
                tracing::warn!(output = %name, error = %format!("{e:#}"), "umbriel: output not re-enabled");
                dark.push(name.clone());
            }
        }
    }
    dark
}

/// Take back one journaled desk change: re-light heads, or destroy an output this Umbriel
/// named and still lists. Another instance's output is left alone.
pub(crate) fn undo(undo: &Undo) -> Result<()> {
    match undo {
        Undo::EnableHeads { heads } => {
            let names: Vec<String> = heads.iter().map(|h| h.name.clone()).collect();
            let dark = restore_heads(&names);
            if dark.is_empty() {
                return Ok(());
            }
            bail!(
                "{} still off — `umbriel msg output-enable <name>` or `umbriel msg config-reload` \
                 re-lights it",
                dark.join(", ")
            )
        }
        Undo::RemoveOutput { name, instance } => {
            if journal::foreign_output("umbriel", name, instance, &ipc::instance()) {
                return Ok(());
            }
            if head(&ipc::outputs()?, name).is_none() {
                return Ok(());
            }
            ipc::destroy_output(name).with_context(|| format!("destroy virtual output {name}"))
        }
        Undo::Takeover { .. } => bail!("no undo in umbriel for {undo:?}"),
    }
}

/// DPMS for one head; `Ok(false)` when it already was in that state, so the darken never
/// records a head the operator had off.
fn set_dpms(name: &str, on: bool) -> Result<bool> {
    if head(&ipc::outputs()?, name).is_some_and(|h| h.powered == on) {
        return Ok(false);
    }
    ipc::msg(&dpms_action(on, name)).map(|()| true)
}

/// DPMS off every head but a sibling's for a **gamescope** `exclusive` stream
/// ([`crate::monitors::dpms_others`]), with `record(plan)` run before the first goes dark.
pub(crate) fn dpms_other_heads(record: impl FnOnce(&[String])) -> Vec<String> {
    let heads = list_monitors();
    if let Ok(listed) = &heads {
        record(&crate::monitors::heads_to_darken(listed, "", &[]));
    }
    crate::monitors::dpms_others("umbriel", heads, |n| set_dpms(n, false))
}

/// `dpms-on` for exactly `names`, the heads [`dpms_other_heads`] darkened.
pub(crate) fn relight_heads(names: &[String]) -> Vec<String> {
    crate::monitors::relight("umbriel", names, |n| set_dpms(n, true))
}

/// A window's workspace as Umbriel spells it: a name, or a number.
fn spell(workspace: &serde_json::Value) -> String {
    match workspace {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Windows on head `output`, or on every head with `None`. A window on no output is dropped:
/// its head is unproven.
fn parse_toplevels(windows: &[Window], output: Option<&str>) -> Vec<Toplevel> {
    windows
        .iter()
        .filter_map(|w| {
            let on = w.output.as_deref()?;
            output.is_none_or(|want| want == on).then(|| Toplevel {
                id: w.id.clone(),
                title: w.title.clone().unwrap_or_default(),
                app_id: w.app_id.clone().unwrap_or_default(),
                pid: w.pid.and_then(|p| u32::try_from(p).ok()).filter(|&p| p > 0),
                focused: w.focused,
                fullscreen: w.fullscreen,
                workspace: spell(&w.workspace),
                output: on.to_string(),
            })
        })
        .collect()
}

/// Windows on head `output`, or on every head with `None`. Empty on a failed read.
pub(crate) fn toplevels(output: Option<&str>) -> Vec<Toplevel> {
    match ipc::windows() {
        Ok(windows) => parse_toplevels(&windows, output),
        Err(e) => {
            tracing::debug!(output = ?output, error = %format!("{e:#}"), "umbriel: no window list");
            Vec::new()
        }
    }
}

/// Carry window `id` onto `dest`'s shown workspace, read back.
pub(crate) fn move_to_output(id: &str, dest: &str) -> Result<()> {
    let index = active_index(&ipc::workspaces()?, dest)
        .with_context(|| format!("umbriel shows no workspace on {dest}"))?;
    move_window(id, index, dest)?;
    if toplevels(Some(dest)).iter().any(|w| w.id == id) {
        return Ok(());
    }
    bail!("umbriel accepted the move of window {id} but it is not on {dest}")
}

/// Run one verb on window `id`. A full-screen window is left alone for `Fullscreen`.
pub(crate) fn window_action(verb: WindowVerb, id: &str) -> Result<()> {
    match verb {
        WindowVerb::Focus => ipc::msg(&focus_action(id)),
        WindowVerb::Fullscreen => {
            if toplevels(None).iter().any(|w| w.id == id && w.fullscreen) {
                return Ok(());
            }
            ipc::msg(&focus_action(id))?;
            ipc::msg(&toggle_fullscreen_action())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitors::{heads_to_darken, PhysicalMonitor};
    use serde_json::{from_value, json};

    fn spaces() -> Vec<Workspace> {
        from_value(json!([
            {"id":"DP-1:1","index":1,"output":"DP-1","active":true,"occupied":true,"window_count":1},
            {"id":"DP-1:2","index":2,"output":"DP-1","active":false,"occupied":false,"window_count":0},
            {"id":"PF-1234-1:1","index":1,"output":"PF-1234-1","active":true,"occupied":true,"window_count":2},
            {"id":"PF-1234-1:2","index":2,"output":"PF-1234-1","active":false,"occupied":true,"window_count":1},
            {"id":"PF-1234-1:3","index":3,"output":"PF-1234-1","active":false,"occupied":false,"window_count":0},
            {"id":"PF-1234-1:4","index":4,"output":"PF-1234-1","active":false,"occupied":false,"window_count":0}
        ]))
        .unwrap()
    }

    fn windows() -> Vec<Window> {
        from_value(json!([
            {"id":"a","title":"Private call — Ada","app_id":"discord","pid":7,"output":"DP-1",
             "workspace":"DP-1:1","fullscreen":false,"focused":false},
            {"id":"b","title":"Dota 2","app_id":"steam_app_570","pid":4242,"output":"PF-1234-1",
             "workspace":"PF-1234-1:1","fullscreen":true,"focused":true},
            {"id":"c","title":"Steam","app_id":"steam","pid":99,"output":"PF-1234-1","workspace":2},
            {"id":"d","title":"orphan","app_id":"x","pid":0}
        ]))
        .unwrap()
    }

    fn head(connector: &str, enabled: bool) -> PhysicalMonitor {
        PhysicalMonitor {
            connector: connector.to_string(),
            description: connector.to_string(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            x: 0,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled,
            managed: crate::hyprland::is_managed_output(connector),
        }
    }

    /// A sibling session's output, ours or another host's, survives a second exclusive session.
    #[test]
    fn exclusive_never_disables_a_managed_sibling() {
        let ours = "PF-4242-1";
        let heads = [
            head("DP-1", true),
            head(ours, true),
            head("PF-4242-2", true),
            head("PF-3", true),
            head("HDMI-A-1", false),
        ];
        assert_eq!(heads_to_darken(&heads, ours, &[]), vec!["DP-1"]);
        assert!(heads_to_darken(&heads, ours, &["dp-1".into()]).is_empty());
    }

    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "PF-4242-1";
        assert!(heads_to_darken(&[head(ours, true)], ours, &[]).is_empty());
    }

    #[test]
    fn the_window_list_holds_the_streamed_head_only() {
        let list = parse_toplevels(&windows(), Some("PF-1234-1"));
        assert_eq!(
            list.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
        let game = &list[0];
        assert_eq!(
            (game.title.as_str(), game.app_id.as_str(), game.pid),
            ("Dota 2", "steam_app_570", Some(4242))
        );
        assert!(game.focused && game.fullscreen);
        assert_eq!(game.workspace, "PF-1234-1:1");
        assert_eq!(list[1].workspace, "2");
        assert!(!list.iter().any(|w| w.title.contains("Private")));
        let all = parse_toplevels(&windows(), None);
        assert_eq!(all.len(), 3, "a window on no output is never listed");
    }

    #[test]
    fn a_launch_lands_on_an_unoccupied_workspace() {
        let spaces = spaces();
        assert_eq!(
            pick_launch_workspace(&spaces, "PF-1234-1", None),
            Some((3, 1)),
            "the lowest free index"
        );
        assert_eq!(pick_launch_workspace(&spaces, "DP-1", None), Some((2, 1)));
        assert_eq!(pick_launch_workspace(&spaces, "PF-9-9", None), None);
    }

    /// A head already showing a free workspace stays on it: nothing to switch or restore.
    #[test]
    fn a_head_showing_a_free_workspace_stays_on_it() {
        let mut spaces = spaces();
        for s in spaces
            .iter_mut()
            .filter(|s| s.output.as_deref() == Some("PF-1234-1"))
        {
            s.active = s.index == 4;
        }
        assert_eq!(
            pick_launch_workspace(&spaces, "PF-1234-1", None),
            Some((4, 4))
        );
    }

    /// `want` is honoured while the output still has that index, occupied or not.
    #[test]
    fn want_is_honoured_while_the_output_has_it() {
        let spaces = spaces();
        let pick = |want| pick_launch_workspace(&spaces, "PF-1234-1", want);
        assert_eq!(pick(Some(2)), Some((2, 1)));
        assert_eq!(pick(Some(9)), Some((3, 1)), "9 is gone");
        assert_eq!(pick(Some(-1)), Some((3, 1)));
    }

    /// Every action in both shapes, with exactly the fields Umbriel's typed parser allows.
    #[test]
    fn actions_are_pinned_to_the_umbriel_shapes() {
        let pins = [
            (
                switch_action(3, "PF-1-0"),
                json!({"cmd":"action","action":"workspace-switch","workspace":3,"output":"PF-1-0"}),
                "workspace-switch:3/PF-1-0",
            ),
            (
                focus_action("t1"),
                json!({"cmd":"action","action":"window-focus","window":"t1"}),
                "window-focus:t1",
            ),
            (
                move_silent_action(2, "PF-1-0"),
                json!({"cmd":"action","action":"window-move-to-workspace-silent",
                       "workspace":2,"output":"PF-1-0"}),
                "window-move-to-workspace-silent:2/PF-1-0",
            ),
            (
                head_action(false, "DP-1"),
                json!({"cmd":"action","action":"output-disable","output":"DP-1"}),
                "output-disable:DP-1",
            ),
            (
                head_action(true, "DP-1"),
                json!({"cmd":"action","action":"output-enable","output":"DP-1"}),
                "output-enable:DP-1",
            ),
            (
                dpms_action(false, "DP-1"),
                json!({"cmd":"action","action":"dpms-off","output":"DP-1"}),
                "dpms-off:DP-1",
            ),
            (
                dpms_action(true, "DP-1"),
                json!({"cmd":"action","action":"dpms-on","output":"DP-1"}),
                "dpms-on:DP-1",
            ),
            (
                toggle_fullscreen_action(),
                json!({"cmd":"action","action":"window-toggle-fullscreen"}),
                "window-toggle-fullscreen",
            ),
        ];
        for (action, typed, word) in pins {
            assert_eq!(action.typed, typed);
            assert_eq!(action.word, word);
        }
    }

    /// No socket here, so the live instance is empty and every output entry is another's.
    #[test]
    fn an_output_entry_without_a_live_umbriel_is_moot() {
        let moot = Undo::RemoveOutput {
            name: "PF-1-1".into(),
            instance: "/run/user/1000/umbriel-wayland-1.sock".into(),
        };
        assert!(undo(&moot).is_ok());
    }
}
