//! The desk around a mango output: the heads as the host sees them, the launch's tag, the heads
//! `exclusive` turns off, the gamescope darken, and the window list and verbs.
//!
//! Output management says which heads are lit and at what mode; the IPC says where each sits and
//! which one is focused. A head mango put to sleep (`sleep_monitor`, or an idle daemon over
//! output power) reads disabled there but keeps its box in the layout, which a disabled head
//! loses. It is listed lit: powered off, not turned off. mango answers `success` for a dispatch
//! that matched nothing, so a step that matters is read back.

use super::{dispatch, focus_output, list_monitors, monitor_spec, request, word};
use crate::monitors::{PhysicalMonitor, DISABLE_BUDGET};
use crate::output_mgmt::{self, HeadState, Set};
use crate::toplevels::{Toplevel, WindowVerb};
use crate::topology_journal::{self as journal, Head, Undo};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Deserialize)]
struct IpcMonitor {
    name: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Tag {
    index: u32,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    client_count: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Client {
    id: u32,
    #[serde(default)]
    pid: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    appid: String,
    #[serde(default)]
    monitor: String,
    #[serde(default)]
    tags: Vec<u32>,
    #[serde(default)]
    is_focused: bool,
    #[serde(default)]
    is_fullscreen: bool,
}

/// `key` of a reply, decoded; an `error` reply fails.
fn field<T: serde::de::DeserializeOwned>(reply: serde_json::Value, key: &str) -> Result<T> {
    if let Some(e) = reply.get("error") {
        bail!("mango: {e}");
    }
    let v = reply
        .get(key)
        .cloned()
        .with_context(|| format!("no {key} in {reply}"))?;
    serde_json::from_value(v).with_context(|| format!("decode {key}"))
}

fn ipc_monitors() -> Result<Vec<IpcMonitor>> {
    field(request("get all-monitors")?, "monitors")
}

fn tags(monitor: &str) -> Result<Vec<Tag>> {
    field(request(&format!("get tags {}", word(monitor)?))?, "tags")
}

fn clients() -> Result<Vec<Client>> {
    field(request("get all-clients")?, "clients")
}

/// One head as output management and the IPC see it together.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeskHead {
    head: HeadState,
    /// Its IPC entry. A head mango put to sleep keeps its box there; a disabled one has none.
    ipc: Option<IpcMonitor>,
}

impl DeskHead {
    fn name(&self) -> &str {
        &self.head.name
    }

    fn lit(&self) -> bool {
        self.head.enabled
    }

    /// Off, but kept in the layout.
    fn asleep(&self) -> bool {
        !self.head.enabled
            && self
                .ipc
                .as_ref()
                .is_some_and(|m| m.width > 0 && m.height > 0)
    }

    /// Where it sits. Output management reports a dark head at the origin; the IPC does not.
    fn position(&self) -> (i32, i32) {
        match &self.ipc {
            Some(m) if m.width > 0 => (m.x, m.y),
            _ => (self.head.x, self.head.y),
        }
    }

    fn monitor(&self) -> PhysicalMonitor {
        let scale = if self.head.scale > 0.0 {
            self.head.scale
        } else {
            1.0
        };
        let (width, height, refresh_mhz) = match (self.head.mode, &self.ipc) {
            (Some(mode), _) => mode,
            // Asleep, it reports no mode: its box in pixels, at an unknown rate.
            (None, Some(m)) if self.asleep() => {
                let px = |v: i32| (f64::from(v) * scale).round() as u32;
                (px(m.width), px(m.height), 0)
            }
            _ => (0, 0, 0),
        };
        let (x, y) = self.position();
        PhysicalMonitor {
            description: crate::monitors::describe(&self.head.make, &self.head.model, self.name()),
            width,
            height,
            refresh_mhz,
            x,
            y,
            scale,
            primary: self.ipc.as_ref().is_some_and(|m| m.active),
            enabled: self.lit() || self.asleep(),
            managed: crate::hyprland::is_managed_output(self.name()),
            connector: self.head.name.clone(),
        }
    }
}

fn join(heads: Vec<HeadState>, ipc: &[IpcMonitor]) -> Vec<DeskHead> {
    heads
        .into_iter()
        .map(|head| DeskHead {
            ipc: ipc.iter().find(|m| m.name == head.name).cloned(),
            head,
        })
        .collect()
}

/// Every head, with what the IPC adds. An IPC that does not answer adds nothing.
pub(crate) fn desk() -> Result<Vec<DeskHead>> {
    let heads = output_mgmt::heads()?;
    let ipc = ipc_monitors().unwrap_or_else(|e| {
        tracing::debug!(error = %format!("{e:#}"), "mango: no monitor list over the IPC");
        Vec::new()
    });
    Ok(join(heads, &ipc))
}

pub(super) fn monitors(desk: &[DeskHead]) -> Vec<PhysicalMonitor> {
    desk.iter().map(DeskHead::monitor).collect()
}

fn desk_head(name: &str) -> Result<Option<DeskHead>> {
    Ok(desk()?.into_iter().find(|d| d.name() == name))
}

/// Poll until `name` reads lit (`want`) or [`DISABLE_BUDGET`] runs out.
fn wait_lit(name: &str, want: bool) -> bool {
    let deadline = Instant::now() + DISABLE_BUDGET;
    loop {
        if output_mgmt::heads().is_ok_and(|h| h.iter().any(|h| h.name == name && h.enabled == want))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The tag `monitor` shows first.
fn shown(tags: &[Tag]) -> Option<u32> {
    tags.iter().find(|t| t.is_active).map(|t| t.index)
}

/// `(claimed, restore)` for a launch on a monitor, as 1-based tags.
///
/// `want` is the tag an earlier session claimed, honoured while the monitor has it. Otherwise an
/// empty tag: the one it shows when that is empty, else the lowest. `None` when every tag holds a
/// window.
fn pick_tag(tags: &[Tag], want: Option<i64>) -> Option<(i64, i64)> {
    let restore = shown(tags)?;
    let index = want
        .and_then(|w| u32::try_from(w).ok())
        .filter(|w| tags.iter().any(|t| t.index == *w))
        .or_else(|| {
            tags.iter()
                .filter(|t| t.client_count == 0)
                .min_by_key(|t| (!t.is_active, t.index))
                .map(|t| t.index)
        })?;
    Some((index.into(), restore.into()))
}

/// Tag this launch gets on monitor `name`, as `(claimed, restore)`, switched to and read back.
/// `None` when the tags cannot be read or the switch did not take: the launch then opens on what
/// the monitor already shows.
pub(crate) fn claim_workspace(name: &str, want: Option<i64>) -> Option<(i64, i64)> {
    let (index, restore) = pick_tag(&tags(name).ok()?, want)?;
    if index != restore {
        let switched = focus_workspace(name, index).and_then(|()| {
            let now = shown(&tags(name)?);
            if now.map(i64::from) != Some(index) {
                bail!("{name} still shows tag {now:?}");
            }
            Ok(())
        });
        if let Err(e) = switched {
            tracing::warn!(
                tag = index, output = %name, error = %format!("{e:#}"),
                "mango: tag switch did not take — this launch opens beside whatever the streamed \
                 output is already showing"
            );
            return None;
        }
    }
    Some((index, restore))
}

/// Show tag `index` on `output`, which takes focus. Nothing when it already does: under
/// `view_current_to_back`, viewing the shown tag goes back to the one before. `view` acts on
/// the focused monitor, so the focus is read back first.
pub(crate) fn focus_workspace(output: &str, index: i64) -> Result<()> {
    let index = u32::try_from(index).with_context(|| format!("{index} is not a tag"))?;
    if shown(&tags(output)?) == Some(index) {
        return Ok(());
    }
    dispatch("focusmon", Some(&monitor_spec(output)), None)?;
    if !ipc_monitors()?.iter().any(|m| m.name == output && m.active) {
        bail!("{output} did not take focus");
    }
    dispatch("view", Some(&index.to_string()), None)
}

/// Turn off the heads an `exclusive` session darkens ([`crate::monitors::disable_for_exclusive`]),
/// journaling each with its mode and position onto `pending` before the first goes dark: mango
/// places a re-enabled head anew. `keep_monitors` stays lit and a managed sibling is never touched.
pub(crate) fn disable_other_heads(ours: &str, pending: &mut Vec<u64>) {
    let keep = crate::policy::prefs().get().keep_monitors;
    let listed = list_monitors();
    // Always `WxH@Hz`: a head at an unknown rate reads `@0`, which mango fills in. A head that
    // lists no mode, a headless one, comes back at this one alone.
    let saved: Vec<(String, String, (i32, i32))> = listed
        .iter()
        .flatten()
        .map(|m| {
            let hz = (f64::from(m.refresh_mhz) / 1000.0).round() as u32;
            let mode = format!("{}x{}@{hz}", m.width, m.height);
            (m.connector.clone(), mode, (m.x, m.y))
        })
        .collect();
    let journaled = |plan: &[String]| {
        let heads = plan
            .iter()
            .map(|name| {
                let s = saved.iter().find(|(n, ..)| n == name);
                Head {
                    name: name.clone(),
                    mode: s.map(|(_, m, _)| m.clone()),
                    at: s.map(|(.., at)| *at),
                }
            })
            .collect();
        pending.push(journal::push("mango", Undo::EnableHeads { heads }))
    };
    crate::monitors::disable_for_exclusive(
        "mango",
        listed,
        ours,
        &keep,
        disable_head,
        focus_output,
        journaled,
    );
}

fn disable_head(name: &str) -> Result<()> {
    output_mgmt::configure(name, Set::Off, None)?;
    if wait_lit(name, false) {
        return Ok(());
    }
    bail!("mango accepted the disable of {name} but it stayed lit")
}

/// Light `h` again. A head with a position was disabled: it is enabled at its journaled mode
/// there, and moved there once more if mango placed it elsewhere. One without was put to sleep and
/// kept its place: it wakes over the IPC. `Ok(false)` when it is gone or already lit.
fn restore_head(h: &Head) -> Result<bool> {
    let Some(now) = desk_head(&h.name)? else {
        return Ok(false);
    };
    if now.lit() {
        return Ok(false);
    }
    match h.at {
        None => dispatch("wakeup_monitor", Some(&monitor_spec(&h.name)), None)?,
        Some(at) => {
            let want = h.mode.as_deref().and_then(crate::policy::parse_mode);
            output_mgmt::configure(&h.name, Set::Listed(want), Some(at))?;
            let placed = desk_head(&h.name)?.map(|d| d.position());
            if placed.is_some_and(|p| p != at) {
                output_mgmt::configure(&h.name, Set::Listed(want), Some(at))?;
            }
        }
    }
    if wait_lit(&h.name, true) {
        return Ok(true);
    }
    bail!(
        "mango accepted the re-enable of {} but it stayed off",
        h.name
    )
}

/// The heads of `heads` still off after their restore.
fn restore_heads(heads: &[Head]) -> Vec<String> {
    let mut dark = Vec::new();
    for h in heads {
        match restore_head(h) {
            Ok(true) => {
                tracing::info!(output = %h.name, "mango: re-lit the output the stream turned off")
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(output = %h.name, error = %format!("{e:#}"), "mango: output not re-lit");
                dark.push(h.name.clone());
            }
        }
    }
    dark
}

/// Take back one journaled desk change: re-light heads, or turn off an output this mango named.
/// Another instance's output died with it.
pub(crate) fn undo(undo: &Undo) -> Result<()> {
    match undo {
        Undo::EnableHeads { heads } => {
            let dark = restore_heads(heads);
            if dark.is_empty() {
                return Ok(());
            }
            bail!(
                "{} still off — `wlr-randr --output <name> --on` re-lights it",
                dark.join(", ")
            )
        }
        Undo::RemoveOutput { name, instance } => {
            if journal::foreign_output("mango", name, instance, &super::instance()) {
                return Ok(());
            }
            super::disable(name)
        }
        Undo::Takeover { .. } => bail!("no undo in mango for {undo:?}"),
    }
}

/// Lit or asleep; `None` for a head turned off, which a wake must never light.
fn powered(d: &DeskHead) -> Option<bool> {
    if d.lit() {
        Some(true)
    } else {
        d.asleep().then_some(false)
    }
}

/// Whether mango reports `name` powered. `None` when turned off, unlisted or unread.
pub(crate) fn head_powered(name: &str) -> Option<bool> {
    powered(&desk_head(name).ok()??)
}

/// Sleep or wake one head over the IPC; asleep, it keeps its place. `Ok(false)` when it is gone
/// or already in that state, so the darken never records a head the operator had off. A failed
/// read is an error: the head stays owed.
pub(crate) fn set_dpms(name: &str, on: bool) -> Result<bool> {
    let Some(d) = desk_head(name)? else {
        return Ok(false);
    };
    if powered(&d) != Some(!on) {
        return Ok(false);
    }
    let verb = if on {
        "wakeup_monitor"
    } else {
        "sleep_monitor"
    };
    dispatch(verb, Some(&monitor_spec(name)), None)?;
    if wait_lit(name, on) {
        return Ok(true);
    }
    bail!("mango accepted {verb} for {name} but nothing changed")
}

/// Sleep every head but a sibling's for a **gamescope** `exclusive` stream
/// ([`crate::monitors::dpms_others`]), with `record(plan)` run before the first goes dark. Nothing
/// without a mango: output management alone also answers on niri and sway.
pub(crate) fn dpms_other_heads(record: impl FnOnce(&[String])) -> Vec<String> {
    if !super::is_available() {
        return Vec::new();
    }
    let heads = list_monitors();
    if let Ok(listed) = &heads {
        record(&crate::monitors::heads_to_darken(listed, "", &[]));
    }
    crate::monitors::dpms_others("mango", heads, |n| set_dpms(n, false))
}

/// Wake exactly `names`, the heads [`dpms_other_heads`] put to sleep.
pub(crate) fn relight_heads(names: &[String]) -> Vec<String> {
    crate::monitors::relight("mango", names, |n| set_dpms(n, true))
}

/// Windows on monitor `output`, or on every monitor with `None`. A window on no monitor is
/// dropped: its head is unproven.
fn parse_toplevels(clients: &[Client], output: Option<&str>) -> Vec<Toplevel> {
    clients
        .iter()
        .filter(|c| !c.monitor.is_empty() && output.is_none_or(|want| want == c.monitor))
        .map(|c| Toplevel {
            id: c.id.to_string(),
            title: c.title.clone(),
            app_id: c.appid.clone(),
            pid: u32::try_from(c.pid).ok().filter(|&p| p > 0),
            focused: c.is_focused,
            fullscreen: c.is_fullscreen,
            workspace: c
                .tags
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("|"),
            output: c.monitor.clone(),
        })
        .collect()
}

/// Windows on monitor `output`, or on every monitor with `None`. Empty on a failed read.
pub(crate) fn toplevels(output: Option<&str>) -> Vec<Toplevel> {
    match clients() {
        Ok(clients) => parse_toplevels(&clients, output),
        Err(e) => {
            tracing::debug!(output = ?output, error = %format!("{e:#}"), "mango: no window list");
            Vec::new()
        }
    }
}

/// A window id as mango numbers them. Anything else is refused before it reaches a dispatch.
fn window_id(id: &str) -> Result<u32> {
    id.parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
        .with_context(|| format!("{id:?} is not a mango window id"))
}

/// Carry window `id` onto monitor `dest`, read back.
pub(crate) fn move_to_output(id: &str, dest: &str) -> Result<()> {
    let n = window_id(id)?;
    dispatch("tagmon", Some(&monitor_spec(dest)), Some(n))?;
    if toplevels(Some(dest)).iter().any(|w| w.id == id) {
        return Ok(());
    }
    bail!("mango accepted the move of window {id} but it is not on {dest}")
}

/// Move every window on `output` onto a lit desk head, the focused one first. mango then
/// remembers that head for them, so `output` re-enabled pulls none back. Nothing without a desk
/// head: mango moves them itself on the disable.
pub(super) fn evict(output: &str) {
    let Ok(desk) = desk() else { return };
    let Some(dest) = desk
        .iter()
        .filter(|d| d.lit() && !crate::hyprland::is_managed_output(d.name()))
        .max_by_key(|d| d.ipc.as_ref().is_some_and(|m| m.active))
    else {
        return;
    };
    for w in toplevels(Some(output)) {
        let moved = window_id(&w.id)
            .and_then(|n| dispatch("tagmon", Some(&monitor_spec(dest.name())), Some(n)));
        if let Err(e) = moved {
            tracing::debug!(window = %w.id, error = %format!("{e:#}"), "mango: window not moved off");
        }
    }
}

/// Run one verb on window `id`. A full-screen window is left alone for `Fullscreen`.
pub(crate) fn window_action(verb: WindowVerb, id: &str) -> Result<()> {
    let n = window_id(id)?;
    match verb {
        WindowVerb::Focus => dispatch("focusid", None, Some(n)),
        WindowVerb::Fullscreen => {
            if toplevels(None).iter().any(|w| w.id == id && w.fullscreen) {
                return Ok(());
            }
            dispatch("togglefullscreen", None, Some(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    fn tag(index: u32, is_active: bool, client_count: u32) -> Tag {
        Tag {
            index,
            is_active,
            client_count,
        }
    }

    /// The shown tag when it is empty; else the lowest empty one; an earlier claim while it exists.
    #[test]
    fn a_launch_takes_an_empty_tag_and_remembers_the_shown_one() {
        let busy = [
            tag(1, true, 2),
            tag(2, false, 1),
            tag(3, false, 0),
            tag(4, false, 0),
        ];
        assert_eq!(pick_tag(&busy, None), Some((3, 1)));
        assert_eq!(pick_tag(&busy, Some(2)), Some((2, 1)));
        assert_eq!(pick_tag(&busy, Some(12)), Some((3, 1)));
        let calm = [tag(1, false, 0), tag(2, true, 0)];
        assert_eq!(pick_tag(&calm, None), Some((2, 2)));
        assert_eq!(pick_tag(&[tag(1, true, 1)], None), None);
        assert_eq!(pick_tag(&[tag(1, false, 0)], None), None, "nothing shown");
    }

    fn head_at(name: &str, enabled: bool, ipc: Option<(i32, i32, i32, i32)>) -> DeskHead {
        DeskHead {
            head: HeadState {
                name: name.into(),
                enabled,
                mode: enabled.then_some((1920, 1080, 60_000)),
                scale: 1.5,
                ..HeadState::default()
            },
            ipc: ipc.map(|(x, y, width, height)| IpcMonitor {
                name: name.into(),
                x,
                y,
                width,
                height,
                ..IpcMonitor::default()
            }),
        }
    }

    /// Asleep is listed lit at its layout box; turned off is listed off; position comes from the
    /// IPC, since output management puts every dark head at the origin.
    #[test]
    fn a_sleeping_head_is_listed_lit_and_a_disabled_one_off() {
        let lit = head_at("DP-1", true, Some((0, 0, 1280, 720)));
        let asleep = head_at("DP-2", false, Some((1280, 0, 1280, 720)));
        let off = head_at("DP-3", false, Some((0, 0, 0, 0)));
        assert!(lit.monitor().enabled && !lit.asleep());
        let m = asleep.monitor();
        assert!(asleep.asleep() && m.enabled);
        assert_eq!(
            (m.x, m.y, m.width, m.height, m.refresh_mhz),
            (1280, 0, 1920, 1080, 0)
        );
        assert!(!off.asleep() && !off.monitor().enabled);
        assert!(!head_at("DP-4", false, None).monitor().enabled);
    }

    fn listed() -> Vec<Client> {
        from_value(json!([
            {"id":4,"pid":77,"title":"Game","appid":"steam_app_1","monitor":"PF-3","tags":[2],
             "is_focused":true,"is_fullscreen":false},
            {"id":5,"pid":0,"title":"Notes","appid":"foot","monitor":"DP-1","tags":[1,3]},
            {"id":6,"title":"parked","monitor":""}
        ]))
        .unwrap()
    }

    #[test]
    fn a_window_list_holds_the_streamed_output_and_nothing_else() {
        let on = parse_toplevels(&listed(), Some("PF-3"));
        assert_eq!(on.len(), 1);
        let w = &on[0];
        assert_eq!(
            (w.id.as_str(), w.app_id.as_str(), w.pid),
            ("4", "steam_app_1", Some(77))
        );
        assert!(w.focused && !w.fullscreen);
        let all = parse_toplevels(&listed(), None);
        assert_eq!(all.len(), 2, "a window on no monitor is dropped");
        assert_eq!((all[1].workspace.as_str(), all[1].pid), ("1|3", None));
    }

    /// A window id reaches a dispatch only as mango's own number.
    #[test]
    fn a_window_verb_refuses_an_id_that_is_not_a_mango_number() {
        assert_eq!(window_id("4").unwrap(), 4);
        for id in ["0", "-1", "4 spawn,x", "4,5", "", "a"] {
            assert!(window_id(id).is_err(), "{id:?}");
        }
    }

    #[test]
    fn an_error_reply_is_not_decoded() {
        let err = field::<Vec<Tag>>(json!({"error":"monitor not found"}), "tags").unwrap_err();
        assert!(format!("{err:#}").contains("monitor not found"));
        let ok: Vec<Tag> = field(
            json!({"monitor":"PF-3","tags":[{"index":1,"is_active":true}]}),
            "tags",
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
    }
}
