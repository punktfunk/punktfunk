//! Settings → Input → Shortcuts: what a press does while a stream runs. One row per binding
//! of [`pf_client_core::shortcuts`], under the device it is pressed on, and an Add row for
//! each device; touch and remote rows are listed, fixed. A binding opens
//! [`ShortcutRowScreen`]: its action (the dial's [`Picker`]), its buttons or keys, press or
//! hold, remove.
//!
//! Writes go through a [`Scope`]: the device's settings, or a preset's. The page reads the
//! blob each frame, so a save on the row screen shows when it pops.
//!
//! A pad is recorded from the input test's feed ([`crate::model::ConsoleCmd::PadTest`]): the
//! chord is what was held when the first button lifted. Only shells that feed it record
//! ([`super::players::can_test`]); elsewhere a grid of buttons stands in. The console's key
//! events carry no letters or modifiers, so a key is always chosen on the key grid.

use super::preset::Scope;
use super::ring_editor::{ring_platform, Picked, Picker};
use super::shortcut_editor::{KeyTray, TrayMsg};
use crate::glyphs::{self, GlyphStyle, Hint, HintKey};
use crate::model::PadTestState;
use crate::platform::Platform;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::theme::{fill, stroke, Fonts, W};
use crate::widgets::{blurb, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{
    catalogue, key_legend, CatalogueEntry, CatalogueGroup, GroupId, OverlayConfig, SlotId,
};
use pf_client_core::shortcuts::{
    button_bit, defaults, resolve, Action, Binding, Client, Conflict, Device, Kind, Press,
    Shortcuts, Trigger, MODIFIERS,
};
use pf_client_core::trust::Settings;
use pf_l10n::{client as c, common, console as con};
use skia_safe::{Canvas, Color4f, RRect, Rect};
use std::collections::BTreeSet;
use std::sync::Mutex;

/// This build's bindings family. The console is built for the device it runs on, so a Mac's
/// build is what tells it from an iPad.
pub(crate) fn client(platform: Platform) -> Client {
    match platform {
        Platform::Desktop => Client::Desktop,
        Platform::Android => Client::Android,
        Platform::Apple if cfg!(target_os = "macos") => Client::Mac,
        Platform::Apple => Client::Apple,
        Platform::WebOS => Client::WebOS,
        Platform::Web => Client::Web,
        Platform::Tizen => Client::Tizen,
        Platform::Ps5 => Client::Ps5,
        Platform::Switch => Client::Switch,
    }
}

/// The desktop session shares this process, so its gamescope is ours to see. No other
/// client's defaults depend on Game Mode.
fn game_mode(platform: Platform) -> bool {
    platform == Platform::Desktop && pf_client_core::gamescope::under_gamescope()
}

/// The bindings `s` streams with: its blob, or this client's defaults.
fn bindings(s: &Settings, client: Client, platform: Platform) -> Shortcuts {
    resolve(&s.shortcuts, client, game_mode(platform), &s.guide_gesture)
}

/// What a shortcut can do: the dial's catalogue with `s`'s key combinations, Quick actions
/// itself first under Input, and no empty slot.
fn actions(s: &Settings, platform: Platform) -> Vec<CatalogueGroup> {
    let ring = ring_platform(platform);
    let mut groups = catalogue(&OverlayConfig::parse(&s.overlay_actions, ring), ring);
    groups.retain(|g| g.id != GroupId::Empty);
    if let Some(input) = groups.iter_mut().find(|g| g.id == GroupId::Input) {
        let open = CatalogueEntry {
            id: Action::Ring.id(),
            label: c::ring_open_ring().into(),
            note: String::new(),
        };
        input.entries.insert(0, open);
    }
    groups
}

/// An action's name; `None` where this device has no such action.
fn action_label(groups: &[CatalogueGroup], action: &Action) -> Option<String> {
    let id = action.id();
    (groups.iter().flat_map(|g| &g.entries))
        .find(|e| e.id == id)
        .map(|e| e.label.clone())
}

/// [`RowSpec::header`] and [`Press`] hold `&'static str`: a heading or chord is kept for the
/// process, one string per distinct text.
fn keep(s: &str) -> &'static str {
    static KEPT: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(k) = kept.get(s) {
        return k;
    }
    let k: &'static str = Box::leak(s.to_owned().into_boxed_str());
    kept.insert(k);
    k
}

/// Whether `client` lists keys at all, so its Add row for keys stays when none is bound.
fn has_keyboard(client: Client, touchscreen: bool) -> bool {
    (defaults(client, false, "auto")
        .rows(client, touchscreen)
        .iter())
    .any(|g| g.kind == Kind::Keyboard)
}

/// The second press an [`Press::Either`] row folds in, as `rows` pairs them: the next press of
/// the same action on the same device.
fn partner(list: &Shortcuts, i: usize) -> Option<usize> {
    let b = &list.bindings[i];
    (i + 1..list.bindings.len()).find(|&j| {
        let o = &list.bindings[j];
        o.action == b.action && o.trigger.device() == b.trigger.device() && !o.trigger.hold()
    })
}

/// An action this device can't run: an id from a newer client, or a key combination that
/// is gone from the dial.
fn missing(action: &Action, cfg: &OverlayConfig) -> bool {
    match action {
        Action::Unknown(_) => true,
        Action::Slot(SlotId::Shortcut(id)) => !cfg.shortcuts.iter().any(|s| &s.id == id),
        Action::Slot(_) | Action::Ring => false,
    }
}

/// A binding's row: `label` is its item's text, the chord on the right.
fn binding_row(b: &Binding, label: String, cfg: &OverlayConfig, client: Client) -> RowSpec {
    let mut r = RowSpec::field(label, b.trigger.label(client), "");
    if missing(&b.action, cfg) {
        r.enabled = false;
        r.note = Some(c::shortcuts_not_on_device().into());
    }
    r
}

/// The pad family's style: the first pad's, else Xbox letters.
fn pad_style(ctx: &Ctx) -> GlyphStyle {
    ctx.pads
        .first()
        .map_or(GlyphStyle::Letters, |p| GlyphStyle::from_pref(Some(p.pref)))
}

// --- the page --------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    Binding(usize),
    Fixed,
    Add(Device),
    Reset,
}

pub(crate) struct ShortcutsScreen {
    scope: Scope,
    client: Client,
    list: MenuList,
    /// An iPhone's or iPad's keys wait for a keyboard, which can arrive while the page is up.
    keys_need_keyboard: bool,
    keyboard: bool,
    reset_armed: bool,
}

impl ShortcutsScreen {
    pub(crate) fn new(ctx: &Ctx, scope: Scope) -> ShortcutsScreen {
        ShortcutsScreen::with_client(client(ctx.device.platform), scope)
    }

    fn with_client(client: Client, scope: Scope) -> ShortcutsScreen {
        ShortcutsScreen {
            scope,
            client,
            list: MenuList::new(),
            keys_need_keyboard: client == Client::Apple,
            keyboard: false,
            reset_armed: false,
        }
    }

    /// Whether a keyboard is attached, from the shell each frame.
    pub(crate) fn set_keyboard(&mut self, attached: bool) {
        self.keyboard = attached;
    }

    /// Every row: each device's group under its heading, each with its Add, then Reset.
    fn lines(&self, ctx: &Ctx) -> Vec<(Line, RowSpec)> {
        let s = self.scope.settings(ctx);
        let platform = ctx.device.platform;
        let list = bindings(&s, self.client, platform);
        let cfg = OverlayConfig::parse(&s.overlay_actions, ring_platform(platform));
        let touchscreen = !ctx.device.tv;
        let keys_shown = !self.keys_need_keyboard || self.keyboard;
        let add = |device| (Line::Add(device), RowSpec::action(c::shortcuts_add(), true));
        // A device with nothing bound has no group of its own; its Add keeps a heading.
        let mut keys_owed = keys_shown && has_keyboard(self.client, touchscreen);
        let mut out = Vec::new();
        for g in list.rows_in(&cfg, self.client, touchscreen) {
            if g.kind == Kind::Keyboard && !keys_shown {
                continue;
            }
            if keys_owed && matches!(g.kind, Kind::Touch | Kind::Pad) {
                let (line, row) = add(Device::Keyboard);
                out.push((line, row.with_header(c::controls_group_keyboard())));
            }
            keys_owed &= g.kind == Kind::Remote;
            let mut rows = Vec::new();
            for item in &g.items {
                let Some(i) = item.binding else {
                    let row = RowSpec::field(item.text(), item.keys.to_string(), "");
                    rows.push((Line::Fixed, row.locked(c::shortcuts_fixed())));
                    continue;
                };
                let row = |b| binding_row(b, item.text().into_owned(), &cfg, self.client);
                rows.push((Line::Binding(i), row(&list.bindings[i])));
                // An Either row is two bindings; each gets its own row to open.
                if let (Press::Either(..), Some(j)) = (item.keys, partner(&list, i)) {
                    rows.push((Line::Binding(j), row(&list.bindings[j])));
                }
            }
            match g.kind {
                Kind::Pad => rows.push(add(Device::Pad)),
                Kind::Keyboard => rows.push(add(Device::Keyboard)),
                Kind::Touch | Kind::Remote => {}
            }
            if let Some((_, first)) = rows.first_mut() {
                first.header = Some(keep(&g.heading()));
            }
            out.extend(rows);
        }
        let reset = if self.reset_armed {
            c::ring_reset_confirm()
        } else {
            c::shortcuts_reset()
        };
        out.push((Line::Reset, RowSpec::action(reset, true)));
        out
    }

    fn activate(&mut self, line: Line, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let armed = std::mem::take(&mut self.reset_armed);
        let row = match line {
            Line::Fixed => return Some(MenuPulse::Boundary),
            Line::Reset if !armed => {
                self.reset_armed = true;
                return Some(MenuPulse::Boundary);
            }
            Line::Reset => {
                self.scope
                    .write("shortcuts", ctx, fx, |s| s.shortcuts.clear());
                return Some(MenuPulse::Confirm);
            }
            Line::Binding(i) => ShortcutRowScreen::edit(ctx, self.scope.clone(), self.client, i),
            Line::Add(device) => {
                ShortcutRowScreen::add(ctx, self.scope.clone(), self.client, device)
            }
        };
        fx.push(Screen::ShortcutRow(Box::new(row)));
        Some(MenuPulse::Confirm)
    }
}

impl ScreenView for ShortcutsScreen {
    fn title(&self) -> String {
        c::player_shortcuts().into()
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        if matches!(ev, MenuEvent::Move(_)) {
            self.reset_armed = false;
        }
        let lines = self.lines(ctx);
        let (msg, pulse) = self.list.menu(ev, lines.len());
        match (msg, lines.get(self.list.cursor)) {
            (ListMsg::Activate, Some(&(line, _))) => self.activate(line, ctx, fx),
            (ListMsg::Adjust(_), _) => Some(MenuPulse::Boundary),
            _ => pulse,
        }
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let lines = self.lines(ctx);
        let (msg, pulse) = self.list.pointer(p, lines.len());
        if let (ListMsg::Activate, Some(&(line, _))) = (&msg, lines.get(self.list.cursor)) {
            self.activate(line, ctx, fx);
            return true;
        }
        msg != ListMsg::None || pulse.is_some()
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let line = self.lines(ctx).get(self.list.cursor).map(|(l, _)| *l);
        let confirm = match line {
            Some(Line::Binding(_)) => Some(common::common_edit()),
            Some(Line::Add(_)) => Some(common::common_add()),
            Some(Line::Reset) => Some(common::common_reset()),
            Some(Line::Fixed) | None => None,
        };
        (confirm.map(|l| Hint::new(HintKey::Confirm, l)).into_iter())
            .chain([Hint::new(HintKey::Back, common::common_back())])
            .collect()
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let ui = ctx.ui;
        let below = blurb(canvas, ui, fonts, c::shortcuts_lead(), rect, k);
        let rows: Vec<RowSpec> = self.lines(ctx).into_iter().map(|(_, r)| r).collect();
        self.list.cursor = self.list.cursor.min(rows.len().saturating_sub(1));
        let list_rect = Rect::from_ltrb(rect.left, below.top, rect.right, rect.bottom);
        self.list
            .render(canvas, ui, list_rect, &rows, fonts, k, dt, true);
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

// --- one shortcut ----------------------------------------------------------------------------

/// Every pad button by its stored name, laid out by where it sits on a pad.
const PAD_GRID: [&[&str]; 5] = [
    &["a", "b", "x", "y"],
    &["lb", "rb", "ls", "rs"],
    &["up", "down", "left", "right"],
    &["select", "start", "guide", "touchpad"],
    &["misc1", "misc2", "paddle1", "paddle2", "paddle3", "paddle4"],
];

/// A pad chord as it is pressed: what was held when the first button lifted.
#[derive(Default)]
struct Recorder {
    /// Buttons count once every button is up, or once a reading lacks A: an A still held is
    /// the press that opened the recorder.
    armed: bool,
    /// In the order they went down.
    chord: Vec<u32>,
}

impl Recorder {
    /// The chord, once a reading lets go of any of it.
    fn feed(&mut self, held: &[String]) -> Option<Vec<u32>> {
        let now: Vec<u32> = held.iter().filter_map(|n| button_bit(n)).collect();
        if !self.armed {
            let a = button_bit("a");
            self.armed = now.is_empty() || !now.iter().any(|b| Some(*b) == a);
            if !self.armed {
                return None;
            }
        }
        if self.chord.iter().all(|b| now.contains(b)) {
            for b in now {
                if !self.chord.contains(&b) {
                    self.chord.push(b);
                }
            }
            return None;
        }
        Some(std::mem::take(&mut self.chord))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Does,
    Press,
    /// The button grid, where Press records.
    Choose,
    /// [`MODIFIERS`]`[i]`, on a key.
    Modifier(usize),
    FiresOn,
    Remove,
    /// The answers while [`ShortcutRowScreen::replace`] is asked.
    Replace,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tray {
    Keys,
    Buttons,
}

/// A new binding's action until one is picked.
fn no_action() -> Action {
    Action::Unknown(String::new())
}

pub(crate) struct ShortcutRowScreen {
    scope: Scope,
    client: Client,
    /// Whose button names the grid and the recorder write: the pad in hand's family.
    names: Client,
    /// The binding this screen edits; `None` until a new one is first saved.
    index: Option<usize>,
    draft: Binding,
    /// Why the draft is not saved.
    line: Option<String>,
    /// The binding the draft would replace, asked first.
    replace: Option<usize>,
    /// The action's name, as the last frame read it.
    title: String,
    list: MenuList,
    picker: Option<Picker>,
    tray: Option<Tray>,
    keys: KeyTray,
    buttons: KeyTray,
    recorder: Option<Recorder>,
}

impl ShortcutRowScreen {
    fn new(ctx: &Ctx, scope: Scope, client: Client, index: Option<usize>, draft: Binding) -> Self {
        // A DualSense names its buttons by shape and a Switch pad by its own letters; the PS5
        // and Switch clients' chords already do.
        let names = match pad_style(ctx) {
            GlyphStyle::Shapes => Client::Ps5,
            GlyphStyle::Nintendo => Client::Switch,
            _ => client,
        };
        let legend = move |n: &str| {
            let buttons = button_bit(n).into_iter().collect();
            Trigger::Pad {
                buttons,
                hold: false,
            }
            .chord(names)
        };
        ShortcutRowScreen {
            scope,
            client,
            names,
            index,
            draft,
            line: None,
            replace: None,
            title: String::new(),
            list: MenuList::new(),
            picker: None,
            tray: None,
            keys: KeyTray::keys(),
            buttons: KeyTray::new(&PAD_GRID, legend),
            recorder: None,
        }
    }

    /// Binding `index` of the scope's list.
    pub(crate) fn edit(ctx: &Ctx, scope: Scope, client: Client, index: usize) -> Self {
        let list = bindings(&scope.settings(ctx), client, ctx.device.platform);
        let draft = list.bindings.get(index).cloned().unwrap_or(Binding {
            trigger: Trigger::Pad {
                buttons: Vec::new(),
                hold: false,
            },
            action: no_action(),
        });
        Self::new(ctx, scope, client, Some(index), draft)
    }

    /// A new binding on `device`: the action first, then its buttons or keys.
    pub(crate) fn add(ctx: &Ctx, scope: Scope, client: Client, device: Device) -> Self {
        let trigger = match device {
            Device::Pad => Trigger::Pad {
                buttons: Vec::new(),
                hold: false,
            },
            Device::Keyboard => Trigger::Key {
                modifiers: Vec::new(),
                key: String::new(),
            },
        };
        let draft = Binding {
            trigger,
            action: no_action(),
        };
        let mut s = Self::new(ctx, scope, client, None, draft);
        s.open_picker(ctx);
        s
    }

    /// Recording: the shell feeds [`Self::pad_test`] and keeps the pad out of menu moves.
    pub(crate) fn recording(&self) -> bool {
        self.recorder.is_some()
    }

    #[cfg(test)]
    pub(crate) fn start_recording(&mut self) {
        self.recorder = Some(Recorder::default());
    }

    /// A reading of the input test while recording.
    pub(crate) fn pad_test(&mut self, state: &PadTestState, ctx: &mut Ctx, fx: &mut Outbox) {
        let Some(buttons) = self.recorder.as_mut().and_then(|r| r.feed(&state.held)) else {
            return;
        };
        self.recorder = None;
        let hold = self.draft.trigger.hold() || buttons.len() == 1;
        self.draft.trigger = Trigger::Pad { buttons, hold };
        self.commit(None, ctx, fx);
    }

    fn can_record(ctx: &Ctx) -> bool {
        super::players::can_test(ctx.device.platform) && !ctx.pads.is_empty()
    }

    fn list_now(&self, ctx: &Ctx) -> Shortcuts {
        bindings(&self.scope.settings(ctx), self.client, ctx.device.platform)
    }

    fn fields(&self, ctx: &Ctx) -> Vec<Field> {
        if self.replace.is_some() {
            return vec![Field::Replace, Field::Cancel];
        }
        let mut f = vec![Field::Does, Field::Press];
        match self.draft.trigger {
            Trigger::Pad { .. } if Self::can_record(ctx) => f.push(Field::Choose),
            Trigger::Pad { .. } => {}
            Trigger::Key { .. } => f.extend((0..MODIFIERS.len()).map(Field::Modifier)),
        }
        f.push(Field::FiresOn);
        if self.index.is_some() {
            f.push(Field::Remove);
        }
        f
    }

    fn spec(&self, field: Field, ctx: &Ctx) -> RowSpec {
        let (press, hold) = (c::shortcuts_fires_press(), c::shortcuts_fires_hold());
        match field {
            Field::Does => {
                let s = self.scope.settings(ctx);
                let cfg =
                    OverlayConfig::parse(&s.overlay_actions, ring_platform(ctx.device.platform));
                let mut r = RowSpec::field(
                    c::shortcuts_does(),
                    self.action_name(ctx),
                    con::console_shortcut_key_choose(),
                );
                if self.draft.action != no_action() && missing(&self.draft.action, &cfg) {
                    r.note = Some(c::shortcuts_not_on_device().into());
                }
                r
            }
            Field::Press => {
                let chord = match &self.draft.trigger {
                    Trigger::Pad { buttons, .. } if buttons.is_empty() => String::new(),
                    Trigger::Key { key, .. } if key.is_empty() => String::new(),
                    t => t.chord(self.client),
                };
                let mut r = RowSpec::field(
                    c::shortcuts_press(),
                    chord,
                    con::console_shortcut_key_choose(),
                );
                r.note = self.line.clone();
                r
            }
            Field::Choose => RowSpec::action(c::shortcuts_choose(), true),
            Field::Modifier(i) => {
                let on = matches!(&self.draft.trigger,
                    Trigger::Key { modifiers, .. } if modifiers.iter().any(|m| m == MODIFIERS[i]));
                let value = if on {
                    con::console_shortcut_modifier_on()
                } else {
                    con::console_shortcut_modifier_off()
                };
                RowSpec::choice(key_legend(MODIFIERS[i]), value)
            }
            Field::FiresOn => match &self.draft.trigger {
                // The engine has no keyboard holds.
                Trigger::Key { .. } => RowSpec::field(c::shortcuts_fires_on(), press.into(), ""),
                Trigger::Pad { buttons, .. } if buttons.len() == 1 => {
                    RowSpec::field(c::shortcuts_fires_on(), hold.into(), "")
                        .locked(c::shortcuts_one_button_hold())
                }
                Trigger::Pad { hold: on, .. } => {
                    RowSpec::choice(c::shortcuts_fires_on(), if *on { hold } else { press })
                }
            },
            Field::Remove => RowSpec::action(c::shortcuts_remove(), true),
            Field::Replace => RowSpec::action(c::shortcuts_replace(), true),
            Field::Cancel => RowSpec::action(common::common_cancel(), true),
        }
    }

    /// The action's name, its id where this device has none, empty until one is picked.
    fn action_name(&self, ctx: &Ctx) -> String {
        if self.draft.action == no_action() {
            return String::new();
        }
        let groups = actions(&self.scope.settings(ctx), ctx.device.platform);
        action_label(&groups, &self.draft.action).unwrap_or_else(|| self.draft.action.id())
    }

    fn open_picker(&mut self, ctx: &Ctx) {
        let groups = actions(&self.scope.settings(ctx), ctx.device.platform);
        self.picker = Some(Picker::new(groups, &self.draft.action.id()));
    }

    /// The buttons or keys of a new binding, once its action is picked: recorded where the
    /// shell can, else chosen.
    fn ask_trigger(&mut self, ctx: &Ctx) {
        match self.draft.trigger {
            Trigger::Pad { .. } if Self::can_record(ctx) => {
                self.recorder = Some(Recorder::default());
            }
            Trigger::Pad { .. } => self.open_tray(Tray::Buttons),
            Trigger::Key { .. } => self.open_tray(Tray::Keys),
        }
    }

    fn open_tray(&mut self, tray: Tray) {
        match (&self.draft.trigger, tray) {
            (Trigger::Key { key, .. }, Tray::Keys) => self.keys.seat_on(Some(key)),
            _ => self.buttons.seat_on(None),
        }
        self.tray = Some(tray);
    }

    /// Saves the draft when the list takes it. A changed trigger is checked against the
    /// others first; `drop` is a binding the player chose to replace.
    fn commit(&mut self, drop: Option<usize>, ctx: &mut Ctx, fx: &mut Outbox) {
        self.line = None;
        self.replace = None;
        let empty =
            matches!(&self.draft.trigger, Trigger::Pad { buttons, .. } if buttons.is_empty());
        if empty || self.draft.action == no_action() {
            return;
        }
        let mut list = self.list_now(ctx);
        let mut index = self.index.filter(|&i| i < list.bindings.len());
        if let Some(r) = drop.filter(|&r| r < list.bindings.len()) {
            list.bindings.remove(r);
            index = index.and_then(|i| match i.cmp(&r) {
                std::cmp::Ordering::Less => Some(i),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(i - 1),
            });
        }
        let same = index.is_some_and(|i| list.bindings[i].trigger == self.draft.trigger);
        if !same {
            let mut others = list.clone();
            if let Some(i) = index {
                others.bindings.remove(i);
            }
            if let Err(conflict) = others.validate(&self.draft) {
                self.refuse(conflict, &others, index);
                return;
            }
        }
        match index {
            Some(i) => list.bindings[i] = self.draft.clone(),
            None => list.bindings.push(self.draft.clone()),
        }
        self.index = Some(index.unwrap_or(list.bindings.len() - 1));
        let json = list.to_json();
        self.scope
            .write("shortcuts", ctx, fx, move |s| s.shortcuts = json);
    }

    /// What a conflict leaves on screen. `others` is the list without the draft's own row,
    /// which sits at `index` in the whole list.
    fn refuse(&mut self, conflict: Conflict, others: &Shortcuts, index: Option<usize>) {
        let label = |t: &Trigger| t.label(self.client);
        self.line = Some(match conflict {
            Conflict::Identical(j) => {
                self.replace = Some(match index {
                    Some(i) if j >= i => j + 1,
                    _ => j,
                });
                self.list.jump_to(0);
                return;
            }
            Conflict::PressSubset(j) => {
                let (mine, theirs) = (&self.draft.trigger, &others.bindings[j].trigger);
                let (first, second) = if mine.ids().len() < theirs.ids().len() {
                    (mine, theirs)
                } else {
                    (theirs, mine)
                };
                c::shortcuts_subset(label(first), label(second))
            }
            Conflict::SinglePress => c::shortcuts_one_button_hold().into(),
            Conflict::NoModifier => c::shortcuts_needs_modifier().into(),
            Conflict::NoKey => c::ring_shortcut_pick_key_first().into(),
        });
    }

    fn remove(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        let mut list = self.list_now(ctx);
        if let Some(i) = self.index.filter(|&i| i < list.bindings.len()) {
            list.bindings.remove(i);
            let json = list.to_json();
            self.scope
                .write("shortcuts", ctx, fx, move |s| s.shortcuts = json);
        }
        fx.pop();
    }

    /// The question for [`Self::replace`]: what the other binding's trigger already does.
    fn question(&self, ctx: &Ctx) -> Option<String> {
        let r = self.replace?;
        let s = self.scope.settings(ctx);
        let other = self.list_now(ctx).bindings.get(r)?.action.clone();
        let groups = actions(&s, ctx.device.platform);
        let action = action_label(&groups, &other).unwrap_or_else(|| other.id());
        let keys = self.draft.trigger.label(self.client);
        Some(c::shortcuts_identical_replace(keys, action))
    }

    fn toggle_modifier(&mut self, i: usize, on: Option<bool>) -> bool {
        let Trigger::Key { modifiers, .. } = &mut self.draft.trigger else {
            return false;
        };
        let was = modifiers.iter().any(|m| m == MODIFIERS[i]);
        let want = on.unwrap_or(!was);
        if want == was {
            return false;
        }
        let mut set: Vec<&str> = modifiers.iter().map(String::as_str).collect();
        if want {
            set.push(MODIFIERS[i]);
        } else {
            set.retain(|m| *m != MODIFIERS[i]);
        }
        *modifiers = (MODIFIERS.iter())
            .filter(|m| set.contains(m))
            .map(|m| m.to_string())
            .collect();
        true
    }

    /// Press or Hold. One button is always a hold, and a key always a press.
    fn set_hold(&mut self, hold: Option<bool>) -> bool {
        let Trigger::Pad { buttons, hold: was } = &mut self.draft.trigger else {
            return false;
        };
        let want = hold.unwrap_or(!*was);
        if buttons.len() == 1 || want == *was {
            return false;
        }
        *was = want;
        true
    }

    fn activate(&mut self, field: Field, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match field {
            Field::Does => self.open_picker(ctx),
            Field::Press => match self.draft.trigger {
                Trigger::Pad { .. } if Self::can_record(ctx) => {
                    self.recorder = Some(Recorder::default());
                }
                Trigger::Pad { .. } => self.open_tray(Tray::Buttons),
                Trigger::Key { .. } => self.open_tray(Tray::Keys),
            },
            Field::Choose => self.open_tray(Tray::Buttons),
            Field::Modifier(i) => {
                self.toggle_modifier(i, None);
                self.commit(None, ctx, fx);
            }
            Field::FiresOn => {
                if !self.set_hold(None) {
                    return Some(MenuPulse::Boundary);
                }
                self.commit(None, ctx, fx);
            }
            Field::Remove => self.remove(ctx, fx),
            Field::Replace => {
                self.commit(self.replace, ctx, fx);
                if self.replace.is_none() {
                    self.list.jump_to(1);
                }
            }
            Field::Cancel => {
                self.replace = None;
                self.revert(ctx);
            }
        }
        Some(MenuPulse::Confirm)
    }

    /// Leaves the question on the trigger the list holds for this row, at Press.
    fn revert(&mut self, ctx: &Ctx) {
        self.list.jump_to(1);
        if let Some(b) = self
            .index
            .and_then(|i| self.list_now(ctx).bindings.get(i).cloned())
        {
            self.draft.trigger = b.trigger;
        }
    }

    fn adjust(&mut self, field: Field, delta: i32, ctx: &mut Ctx, fx: &mut Outbox) -> MenuPulse {
        let moved = match field {
            Field::Modifier(i) => self.toggle_modifier(i, Some(delta > 0)),
            Field::FiresOn => self.set_hold(Some(delta > 0)),
            _ => false,
        };
        if !moved {
            return MenuPulse::Boundary;
        }
        self.commit(None, ctx, fx);
        MenuPulse::Move
    }

    fn picked(&mut self, picked: Picked, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match picked {
            Picked::Stay(pulse) => return pulse,
            // Leaving the first pick of a new binding leaves the binding.
            Picked::Closed if self.draft.action == no_action() => fx.pop(),
            Picked::Closed => {}
            Picked::Chose(id) => {
                let new = self.draft.action == no_action();
                self.draft.action = Action::parse(&id);
                self.commit(None, ctx, fx);
                if new {
                    self.ask_trigger(ctx);
                }
            }
        }
        self.picker = None;
        Some(MenuPulse::Confirm)
    }

    fn tray_msg(&mut self, msg: TrayMsg, ctx: &mut Ctx, fx: &mut Outbox) {
        match (msg, self.tray) {
            (TrayMsg::None, _) | (_, None) => {}
            (TrayMsg::Close, Some(Tray::Buttons)) => {
                self.tray = None;
                self.commit(None, ctx, fx);
            }
            (TrayMsg::Close, Some(Tray::Keys)) => self.tray = None,
            (TrayMsg::Pick(name), Some(Tray::Keys)) => {
                if let Trigger::Key { key, .. } = &mut self.draft.trigger {
                    *key = name.into();
                }
                self.tray = None;
                self.commit(None, ctx, fx);
            }
            (TrayMsg::Pick(name), Some(Tray::Buttons)) => {
                let Some(bit) = button_bit(name) else { return };
                if let Trigger::Pad { buttons, hold } = &mut self.draft.trigger {
                    match buttons.iter().position(|b| *b == bit) {
                        Some(at) => {
                            buttons.remove(at);
                        }
                        None => buttons.push(bit),
                    }
                    *hold |= buttons.len() == 1;
                }
            }
        }
    }

    fn chosen_buttons(&self) -> Vec<&'static str> {
        let Trigger::Pad { buttons, .. } = &self.draft.trigger else {
            return Vec::new();
        };
        PAD_GRID
            .iter()
            .flat_map(|r| r.iter().copied())
            .filter(|n| button_bit(n).is_some_and(|b| buttons.contains(&b)))
            .collect()
    }

    /// The prompt and the chord so far, on a card over a scrim.
    fn draw_recorder(&self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, ctx: &Ctx) {
        let ui = ctx.ui;
        let kf = k as f32;
        canvas.draw_rect(
            rect.with_outset((2000.0 * kf, 2000.0 * kf)),
            &fill(Color4f::new(0.0, 0.0, 0.0, 0.45)),
        );
        let w = (560.0 * kf).min(rect.width() * 0.92);
        let h = 170.0 * kf;
        let card = Rect::from_xywh(rect.center_x() - w / 2.0, rect.center_y() - h / 2.0, w, h);
        let rr = RRect::new_rect_xy(card, 16.0 * kf, 16.0 * kf);
        canvas.draw_rrect(rr, &fill(Color4f::new(0.06, 0.055, 0.1, 0.96)));
        canvas.draw_rrect(rr, &stroke(ui.fg(0.18), kf));
        let prompt = c::shortcuts_record_prompt();
        let size = 18.0 * k;
        let pw = f64::from(fonts.measure(prompt, W::SemiBold, size));
        let cx = f64::from(card.center_x());
        let top = f64::from(card.top);
        fonts.draw_clipped(
            canvas,
            prompt,
            (cx - pw / 2.0).max(f64::from(card.left) + 16.0 * k),
            top + 50.0 * k,
            W::SemiBold,
            size,
            ui.fg(0.95),
            f64::from(w) - 32.0 * k,
        );
        let chord = self.recorder.as_ref().map(|r| r.chord.clone());
        let Some(buttons) = chord.filter(|b| !b.is_empty()) else {
            return;
        };
        let text = Trigger::Pad {
            buttons,
            hold: false,
        }
        .chord(self.names);
        let press = Press::Keys(keep(&text));
        // Nintendo letters are already the pad's own: the glyph style must not swap them again.
        let style = match pad_style(ctx) {
            GlyphStyle::Nintendo => GlyphStyle::Letters,
            style => style,
        };
        let cw = glyphs::chord_width(fonts, press, style, k);
        glyphs::draw_chord(
            canvas,
            ui,
            fonts,
            press,
            style,
            cx - cw / 2.0,
            top + 110.0 * k,
            k,
        );
    }
}

impl ScreenView for ShortcutRowScreen {
    fn title(&self) -> String {
        if self.title.is_empty() {
            c::shortcuts_add().into()
        } else {
            self.title.clone()
        }
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.recorder.is_some() {
            if ev == MenuEvent::Back {
                self.recorder = None;
                return Some(MenuPulse::Confirm);
            }
            return None;
        }
        if let Some(p) = self.picker.as_mut() {
            let picked = p.menu(ev);
            return self.picked(picked, ctx, fx);
        }
        if let Some(tray) = self.tray {
            let (msg, pulse) = match tray {
                Tray::Keys => self.keys.menu(ev),
                Tray::Buttons => self.buttons.menu(ev),
            };
            self.tray_msg(msg, ctx, fx);
            return pulse;
        }
        if ev == MenuEvent::Back {
            if self.replace.take().is_some() {
                self.revert(ctx);
                return Some(MenuPulse::Confirm);
            }
            fx.pop();
            return None;
        }
        let fields = self.fields(ctx);
        let (msg, pulse) = self.list.menu(ev, fields.len());
        let field = fields.get(self.list.cursor).copied();
        match (msg, field) {
            (ListMsg::Activate, Some(f)) => self.activate(f, ctx, fx),
            (ListMsg::Adjust(d), Some(f)) => Some(self.adjust(f, d, ctx, fx)),
            _ => pulse,
        }
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.recorder.is_some() {
            if p.press() {
                self.recorder = None;
            }
            return true;
        }
        if let Some(pk) = self.picker.as_mut() {
            let picked = pk.pointer(p);
            self.picked(picked, ctx, fx);
            return true;
        }
        if let Some(tray) = self.tray {
            let t = match tray {
                Tray::Keys => &mut self.keys,
                Tray::Buttons => &mut self.buttons,
            };
            if !t.covers(p) {
                if p.press() {
                    self.tray_msg(TrayMsg::Close, ctx, fx);
                }
                return true;
            }
            let (msg, _) = t.pointer(p);
            self.tray_msg(msg, ctx, fx);
            return true;
        }
        let fields = self.fields(ctx);
        let (msg, pulse) = self.list.pointer(p, fields.len());
        let field = fields.get(self.list.cursor).copied();
        match (msg, field) {
            (ListMsg::Activate, Some(f)) => {
                self.activate(f, ctx, fx);
                true
            }
            (ListMsg::Adjust(d), Some(f)) => {
                self.adjust(f, d, ctx, fx);
                true
            }
            _ => pulse.is_some(),
        }
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let back = |label| Hint::new(HintKey::Back, label);
        if self.recorder.is_some() {
            return vec![back(common::common_cancel())];
        }
        if self.picker.is_some() {
            return Picker::hints();
        }
        match self.tray {
            Some(Tray::Keys) => {
                return vec![
                    Hint::new(HintKey::Confirm, common::common_choose()),
                    back(common::common_close()),
                ]
            }
            Some(Tray::Buttons) => {
                return vec![
                    Hint::new(HintKey::Confirm, con::console_hint_toggle()),
                    back(common::common_done()),
                ]
            }
            None => {}
        }
        let field = self.fields(ctx).get(self.list.cursor).copied();
        let mut hints = Vec::with_capacity(3);
        match field {
            Some(Field::Does) => {
                hints.push(Hint::new(HintKey::Confirm, con::console_hint_change()))
            }
            Some(Field::Press)
                if matches!(self.draft.trigger, Trigger::Pad { .. }) && Self::can_record(ctx) =>
            {
                hints.push(Hint::new(HintKey::Confirm, c::shortcuts_record()))
            }
            Some(Field::Press | Field::Choose) => {
                hints.push(Hint::new(HintKey::Confirm, common::common_choose()))
            }
            Some(Field::Modifier(_)) => {
                hints.push(Hint::new(HintKey::Adjust, common::common_change()));
                hints.push(Hint::new(HintKey::Confirm, con::console_hint_toggle()));
            }
            Some(Field::FiresOn) if self.spec(Field::FiresOn, ctx).adjustable => {
                hints.push(Hint::new(HintKey::Adjust, common::common_change()))
            }
            Some(Field::FiresOn) => {}
            Some(Field::Remove) => hints.push(Hint::new(HintKey::Confirm, common::common_remove())),
            Some(Field::Replace | Field::Cancel) => {
                hints.push(Hint::new(HintKey::Confirm, common::common_choose()))
            }
            None => {}
        }
        hints.push(back(common::common_back()));
        hints
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let ui = ctx.ui;
        self.title = self.action_name(ctx);
        let top = match self.question(ctx) {
            Some(q) => blurb(canvas, ui, fonts, &q, rect, k).top,
            None => rect.top,
        };
        let seat_keys = self.keys.seat(self.tray == Some(Tray::Keys), dt);
        let seat_buttons = self.buttons.seat(self.tray == Some(Tray::Buttons), dt);
        let tray_h = (self.keys.height() + 12.0) * k * seat_keys
            + (self.buttons.height() + 12.0) * k * seat_buttons;
        let fields = self.fields(ctx);
        let rows: Vec<RowSpec> = fields.iter().map(|f| self.spec(*f, ctx)).collect();
        self.list.cursor = self.list.cursor.min(rows.len().saturating_sub(1));
        let list_rect = Rect::from_ltrb(rect.left, top, rect.right, rect.bottom - tray_h as f32);
        let active = self.picker.is_none() && self.tray.is_none() && self.recorder.is_none();
        self.list
            .render(canvas, ui, list_rect, &rows, fonts, k, dt, active);
        let (w, bottom) = (f64::from(rect.width()), f64::from(rect.bottom));
        if seat_keys > 0.0 {
            let key = match &self.draft.trigger {
                Trigger::Key { key, .. } => vec![key.as_str()],
                Trigger::Pad { .. } => Vec::new(),
            };
            (self.keys).render(canvas, ui, fonts, w, bottom, seat_keys, k, &key);
        }
        if seat_buttons > 0.0 {
            let chosen = self.chosen_buttons();
            (self.buttons).render(canvas, ui, fonts, w, bottom, seat_buttons, k, &chosen);
        }
        if let Some(pk) = self.picker.as_mut() {
            pk.render(canvas, ui, rect, fonts, k, dt);
        }
        if self.recorder.is_some() {
            self.draw_recorder(canvas, rect, k, fonts, ctx);
        }
    }

    fn press(&mut self) {
        match self.picker.as_mut() {
            Some(p) => p.list().dip(),
            None => self.list.dip(),
        }
    }

    fn pan(&mut self, p: Pointer) -> bool {
        match self.picker.as_mut() {
            Some(pk) => pk.list().pan(p),
            None if self.tray.is_none() => self.list.pan(p),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
