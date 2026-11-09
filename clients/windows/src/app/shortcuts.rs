//! The Shortcuts page on Windows (design/client-shortcuts.md §5): the Stream controls rows,
//! each binding's chord a button that opens its editor. *Does* is the dial's own catalogue
//! plus Quick actions itself. *Press* records a key chord off the real keyboard (Ctrl, Alt,
//! Shift and a grid key; Windows keeps Win) or chooses keys or buttons from a grid: this shell
//! reads no controller, so a pad shortcut is always chosen. *Fires on* is Press or Hold, for
//! buttons only. [`Shortcuts::validate`] judges every save. A save writes the whole blob
//! through the settings page's [`super::settings::commit`], so a preset owns all its shortcuts
//! once it changes one. The model is `pf_client_core::shortcuts`.

use super::help::key_chip;
use super::quick_actions::{
    catalogue_picker, describe, effective, grid_key, mods_of, toggle, with_note, wrap_row, Props,
};
use super::settings::commit;
use super::style::*;
use crate::trust::Settings;
use pf_client_core::overlay_actions::{
    catalogue, key_legend, CatalogueEntry, CatalogueGroup, GroupId, OverlayConfig, RingPlatform,
    SlotId, KEY_GRID,
};
use pf_client_core::shortcuts::{
    resolve, Action, Binding, Client, Conflict, Device, Press, Shortcuts, Trigger, BUTTONS,
    MODIFIERS,
};
use pf_l10n::{client, common, desktop};
use std::rc::Rc;
use windows_reactor::*;

/// A pad shortcut holds at most four buttons (design D3).
const MAX_BUTTONS: usize = 4;

/// The shortcuts of `s`, or this client's defaults while it has none. Windows has no Game Mode.
pub(super) fn resolved(s: &Settings) -> Shortcuts {
    resolve(&s.shortcuts, Client::Desktop, false, &s.guide_gesture)
}

/// Quick actions' key and button shortcuts, for the captions that name both.
pub(super) fn ring_keys(sc: &Shortcuts) -> Option<(String, String)> {
    Some((
        sc.hint(&Action::Ring, Device::Keyboard)?,
        sc.hint(&Action::Ring, Device::Pad)?,
    ))
}

/// One shortcut being edited: `index` is `None` for a new one. `conflict` is what the last
/// save ran into, with its words; an identical trigger waits there for Replace.
#[derive(Clone, PartialEq)]
struct Draft {
    index: Option<usize>,
    binding: Binding,
    picking: bool,
    choosing: bool,
    recording: bool,
    conflict: Option<(Conflict, String)>,
}

impl Draft {
    fn of(index: Option<usize>, binding: Binding) -> Draft {
        let new = index.is_none();
        Draft {
            index,
            binding,
            picking: new,
            choosing: new,
            recording: false,
            conflict: None,
        }
    }
}

/// The page's own state: the shortcut open in the editor, and whether Reset is armed.
#[derive(Clone, PartialEq, Default)]
pub(super) struct Ui {
    edit: Option<Draft>,
    reset_armed: bool,
}

type SetUi = Callback<Ui>;

/// The Shortcuts section: the rows, or the editor of one. Its message is its next state.
pub(super) struct ShortcutsPage {
    ui: Ui,
}

impl Component for ShortcutsPage {
    type Input = Props;
    type Message = Ui;

    fn create(_props: &Props, _cx: &ComponentContext<Self>) -> Self {
        ShortcutsPage { ui: Ui::default() }
    }

    fn update(&mut self, ui: Ui, _cx: &ComponentContext<Self>) {
        self.ui = ui;
    }

    fn view(&self, props: &Props, cx: &mut ViewContext<'_, Self>) -> View {
        let (ui, set_ui) = (&self.ui, &cx.forward());
        let s = effective(props);
        let cfg = OverlayConfig::parse(&s.overlay_actions, RingPlatform::Desktop);
        let sc = resolved(&s);
        match &ui.edit {
            Some(d) => editor(props, &cfg, &sc, d, ui, set_ui),
            None => list(props, &cfg, &sc, ui, set_ui),
        }
    }
}

/// Write the shortcuts: the whole blob, into the scope being edited.
fn write(props: &Props, sc: &Shortcuts) {
    let blob = sc.to_json();
    commit(
        &props.ctx,
        &props.scope,
        "shortcuts",
        (props.rev, &props.set_rev),
        &props.set_status,
        move |s| s.shortcuts = blob,
    );
}

/// An action's name on this shell, and why it is dimmed where it is.
fn action_line(cfg: &OverlayConfig, action: &Action) -> (String, String) {
    match action {
        Action::Ring => (client::ring_open_ring().into(), String::new()),
        Action::Slot(SlotId::Shortcut(id)) if cfg.shortcut(id).is_none() => {
            (action.id(), client::shortcuts_not_on_device().into())
        }
        Action::Slot(slot) => describe(cfg, slot),
        Action::Unknown(id) => (id.clone(), client::shortcuts_not_on_device().into()),
    }
}

/// The second chord of an `Either` row: the next press of the same action on the device.
fn partner(sc: &Shortcuts, i: usize) -> Option<usize> {
    let b = &sc.bindings[i];
    (i + 1..sc.bindings.len()).find(|&j| {
        let o = &sc.bindings[j];
        o.action == b.action && o.trigger.device() == b.trigger.device() && !o.trigger.hold()
    })
}

/// The bindings without the one at `index`. Another writer may have shortened the list since
/// the editor opened; an index past its end removes nothing.
fn without(sc: &Shortcuts, index: Option<usize>) -> Shortcuts {
    let mut rest = sc.clone();
    if let Some(i) = index.filter(|&i| i < rest.bindings.len()) {
        rest.bindings.remove(i);
    }
    rest
}

/// The bindings with `d` in its place, after dropping `replace` from the others; or the
/// conflict, with the list it was found in.
fn place(
    sc: &Shortcuts,
    d: &Draft,
    replace: Option<usize>,
) -> Result<Shortcuts, (Conflict, Shortcuts)> {
    let mut next = without(sc, d.index);
    let mut at = d.index.unwrap_or(next.bindings.len());
    if let Some(j) = replace.filter(|&j| j < next.bindings.len()) {
        next.bindings.remove(j);
        at -= usize::from(j < at);
    }
    if let Err(c) = next.validate(&d.binding) {
        return Err((c, next));
    }
    next.bindings
        .insert(at.min(next.bindings.len()), d.binding.clone());
    Ok(next)
}

/// Why a save was refused, in the player's words. `others` is the list it was checked against.
fn refusal(cfg: &OverlayConfig, others: &Shortcuts, b: &Binding, c: Conflict) -> String {
    let chord = |t: &Trigger| t.chord(Client::Desktop);
    match c {
        Conflict::Identical(j) => client::shortcuts_identical_replace(
            b.trigger.label(Client::Desktop),
            &action_line(cfg, &others.bindings[j].action).0,
        ),
        Conflict::PressSubset(j) => {
            let other = &others.bindings[j].trigger;
            let (first, second) = if b.trigger.ids().len() < other.ids().len() {
                (&b.trigger, other)
            } else {
                (other, &b.trigger)
            };
            client::shortcuts_subset(chord(first), chord(second))
        }
        Conflict::SinglePress => client::shortcuts_one_button_hold().into(),
        Conflict::NoKey => client::ring_shortcut_pick_key_first().into(),
        Conflict::NoModifier => client::shortcuts_needs_modifier().into(),
    }
}

/// An empty trigger on `device`. A lone pad button is a hold, so a new pad trigger starts as one.
fn empty(device: Device) -> Trigger {
    match device {
        Device::Pad => Trigger::Pad {
            buttons: Vec::new(),
            hold: true,
        },
        Device::Keyboard => Trigger::Key {
            modifiers: Vec::new(),
            key: String::new(),
        },
    }
}

/// Whether a trigger names something to press: a button, or a key.
fn complete(t: &Trigger) -> bool {
    match t {
        Trigger::Pad { buttons, .. } => !buttons.is_empty(),
        Trigger::Key { key, .. } => !key.is_empty(),
    }
}

/// Change the open draft; any change clears the last refusal.
fn edit(ui: &Ui, set_ui: &SetUi, f: impl FnOnce(&mut Draft)) {
    let mut u = ui.clone();
    if let Some(d) = u.edit.as_mut() {
        d.conflict = None;
        f(d);
    }
    set_ui.call(u);
}

/// Open `draft` in the editor, or close it with `None`.
fn open(ui: &Ui, set_ui: &SetUi, draft: Option<Draft>) {
    let mut u = ui.clone();
    u.edit = draft;
    u.reset_armed = false;
    set_ui.call(u);
}

/// The rows: a card per group, each binding's chord a button into its editor; Add; Reset.
fn list(props: &Props, cfg: &OverlayConfig, sc: &Shortcuts, ui: &Ui, set_ui: &SetUi) -> View {
    let pal = &props.pal;
    let chip_button = |i: usize, label: &str| -> View {
        let b = sc.bindings[i].clone();
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        button(b.trigger.label(Client::Desktop))
            .automation_help_text(label)
            .on_click(move || open(&ui, &set_ui, Some(Draft::of(Some(i), b.clone()))))
            .into()
    };
    let mut parts: Vec<View> = vec![muted(pal, client::shortcuts_lead())
        .text_wrapping(TextWrapping::Wrap)
        .into()];
    for g in sc.rows_in(cfg, Client::Desktop, true) {
        let mut children: Vec<View> = Vec::new();
        for (r, item) in g.items.iter().enumerate() {
            let row = r as i32;
            let line = match item.binding {
                Some(i) => {
                    let (label, note) = action_line(cfg, &sc.bindings[i].action);
                    let mut chips = vec![chip_button(i, &label)];
                    if matches!(item.keys, Press::Either(..)) {
                        chips.extend(partner(sc, i).map(|j| chip_button(j, &label)));
                    }
                    children.push(
                        hstack(chips)
                            .spacing(6.0)
                            .grid_row(row)
                            .grid_column(0)
                            .into(),
                    );
                    with_note(&label, &note)
                }
                None => {
                    children.push(
                        key_chip(pal, &item.keys.to_string())
                            .grid_row(row)
                            .grid_column(0)
                            .into(),
                    );
                    with_note(&item.text(), client::shortcuts_fixed())
                }
            };
            children.push(
                muted(pal, line)
                    .text_wrapping(TextWrapping::Wrap)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_row(row)
                    .grid_column(1)
                    .into(),
            );
        }
        let table = Grid::new()
            .children(children)
            .columns([GridLength::Auto, GridLength::STAR])
            .rows(vec![GridLength::Auto; g.items.len()])
            .column_spacing(12.0)
            .row_spacing(6.0);
        parts.push(
            card(vstack((
                text(g.heading())
                    .font_weight(FontWeight::SEMI_BOLD)
                    .margin(edges(0.0, 0.0, 0.0, 8.0)),
                table,
            )))
            .into(),
        );
    }
    let add = icon_button("plus", client::shortcuts_add()).on_click({
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        move || {
            let new = Binding {
                trigger: empty(Device::Pad),
                action: Action::Ring,
            };
            open(&ui, &set_ui, Some(Draft::of(None, new)));
        }
    });
    // Two presses, like the ring's reset: the accent fill and the line under it say it is armed.
    let armed = ui.reset_armed;
    let reset = icon_button(
        "rotate-cw",
        if armed {
            client::ring_reset_confirm()
        } else {
            client::shortcuts_reset()
        },
    )
    .style(if armed {
        ButtonStyle::Accent
    } else {
        ButtonStyle::Default
    })
    .on_click({
        let (props, ui, set_ui) = (props.clone(), ui.clone(), set_ui.clone());
        move || {
            if armed {
                commit(
                    &props.ctx,
                    &props.scope,
                    "shortcuts",
                    (props.rev, &props.set_rev),
                    &props.set_status,
                    |s| s.shortcuts.clear(),
                );
            }
            let mut u = ui.clone();
            u.reset_armed = !armed;
            set_ui.call(u);
        }
    });
    parts.push(hstack((add, reset)).spacing(8.0).into());
    if armed {
        parts.push(
            muted(pal, desktop::desktop_shortcuts_reset_body())
                .font_size(12.0)
                .text_wrapping(TextWrapping::Wrap)
                .into(),
        );
    }
    vstack(parts).spacing(10.0).into()
}

/// A labelled block of the editor: the label small above its controls.
fn field(pal: &pf_fluent::Palette, label: &str, body: Vec<View>) -> View {
    let mut parts: Vec<View> = vec![muted(pal, label).font_size(12.0).into()];
    parts.extend(body);
    vstack(parts).spacing(6.0).into()
}

/// One of a set of choices: the picked one filled with the accent.
fn choice(label: &str, on: bool, pick: impl Fn() + 'static) -> Button {
    button(label)
        .style(if on {
            ButtonStyle::Accent
        } else {
            ButtonStyle::Subtle
        })
        .on_click(pick)
}

/// The dial's catalogue for a shortcut: Quick actions itself first, no Empty.
fn shortcut_catalogue(cfg: &OverlayConfig) -> Vec<CatalogueGroup> {
    let mut groups = catalogue(cfg, RingPlatform::Desktop);
    groups.retain(|g| g.id != GroupId::Empty);
    if let Some(first) = groups.first_mut() {
        first.entries.insert(
            0,
            CatalogueEntry {
                id: Action::Ring.id(),
                label: client::ring_open_ring().into(),
                note: String::new(),
            },
        );
    }
    groups
}

/// The pad buttons as toggles, up to [`MAX_BUTTONS`]. Down to one button, it is a hold.
fn button_grid(buttons: &[u32], ui: &Ui, set_ui: &SetUi) -> View {
    let toggles: Vec<View> = BUTTONS
        .iter()
        .map(|&(_, bit)| {
            let on = buttons.contains(&bit);
            let name = Trigger::Pad {
                buttons: vec![bit],
                hold: false,
            }
            .chord(Client::Desktop);
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            toggle(name, on, move |now| {
                edit(&ui, &set_ui, |d| {
                    if let Trigger::Pad { buttons, hold } = &mut d.binding.trigger {
                        buttons.retain(|&b| b != bit);
                        if now {
                            buttons.push(bit);
                        }
                        *hold |= buttons.len() == 1;
                    }
                })
            })
            .is_enabled(on || buttons.len() < MAX_BUTTONS)
            .into()
        })
        .collect();
    wrap_row(toggles)
}

/// Ctrl, Alt and Shift as toggles over the keyboard-shaped key grid.
fn key_grid(modifiers: &[String], key: &str, ui: &Ui, set_ui: &SetUi) -> View {
    let mods: Vec<View> = MODIFIERS
        .iter()
        .map(|&m| {
            let on = modifiers.iter().any(|h| h == m);
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            toggle(key_legend(m), on, move |now| {
                edit(&ui, &set_ui, |d| {
                    if let Trigger::Key { modifiers, .. } = &mut d.binding.trigger {
                        let held: Vec<String> = MODIFIERS
                            .iter()
                            .filter(|&&x| {
                                if x == m {
                                    now
                                } else {
                                    modifiers.iter().any(|h| h == x)
                                }
                            })
                            .map(|x| x.to_string())
                            .collect();
                        *modifiers = held;
                    }
                })
            })
            .into()
        })
        .collect();
    let mut rows: Vec<View> = vec![hstack(mods).spacing(6.0).into()];
    rows.extend(KEY_GRID.iter().map(|row| {
        let keys: Vec<View> = row
            .iter()
            .map(|&name| {
                let (ui, set_ui) = (ui.clone(), set_ui.clone());
                toggle(key_legend(name), key == name, move |now| {
                    if now {
                        edit(&ui, &set_ui, |d| {
                            if let Trigger::Key { key, .. } = &mut d.binding.trigger {
                                *key = name.to_string();
                            }
                        })
                    }
                })
                .into()
            })
            .collect();
        hstack(keys)
            .spacing(4.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into()
    }));
    vstack(rows).spacing(4.0).into()
}

/// One shortcut: Does, the device, Press with Record and Choose, Fires on, the refusal, then
/// Save, Remove and Cancel. While recording, the card takes the next grid key pressed.
fn editor(
    props: &Props,
    cfg: &OverlayConfig,
    sc: &Shortcuts,
    d: &Draft,
    ui: &Ui,
    set_ui: &SetUi,
) -> View {
    let pal = &props.pal;
    let trigger = &d.binding.trigger;
    let device = trigger.device();
    let (label, note) = action_line(cfg, &d.binding.action);
    let title = if d.index.is_some() {
        label.clone()
    } else {
        client::shortcuts_add().to_string()
    };

    let mut does: Vec<View> = vec![button(with_note(&label, &note))
        .on_click({
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            move || edit(&ui, &set_ui, |d| d.picking = !d.picking)
        })
        .into()];
    if d.picking {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        does.push(catalogue_picker(
            pal,
            shortcut_catalogue(cfg),
            &d.binding.action.id(),
            Rc::new(move |id: String| {
                edit(&ui, &set_ui, |d| {
                    d.binding.action = Action::parse(&id);
                    d.picking = false;
                })
            }),
        ));
    }

    let on_device = |dev: Device, label: &str| {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        choice(label, device == dev, move || {
            if dev != device {
                edit(&ui, &set_ui, |d| {
                    d.binding.trigger = empty(dev);
                    d.choosing = true;
                    d.recording = false;
                })
            }
        })
    };
    let chord = trigger.chord(Client::Desktop);
    let shown = if d.recording {
        client::shortcuts_record_prompt().to_string()
    } else if chord.is_empty() {
        "\u{2014}".to_string()
    } else {
        chord
    };
    let mut tools: Vec<View> = Vec::new();
    if device == Device::Keyboard {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        tools.push(
            choice(client::shortcuts_record(), d.recording, move || {
                edit(&ui, &set_ui, |d| {
                    d.recording = !d.recording;
                    d.choosing = false;
                })
            })
            .into(),
        );
    }
    tools.push({
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        choice(client::shortcuts_choose(), d.choosing, move || {
            edit(&ui, &set_ui, |d| {
                d.choosing = !d.choosing;
                d.recording = false;
            })
        })
        .into()
    });
    let mut press: Vec<View> = vec![
        hstack((
            on_device(Device::Pad, client::controls_group_pad()),
            on_device(Device::Keyboard, client::controls_group_keyboard()),
        ))
        .spacing(6.0)
        .into(),
        hstack((
            text(shown)
                .font_size(20.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .vertical_alignment(VerticalAlignment::Center),
            hstack(tools).spacing(6.0),
        ))
        .spacing(16.0)
        .into(),
    ];
    if d.choosing {
        press.push(match trigger {
            Trigger::Pad { buttons, .. } => button_grid(buttons, ui, set_ui),
            Trigger::Key { modifiers, key } => key_grid(modifiers, key, ui, set_ui),
        });
    }

    let mut parts: Vec<View> = vec![
        section(pal, &title),
        field(pal, client::shortcuts_does(), does),
        field(pal, client::shortcuts_press(), press),
    ];
    if let Trigger::Pad { buttons, hold } = trigger {
        let one = buttons.len() == 1;
        let fires_on = |to_hold: bool, label: &str| {
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            choice(label, *hold == to_hold, move || {
                edit(&ui, &set_ui, |d| {
                    if let Trigger::Pad { hold, .. } = &mut d.binding.trigger {
                        *hold = to_hold;
                    }
                })
            })
        };
        let mut body: Vec<View> = vec![hstack((
            fires_on(false, client::shortcuts_fires_press()).is_enabled(!one),
            fires_on(true, client::shortcuts_fires_hold()),
        ))
        .spacing(6.0)
        .into()];
        if one {
            body.push(
                muted(pal, client::shortcuts_one_button_hold())
                    .font_size(12.0)
                    .text_wrapping(TextWrapping::Wrap)
                    .into(),
            );
        }
        parts.push(field(pal, client::shortcuts_fires_on(), body));
    }

    let save = |replace: Option<usize>| {
        let (props, cfg, sc, d) = (props.clone(), cfg.clone(), sc.clone(), d.clone());
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        move || match place(&sc, &d, replace) {
            Ok(next) => {
                write(&props, &next);
                open(&ui, &set_ui, None);
            }
            Err((c, others)) => {
                let why = refusal(&cfg, &others, &d.binding, c);
                edit(&ui, &set_ui, |d| d.conflict = Some((c, why)));
            }
        }
    };
    if let Some((c, why)) = &d.conflict {
        let mut body: Vec<View> = vec![InfoBar::new()
            .message(why)
            .severity(InfoBarSeverity::Warning)
            .is_open(true)
            .is_closable(false)
            .into()];
        if let Conflict::Identical(j) = *c {
            body.push(
                hstack((
                    button(client::shortcuts_replace())
                        .style(ButtonStyle::Accent)
                        .on_click(save(Some(j))),
                    button(common::common_cancel()).on_click({
                        let (ui, set_ui) = (ui.clone(), set_ui.clone());
                        move || edit(&ui, &set_ui, |_| {})
                    }),
                ))
                .spacing(8.0)
                .into(),
            );
        }
        parts.push(vstack(body).spacing(8.0).into());
    }

    let mut actions: Vec<View> = vec![button(if d.index.is_some() {
        common::common_save()
    } else {
        common::common_add()
    })
    .style(ButtonStyle::Accent)
    .is_enabled(complete(trigger))
    .on_click(save(None))
    .into()];
    if let Some(i) = d.index {
        let (props, sc, ui, set_ui) = (props.clone(), sc.clone(), ui.clone(), set_ui.clone());
        actions.push(
            button(client::shortcuts_remove())
                .on_click(move || {
                    write(&props, &without(&sc, Some(i)));
                    open(&ui, &set_ui, None);
                })
                .into(),
        );
    }
    actions.push({
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        button(common::common_cancel())
            .style(ButtonStyle::Subtle)
            .on_click(move || open(&ui, &set_ui, None))
            .into()
    });
    parts.push(hstack(actions).spacing(8.0).into());

    let editor_card = card(vstack(parts).spacing(14.0));
    if !d.recording {
        return editor_card.into();
    }
    let (ui, set_ui) = (ui.clone(), set_ui.clone());
    editor_card
        .on_preview_key_down(RoutedCallback::new(move |k: KeyEventInfo| {
            let Some(name) = grid_key(k.key.0) else {
                return false;
            };
            let held = mods_of(k.modifiers);
            edit(&ui, &set_ui, |d| {
                d.binding.trigger = Trigger::Key {
                    modifiers: MODIFIERS
                        .iter()
                        .zip(held)
                        .filter(|(_, on)| *on)
                        .map(|(m, _)| m.to_string())
                        .collect(),
                    key: name.to_string(),
                };
                d.recording = false;
            });
            true
        }))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::shortcuts::button_bit;

    fn pad(names: &[&str], hold: bool) -> Trigger {
        Trigger::Pad {
            buttons: names.iter().filter_map(|n| button_bit(n)).collect(),
            hold,
        }
    }

    fn draft(index: Option<usize>, trigger: Trigger, action: Action) -> Draft {
        Draft::of(index, Binding { trigger, action })
    }

    /// A save keeps the edited row's place; Replace drops the twin and shifts past it; F11
    /// alone saves.
    #[test]
    fn a_save_keeps_its_place_and_a_replace_drops_the_twin() {
        let sc = resolved(&Settings::default());
        let at = |action: Action, device: Device| {
            sc.bindings
                .iter()
                .position(|b| b.action == action && b.trigger.device() == device)
                .unwrap()
        };
        let ring = at(Action::Ring, Device::Pad);
        let stats = at(Action::Slot(SlotId::Stats), Device::Pad);
        assert!(ring < stats, "the defaults list Select + A first");

        let d = draft(
            Some(stats),
            pad(&["select", "a"], false),
            Action::Slot(SlotId::Stats),
        );
        let Err((Conflict::Identical(j), _)) = place(&sc, &d, None) else {
            panic!("Select + A is taken");
        };
        let next = place(&sc, &d, Some(j)).expect("replaced");
        assert_eq!(next.bindings.len(), sc.bindings.len() - 1);
        assert_eq!(next.bindings[stats - 1], d.binding);
        assert!(!next
            .bindings
            .iter()
            .any(|b| b.action == Action::Ring && b.trigger.device() == Device::Pad));

        let f11 = sc
            .bindings
            .iter()
            .position(
                |b| matches!(&b.trigger, Trigger::Key { modifiers, .. } if modifiers.is_empty()),
            )
            .unwrap();
        let mut d = Draft::of(Some(f11), sc.bindings[f11].clone());
        d.binding.action = Action::Slot(SlotId::Minimize);
        let next = place(&sc, &d, None).expect("F11 keeps its keys");
        assert_eq!(next.bindings[f11], d.binding);

        let d = draft(None, pad(&["lb", "rb"], false), Action::Ring);
        assert!(matches!(
            place(&sc, &d, None),
            Err((Conflict::PressSubset(_), _))
        ));
    }
}
