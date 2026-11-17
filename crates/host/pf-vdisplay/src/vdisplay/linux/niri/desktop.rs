//! The desk around a niri virtual output: the launch's workspace, the heads `exclusive` turns
//! off, the window list and verbs, and the output's scale.
//!
//! Each request is a pure builder, pinned by a test, and each pick is a pure function over the
//! decoded replies. niri answers `Handled` for an output, window or workspace it does not know,
//! so a step that matters is read back.

use super::ipc::{self, Output, Window, Workspace};
use super::{focus_output, list_monitors};
use crate::monitors::DISABLE_BUDGET;
use crate::toplevels::{Toplevel, WindowVerb};
use crate::topology_journal::{self as journal, Head, Undo};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn focus_workspace_action(id: u64) -> Value {
    json!({ "FocusWorkspace": { "reference": { "Id": id } } })
}

fn move_workspace_action(id: u64, output: &str) -> Value {
    json!({ "MoveWorkspaceToMonitor": { "output": output, "reference": { "Id": id } } })
}

fn move_window_action(id: u64, output: &str) -> Value {
    json!({ "MoveWindowToMonitor": { "id": id, "output": output } })
}

fn window_action_request(verb: WindowVerb, id: u64) -> Value {
    match verb {
        WindowVerb::Focus => json!({ "FocusWindow": { "id": id } }),
        // niri toggles; [`window_action`] skips a window that already fills its output.
        WindowVerb::Fullscreen => json!({ "FullscreenWindow": { "id": id } }),
    }
}

fn head_power(on: bool) -> Value {
    json!(if on { "On" } else { "Off" })
}

fn scale_action(scale: f64) -> Value {
    json!({ "Scale": { "scale": { "Specific": scale } } })
}

/// `width`×`height` at `mhz` as a custom mode. niri reads the refresh as decimal hertz.
pub(crate) fn custom_mode_action(width: u32, height: u32, mhz: u32) -> Value {
    let refresh = f64::from(mhz) / 1000.0;
    json!({ "CustomMode": { "mode": { "width": width, "height": height, "refresh": refresh } } })
}

/// `skip_isolated` is biri's, and biri requires it. Stock niri ignores the field.
fn power_off_action() -> Value {
    json!({ "PowerOffMonitors": { "skip_isolated": false } })
}

fn power_on_action() -> Value {
    json!({ "PowerOnMonitors": {} })
}

/// niri ids are `u64`; the claim carries them as `i64`.
fn niri_id(id: i64) -> Result<u64> {
    u64::try_from(id).map_err(|_| anyhow!("{id} is not a niri id"))
}

/// The workspace `output` shows.
fn active_workspace(spaces: &[Workspace], output: &str) -> Option<u64> {
    spaces
        .iter()
        .find(|s| s.is_active && s.output.as_deref() == Some(output))
        .map(|s| s.id)
}

/// No window sits on `space`. Both replies are consulted: they are two reads, and a window
/// that opened between them shows in only one.
fn is_empty(space: &Workspace, windows: &[Window]) -> bool {
    space.active_window_id.is_none() && !windows.iter().any(|w| w.workspace_id == Some(space.id))
}

/// `(claimed, restore)` for a launch on `output`.
///
/// `want` is the workspace an earlier session claimed, honoured while niri still has it on any
/// output. Otherwise the output's empty workspace: the one it shows when that is empty, else the
/// last. niri keeps one at the end of every output; a named workspace is the operator's and is
/// never taken.
fn pick_launch_workspace(
    spaces: &[Workspace],
    windows: &[Window],
    output: &str,
    want: Option<i64>,
) -> Option<(i64, i64)> {
    let restore = active_workspace(spaces, output)?;
    let head = || {
        spaces
            .iter()
            .filter(|s| s.output.as_deref() == Some(output))
    };
    let id = want
        .and_then(|w| u64::try_from(w).ok())
        .filter(|w| spaces.iter().any(|s| s.id == *w))
        .or_else(|| {
            head()
                .filter(|s| s.name.is_none() && is_empty(s, windows))
                .max_by_key(|s| (s.is_active, s.idx))
                .map(|s| s.id)
        })?;
    Some((i64::try_from(id).ok()?, i64::try_from(restore).ok()?))
}

/// Workspace this launch gets on head `name`, as `(claimed, restore)`. A claimed workspace that
/// another output holds is moved here first: focusing it alone would switch that output. `None`
/// when the head's workspaces cannot be read or the switch is refused: the launch then opens
/// where the head already looks.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let (spaces, windows) = (ipc::workspaces().ok()?, ipc::windows().ok()?);
    let (id, restore) = pick_launch_workspace(&spaces, &windows, name, want)?;
    if id != restore {
        let nid = niri_id(id).ok()?;
        let moved = if on_other_output(&spaces, nid, name) {
            ipc::action(move_workspace_action(nid, name))
        } else {
            Ok(())
        };
        if let Err(e) = moved.and_then(|()| ipc::action(focus_workspace_action(nid))) {
            tracing::warn!(
                workspace = id, output = %name, error = %format!("{e:#}"),
                "niri: workspace switch refused — this launch opens beside whatever the streamed \
                 head is already showing"
            );
            return None;
        }
    }
    Some((id, restore))
}

fn on_other_output(spaces: &[Workspace], id: u64, output: &str) -> bool {
    spaces
        .iter()
        .any(|s| s.id == id && s.output.as_deref() != Some(output))
}

/// Switch to workspace `id`, on the output that holds it.
pub(crate) fn focus_workspace(id: i64) -> Result<()> {
    ipc::action(focus_workspace_action(niri_id(id)?))
}

/// The workspace `output` shows, when it holds a window. An empty one carries nothing worth moving.
fn adoptable_workspace(spaces: &[Workspace], windows: &[Window], output: &str) -> Option<u64> {
    let id = active_workspace(spaces, output)?;
    let space = spaces.iter().find(|s| s.id == id)?;
    (!is_empty(space, windows)).then_some(id)
}

/// Carry `prev`'s workspace onto its replacement `new`, then show it there. A `prev` that is
/// gone, or shows nothing, is a reconnect with nothing to carry.
pub(crate) fn adopt_active_workspace(prev: &str, new: &str) {
    let (Ok(spaces), Ok(windows)) = (ipc::workspaces(), ipc::windows()) else {
        return;
    };
    let Some(id) = adoptable_workspace(&spaces, &windows, prev) else {
        return;
    };
    // Focusing makes the workspace active on `new` however the move leaves it.
    if let Err(e) = ipc::action(move_workspace_action(id, new))
        .and_then(|()| ipc::action(focus_workspace_action(id)))
    {
        tracing::warn!(
            workspace = id, from = %prev, to = %new, error = %format!("{e:#}"),
            "niri: could not move the streamed workspace to the replacement head — the client \
             will land on an empty workspace after this resize"
        );
        return;
    }
    match ipc::workspaces()
        .ok()
        .and_then(|ws| active_workspace(&ws, new))
    {
        Some(now) if now == id => tracing::info!(
            workspace = id, from = %prev, to = %new,
            "niri: carried the streamed workspace onto the replacement head"
        ),
        now => tracing::warn!(
            workspace = id, active = ?now, to = %new,
            "niri: the replacement head shows a different workspace after the move — the client \
             may land on an empty one"
        ),
    }
}

/// Is `name` driving a picture? `None` when niri does not list it.
fn head_lit(outputs: &[Output], name: &str) -> Option<bool> {
    outputs
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.logical.is_some())
}

/// Poll until `name` is `lit` or [`DISABLE_BUDGET`] runs out.
fn wait_head(name: &str, lit: bool) -> bool {
    let deadline = Instant::now() + DISABLE_BUDGET;
    loop {
        if ipc::outputs().is_ok_and(|o| head_lit(&o, name) == Some(lit)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Turn off the heads an `exclusive` session darkens ([`crate::monitors::disable_for_exclusive`]),
/// journaling the plan onto `pending` before the first goes dark. A planned head that refused
/// stays lit, so its undo is a no-op. `keep_monitors` stays lit and a managed sibling is never
/// touched.
pub(crate) fn disable_other_heads(ours: &str, pending: &mut Vec<u64>) {
    let keep = crate::policy::prefs().get().keep_monitors;
    crate::monitors::disable_for_exclusive(
        "niri",
        list_monitors(),
        ours,
        &keep,
        disable_head,
        focus_output,
        |plan| {
            let heads = plan
                .iter()
                .map(|name| Head {
                    name: name.clone(),
                    mode: None,
                })
                .collect();
            pending.push(journal::push("niri", Undo::EnableHeads { heads }));
        },
    );
}

/// `Off`, read back: the listing drops the head's logical geometry once it is dark.
fn disable_head(name: &str) -> Result<()> {
    ipc::output_action(name, head_power(false))?;
    if wait_head(name, false) {
        return Ok(());
    }
    bail!("niri accepted `Off` for {name} but the output never went dark")
}

/// `On` for each of `disabled`, read back. The heads still dark, each logged with the hand
/// command. A head niri no longer lists was unplugged meanwhile: nothing to re-light.
pub(crate) fn restore_heads(disabled: &[String]) -> Vec<String> {
    let mut dark = Vec::new();
    for name in disabled {
        match ipc::outputs().map(|o| head_lit(&o, name)) {
            Ok(None) => {
                tracing::info!(output = %name, "niri: a head `topology: exclusive` darkened is gone");
                continue;
            }
            Ok(Some(true)) => continue,
            _ => {}
        }
        match ipc::output_action(name, head_power(true)) {
            Ok(()) if wait_head(name, true) => {
                tracing::info!(output = %name, "niri: re-enabled the output `topology: exclusive` disabled");
            }
            Ok(()) => {
                tracing::warn!(
                    output = %name,
                    "niri: `On` was accepted but the output is still dark — re-enable it by hand \
                     with `niri msg output {name} on`"
                );
                dark.push(name.clone());
            }
            Err(e) => {
                tracing::error!(
                    output = %name, error = %format!("{e:#}"),
                    "niri: could not re-enable this output — it is still dark. Run \
                     `niri msg output {name} on` by hand."
                );
                dark.push(name.clone());
            }
        }
    }
    dark
}

/// Take back one journaled desk change: re-light heads, or remove an output this niri named and
/// still lists. Another niri's output is left alone (it may have reused the name).
pub(crate) fn undo(undo: &Undo) -> Result<()> {
    match undo {
        Undo::EnableHeads { heads } => {
            let names: Vec<String> = heads.iter().map(|h| h.name.clone()).collect();
            let dark = restore_heads(&names);
            if dark.is_empty() {
                return Ok(());
            }
            bail!(
                "{} still dark — `niri msg output <name> on` re-lights it",
                dark.join(", ")
            )
        }
        Undo::RemoveOutput { name, instance } => {
            if journal::foreign_output("niri", name, instance, &ipc::instance()) {
                return Ok(());
            }
            if !ipc::outputs()?.iter().any(|o| o.name == *name) {
                return Ok(());
            }
            ipc::remove_virtual_output(name)
                .with_context(|| format!("remove virtual output {name}"))
        }
        Undo::Takeover { .. } => bail!("no undo in niri for {undo:?}"),
    }
}

/// Power off every head for a **gamescope** `exclusive` stream, when there is a physical one to
/// darken. niri has no per-output DPMS, and any local input wakes the heads.
pub(crate) fn power_off_others() -> bool {
    let Ok(heads) = list_monitors() else {
        return false;
    };
    if crate::monitors::heads_to_darken(&heads, "", &[]).is_empty() {
        return false;
    }
    match ipc::action(power_off_action()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "niri: monitors not powered off for `topology: exclusive`");
            false
        }
    }
}

/// Wake the heads [`power_off_others`] darkened.
pub(crate) fn power_on_all() -> Result<()> {
    ipc::action(power_on_action())
}

/// niri reports no full-screen flag; a tile as large as its output stands in for it.
fn fills_output(window: &Window, output: Option<&Output>) -> bool {
    let Some(l) = output.and_then(|o| o.logical.as_ref()) else {
        return false;
    };
    let (w, h) = window.layout.tile_size;
    l.width > 0 && l.height > 0 && w >= f64::from(l.width) - 1.0 && h >= f64::from(l.height) - 1.0
}

/// Windows on `head`, or on every head with `None`. A window on a workspace no listing
/// claims, or on none, is dropped: its head is unproven.
fn parse_toplevels(
    windows: &[Window],
    spaces: &[Workspace],
    outputs: &[Output],
    head: Option<&str>,
) -> Vec<Toplevel> {
    windows
        .iter()
        .filter_map(|w| {
            let space = spaces.iter().find(|s| Some(s.id) == w.workspace_id)?;
            let on = space.output.as_deref()?;
            head.is_none_or(|want| want == on).then_some(())?;
            Some(Toplevel {
                id: w.id.to_string(),
                title: w.title.clone().unwrap_or_default(),
                app_id: w.app_id.clone().unwrap_or_default(),
                pid: w.pid.and_then(|p| u32::try_from(p).ok()),
                focused: w.is_focused,
                fullscreen: fills_output(w, outputs.iter().find(|o| o.name == on)),
                workspace: space.name.clone().unwrap_or_else(|| space.idx.to_string()),
                output: on.to_string(),
            })
        })
        .collect()
}

/// Windows on head `output`, or on every head with `None`. Empty on any failed read: this list
/// is never worth an error.
pub(crate) fn toplevels(output: Option<&str>) -> Vec<Toplevel> {
    match (ipc::windows(), ipc::workspaces(), ipc::outputs()) {
        (Ok(windows), Ok(spaces), Ok(outputs)) => {
            parse_toplevels(&windows, &spaces, &outputs, output)
        }
        _ => {
            tracing::debug!(output = ?output, "niri: no window list");
            Vec::new()
        }
    }
}

/// Carry window `id` onto head `dest`.
pub(crate) fn move_to_output(id: &str, dest: &str) -> Result<()> {
    ipc::action(move_window_action(window_id(id)?, dest))
}

/// Run one verb on window `id`. A full-screen window is left alone for `Fullscreen`, which niri
/// toggles.
pub(crate) fn window_action(verb: WindowVerb, id: &str) -> Result<()> {
    if verb == WindowVerb::Fullscreen && toplevels(None).iter().any(|w| w.id == id && w.fullscreen)
    {
        return Ok(());
    }
    ipc::action(window_action_request(verb, window_id(id)?))
}

fn window_id(id: &str) -> Result<u64> {
    id.parse()
        .map_err(|_| anyhow!("window id {id} is not ours"))
}

/// Bumped on every event that changes the window list. Free-running and shared by all heads: a
/// "re-read" signal, not a count.
static WINDOW_GEN: AtomicU64 = AtomicU64::new(0);

/// An event-stream reader is feeding [`WINDOW_GEN`]. Without one the counter is frozen.
static WINDOW_READER: AtomicBool = AtomicBool::new(false);

/// The event-stream thread calls this for each event `events::moves_windows` passes, and on
/// connect.
#[allow(dead_code, reason = "called by the event-stream reader")]
pub(crate) fn bump_window_gen() {
    WINDOW_GEN.fetch_add(1, Ordering::Relaxed);
}

/// The event-stream thread brackets its life with this.
#[allow(dead_code, reason = "called by the event-stream reader")]
pub(crate) fn set_window_reader_live(live: bool) {
    WINDOW_READER.store(live, Ordering::Relaxed);
}

/// Re-read token for [`crate::toplevels::toplevels_token`]. `None` with no reader: a frozen
/// value would pin one stale list forever.
pub(crate) fn window_gen() -> Option<u64> {
    WINDOW_READER
        .load(Ordering::Relaxed)
        .then(|| WINDOW_GEN.load(Ordering::Relaxed))
}

/// Start `name` at the device's own scale. Best-effort: the stream runs at niri's default
/// without it.
pub(crate) fn apply_scale(name: &str, scale: f64) {
    if !(scale.is_finite() && scale > 0.0) {
        tracing::warn!(output = %name, scale, "niri: display scale ignored — not a positive number");
        return;
    }
    match ipc::output_action(name, scale_action(scale)) {
        Ok(()) => tracing::info!(output = %name, scale, "niri: applied the client's display scale"),
        Err(e) => tracing::warn!(
            output = %name, scale, error = %format!("{e:#}"),
            "niri: display scale not applied — the stream keeps niri's default"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitors::{heads_to_darken, PhysicalMonitor};
    use serde_json::from_value;

    fn spaces() -> Vec<Workspace> {
        from_value(json!([
            {"id":1,"idx":1,"name":null,"output":"DP-1","is_active":true,"is_focused":false,"active_window_id":10},
            {"id":2,"idx":2,"name":null,"output":"DP-1","is_active":false,"is_focused":false,"active_window_id":null},
            {"id":3,"idx":1,"name":null,"output":"PF-1234-1","is_active":true,"is_focused":true,"active_window_id":20},
            {"id":4,"idx":2,"name":null,"output":"PF-1234-1","is_active":false,"is_focused":false,"active_window_id":null},
            {"id":5,"idx":3,"name":"chat","output":"PF-1234-1","is_active":false,"is_focused":false,"active_window_id":null}
        ]))
        .unwrap()
    }

    /// [`spaces`] with every window closed.
    fn without_windows() -> Vec<Workspace> {
        let mut spaces = spaces();
        spaces.iter_mut().for_each(|s| s.active_window_id = None);
        spaces
    }

    fn windows() -> Vec<Window> {
        from_value(json!([
            {"id":10,"title":"Private call — Ada","app_id":"discord","pid":7,"workspace_id":1,
             "is_focused":false,"layout":{"tile_size":[1200.0,800.0]}},
            {"id":20,"title":"Dota 2","app_id":"steam_app_570","pid":4242,"workspace_id":3,
             "is_focused":true,"layout":{"tile_size":[2560.0,1600.0]}},
            {"id":21,"title":"Steam","app_id":"steam","pid":99,"workspace_id":3,
             "is_focused":false,"layout":{"tile_size":[800.0,600.0]}},
            {"id":22,"title":"orphan","app_id":"x","pid":null,"workspace_id":77},
            {"id":23,"title":"sticky","app_id":"y","pid":null,"workspace_id":null}
        ]))
        .unwrap()
    }

    fn outputs() -> Vec<Output> {
        from_value(json!([
            {"name":"DP-1","make":"ACME","model":"U2720Q","current_mode":0,
             "logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0}},
            {"name":"PF-1234-1","make":"niri","model":"virtual","current_mode":0,
             "logical":{"x":1920,"y":0,"width":2560,"height":1600,"scale":1.0}},
            {"name":"DP-3","current_mode":0,"logical":null}
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

    /// `exclusive` darkens the operator's heads. A sibling session's output, ours or another
    /// host's, must survive, or the second exclusive session blacks out the first.
    #[test]
    fn exclusive_never_disables_a_managed_sibling() {
        let ours = "PF-4242-1";
        let heads = [
            head("DP-1", true),
            head("HDMI-A-1", true),
            head(ours, true),
            head("PF-4242-2", true),
            head("PF-99-1", true),
            // Already dark: re-lighting it at teardown would undo the operator's choice.
            head("DP-3", false),
        ];
        assert_eq!(heads_to_darken(&heads, ours, &[]), vec!["DP-1", "HDMI-A-1"]);
        assert_eq!(
            heads_to_darken(&heads, ours, &["hdmi-a-1".into()]),
            vec!["DP-1"]
        );
    }

    #[test]
    fn exclusive_on_a_headless_box_disables_nothing() {
        let ours = "PF-4242-1";
        assert!(heads_to_darken(&[head(ours, true)], ours, &[]).is_empty());
    }

    /// Dark means no logical geometry; an unlisted head is a third answer, not "dark".
    #[test]
    fn a_head_is_lit_when_niri_gives_it_a_logical_position() {
        let outs = outputs();
        assert_eq!(head_lit(&outs, "DP-1"), Some(true));
        assert_eq!(head_lit(&outs, "DP-3"), Some(false));
        assert_eq!(head_lit(&outs, "HDMI-A-9"), None);
    }

    #[test]
    fn the_window_list_holds_the_streamed_head_only() {
        let list = parse_toplevels(&windows(), &spaces(), &outputs(), Some("PF-1234-1"));
        assert_eq!(
            list.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["20", "21"]
        );
        let game = &list[0];
        assert_eq!(game.title, "Dota 2");
        assert_eq!(game.app_id, "steam_app_570");
        assert_eq!(game.pid, Some(4242));
        assert_eq!(game.workspace, "1");
        assert_eq!(game.output, "PF-1234-1");
        assert!(game.focused);
        assert!(!list[1].focused);
        // The private title is on DP-1, and DP-1 is not what this session streams.
        assert!(!list.iter().any(|w| w.title.contains("Private")));
        let all = parse_toplevels(&windows(), &spaces(), &outputs(), None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].output, "DP-1");
    }

    /// A window on a workspace no listing claims, or on none, has no head we can prove.
    #[test]
    fn a_window_on_an_unknown_workspace_is_never_ours() {
        for scope in [Some("PF-1234-1"), Some("DP-1"), None] {
            let list = parse_toplevels(&windows(), &spaces(), &outputs(), scope);
            assert!(
                !list.iter().any(|w| w.id == "22" || w.id == "23"),
                "{scope:?}"
            );
        }
    }

    /// niri sets no full-screen flag: a tile that fills its output is the signal, and a window
    /// with no layout reads as windowed.
    #[test]
    fn a_tile_as_large_as_its_output_is_full_screen() {
        let list = parse_toplevels(&windows(), &spaces(), &outputs(), Some("PF-1234-1"));
        assert!(list[0].fullscreen);
        assert!(!list[1].fullscreen);
        let bare: Vec<Window> = from_value(json!([{"id":9,"workspace_id":3}])).unwrap();
        let list = parse_toplevels(&bare, &spaces(), &outputs(), None);
        assert!(!list[0].fullscreen);
        assert_eq!(list[0].title, "");
    }

    /// The head's own empty workspace decides; the operator's empty one and a named one are
    /// not ours to take.
    #[test]
    fn a_launch_lands_on_the_streamed_heads_empty_workspace() {
        let (spaces, windows) = (spaces(), windows());
        assert_eq!(
            pick_launch_workspace(&spaces, &windows, "PF-1234-1", None),
            Some((4, 3)),
            "workspace 5 is named and empty, 4 is the unnamed one"
        );
        assert_eq!(
            pick_launch_workspace(&spaces, &windows, "DP-1", None),
            Some((2, 1))
        );
        assert_eq!(
            pick_launch_workspace(&spaces, &windows, "PF-9-9", None),
            None
        );
    }

    /// A window that opened between the two reads shows in one of them; either makes the
    /// workspace occupied.
    #[test]
    fn a_workspace_with_a_window_in_either_reply_is_not_empty() {
        let (mut spaces, windows) = (spaces(), windows());
        let four = spaces.iter_mut().find(|s| s.id == 4).unwrap();
        four.active_window_id = Some(30);
        assert_eq!(
            pick_launch_workspace(&spaces, &windows, "PF-1234-1", None),
            None,
            "4 holds a window the list has not caught up with, 5 is named"
        );
        let late: Vec<Window> = from_value(json!([{"id":31,"workspace_id":4}])).unwrap();
        assert!(!is_empty(&without_windows()[3], &late));
    }

    /// A head already showing an empty workspace stays on it: nothing to switch, nothing to
    /// restore.
    #[test]
    fn a_head_showing_an_empty_workspace_stays_on_it() {
        assert_eq!(
            pick_launch_workspace(&without_windows(), &[], "PF-1234-1", None),
            Some((3, 3))
        );
    }

    /// `want` is the workspace an earlier session claimed: honoured on any output, since a
    /// reconnect's head is new and the game's workspace moved off the old one. Ignored once
    /// niri no longer has it. Only one on another output is moved.
    #[test]
    fn want_is_honoured_wherever_niri_still_has_it() {
        let (spaces, windows) = (spaces(), windows());
        let pick = |head, want| pick_launch_workspace(&spaces, &windows, head, want);
        assert_eq!(pick("PF-1234-1", Some(5)), Some((5, 3)));
        assert_eq!(pick("PF-1234-1", Some(2)), Some((2, 3)), "2 is DP-1's");
        assert_eq!(pick("PF-1234-1", Some(99)), Some((4, 3)), "99 is gone");
        assert_eq!(pick("PF-1234-1", Some(-1)), Some((4, 3)));
        assert!(on_other_output(&spaces, 2, "PF-1234-1"));
        assert!(!on_other_output(&spaces, 5, "PF-1234-1"));
    }

    #[test]
    fn an_active_workspace_is_carried_only_when_it_holds_a_window() {
        let (spaces, windows) = (spaces(), windows());
        assert_eq!(adoptable_workspace(&spaces, &windows, "PF-1234-1"), Some(3));
        assert_eq!(
            adoptable_workspace(&without_windows(), &[], "PF-1234-1"),
            None
        );
        assert_eq!(adoptable_workspace(&spaces, &windows, "PF-9-9"), None);
    }

    /// Every request as it goes on the wire. niri's stock build must parse these too; the one
    /// field stock lacks, `skip_isolated`, it ignores.
    #[test]
    fn requests_are_pinned_to_the_niri_ipc_shapes() {
        let action = ipc::action_request;
        assert_eq!(
            action(focus_workspace_action(7)),
            json!({"Action":{"FocusWorkspace":{"reference":{"Id":7}}}})
        );
        assert_eq!(
            action(move_workspace_action(7, "PF-1-2")),
            json!({"Action":{"MoveWorkspaceToMonitor":{"output":"PF-1-2","reference":{"Id":7}}}})
        );
        assert_eq!(
            action(move_window_action(9, "PF-1-2")),
            json!({"Action":{"MoveWindowToMonitor":{"id":9,"output":"PF-1-2"}}})
        );
        assert_eq!(
            action(window_action_request(WindowVerb::Focus, 9)),
            json!({"Action":{"FocusWindow":{"id":9}}})
        );
        assert_eq!(
            action(window_action_request(WindowVerb::Fullscreen, 9)),
            json!({"Action":{"FullscreenWindow":{"id":9}}})
        );
        assert_eq!(
            action(power_off_action()),
            json!({"Action":{"PowerOffMonitors":{"skip_isolated":false}}})
        );
        assert_eq!(
            action(power_on_action()),
            json!({"Action":{"PowerOnMonitors":{}}})
        );
        assert_eq!(
            ipc::output_request("DP-1", head_power(false)),
            json!({"Output":{"output":"DP-1","action":"Off"}})
        );
        assert_eq!(
            ipc::output_request("DP-1", head_power(true)),
            json!({"Output":{"output":"DP-1","action":"On"}})
        );
        assert_eq!(
            ipc::output_request("PF-1-1", scale_action(1.5)),
            json!({"Output":{"output":"PF-1-1","action":{"Scale":{"scale":{"Specific":1.5}}}}})
        );
        assert_eq!(
            ipc::output_request("PF-1-1", custom_mode_action(1920, 1080, 59_940)),
            json!({"Output":{"output":"PF-1-1","action":{"CustomMode":{"mode":
                {"width":1920,"height":1080,"refresh":59.94}}}}})
        );
    }

    #[test]
    fn ids_cross_the_i64_claim_without_wrapping() {
        assert_eq!(niri_id(4).unwrap(), 4);
        assert!(niri_id(-1).is_err());
        assert_eq!(window_id("20").unwrap(), 20);
        assert!(window_id("0x20").is_err());
    }

    /// Only heads are journaled; any other entry is moot and must not stay for the next pass.
    #[test]
    fn an_output_entry_is_moot_for_niri() {
        let moot = Undo::RemoveOutput {
            name: "PF-1-1".into(),
            instance: "niri.wayland-1.42.sock".into(),
        };
        assert!(undo(&moot).is_ok());
    }

    /// No reader, no token; with one, a bump moves it.
    #[test]
    fn the_window_token_needs_a_live_reader() {
        assert_eq!(window_gen(), None);
        set_window_reader_live(true);
        let before = window_gen().expect("a live reader gives a token");
        bump_window_gen();
        assert_eq!(window_gen(), Some(before + 1));
        set_window_reader_live(false);
        assert_eq!(window_gen(), None);
    }
}
