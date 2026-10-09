//! The quick-action ring's editor on Windows (design/touch-client-overlay.md §3.3): the editor
//! IS the ring — six discs on the ring's own geometry over a flat card stage, drawn with WinUI's
//! own elements. Each disc wears the slot's Lucide mark, the same mark the in-stream ring draws
//! for it, and the name of the disc under the pointer reads out in the band below the ring. A
//! click on a disc opens its picker (the catalogue by group, notes where a slot is unavailable
//! here); a disc dragged onto another swaps the two; a row of six buttons under the ring gives a
//! keyboard and a screen reader the same reach. Under it the shortcuts: a name, the four
//! modifiers as toggles, the key on a keyboard-shaped grid or pressed on the real keyboard, Save
//! and Remove. Every edit commits at once through the settings page's own
//! [`super::settings::commit`], so a preset owns the whole ring the moment it touches it (D10).
//! The model — catalogue, geometry, chords, icons — is `pf_client_core`'s.

use super::lucide;
use super::settings::{active_preset, commit};
use super::style::*;
use super::AppCtx;
use pf_client_core::overlay_actions::{
    catalogue, chord_chip, key_legend, slot_icon, Chord, OverlayConfig, RingPlatform, Shortcut,
    SlotId, CHORD_MODIFIERS, KEY_GRID, RING_SLOTS,
};
use pf_client_core::ring::{slot_offset, CENTRE_DIAMETER, RING_RADIUS, SLOT_DIAMETER};
use std::sync::Arc;
use windows_reactor::*;

/// The stage the ring sits on, in DIPs: the ring's diameter plus a disc plus a margin each way.
const STAGE_W: f64 = 440.0;
const STAGE_H: f64 = 340.0;
/// A press this far from where it landed is a drag, under it a click.
const DRAG_SLOP: f64 = 8.0;
/// A disc takes a press this far past its edge, so a pointer need not be exact.
const HIT_SLOP: f64 = 1.2;
/// The Lucide mark on a disc, in DIPs. The console draws it at 1.05x the disc's radius; this is
/// that, so a disc reads the same weight in the editor as it does in the stream.
const ICON_DIP: f64 = SLOT_DIAMETER as f64 * 1.05 / 2.0;

const CLOCK: [&str; RING_SLOTS] = [
    "12 o'clock",
    "2 o'clock",
    "4 o'clock",
    "6 o'clock",
    "8 o'clock",
    "10 o'clock",
];

/// What the settings page hands the editor: the app, the scope being edited, the revision
/// counter every commit bumps (the page repaints on it) and the status line a failed save
/// lands on.
#[derive(Clone)]
pub(super) struct Props {
    pub ctx: Arc<AppCtx>,
    pub scope: String,
    pub rev: u64,
    pub set_rev: AsyncSetState<u64>,
    pub set_status: AsyncSetState<String>,
}

impl PartialEq for Props {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx) && self.scope == other.scope && self.rev == other.rev
    }
}

/// A disc a pointer carries: its slot, where the press landed, how far it went.
#[derive(Clone, Copy, PartialEq)]
struct Drag {
    slot: usize,
    x0: f64,
    y0: f64,
    dx: f64,
    dy: f64,
}

impl Drag {
    fn moved(&self) -> bool {
        self.dx.hypot(self.dy) > DRAG_SLOP
    }
}

/// One shortcut being edited: `id` is `None` for a new one.
#[derive(Clone, PartialEq, Default)]
struct Draft {
    id: Option<String>,
    label: String,
    chord: Chord,
}

impl Draft {
    fn of(sc: &Shortcut) -> Draft {
        Draft {
            id: Some(sc.id.clone()),
            label: sc.label.clone(),
            chord: Chord::parse(&sc.keys),
        }
    }
}

/// The editor's own state: which disc is open in the picker, which the pointer is over, a
/// carry in progress, a shortcut being edited, whether the next chord on the keyboard is being
/// captured, and whether the reset has been armed by a first press.
#[derive(Clone, PartialEq, Default)]
struct Ui {
    selected: Option<usize>,
    hover: Option<usize>,
    drag: Option<Drag>,
    draft: Option<Draft>,
    capture: bool,
    reset_armed: bool,
}

/// The scope's effective ring: the globals with the preset's overrides on top.
fn current(props: &Props) -> OverlayConfig {
    let base = props.ctx.settings.lock().unwrap().clone();
    let effective = match active_preset(&props.scope) {
        Some(p) => p.overrides.apply(&base),
        None => base,
    };
    OverlayConfig::parse(&effective.overlay_actions, RingPlatform::Desktop)
}

/// Write the ring: the whole blob, through the page's commit, into the scope being edited.
fn write(props: &Props, cfg: &OverlayConfig) {
    let blob = cfg.to_json();
    commit(
        &props.ctx,
        &props.scope,
        "overlay_actions",
        (props.rev, &props.set_rev),
        &props.set_status,
        move |s| {
            s.overlay_actions = blob;
        },
    );
}

/// The word a disc carries — the console ring's short labels, so the desktop rings read alike.
fn short_label(cfg: &OverlayConfig, slot: &SlotId) -> String {
    match slot {
        SlotId::EndStream => "End".into(),
        SlotId::EndGame => "Quit".into(),
        SlotId::DisconnectLinger => "Leave".into(),
        SlotId::TouchMode => "Touch".into(),
        SlotId::Keyboard => "Keys".into(),
        SlotId::Stats => "Stats".into(),
        SlotId::Mic => "Mic".into(),
        SlotId::Pad => "Pad".into(),
        SlotId::SendText => "Text".into(),
        SlotId::Guide => "Guide".into(),
        SlotId::Qam => "QAM".into(),
        SlotId::PadMouse => "Mouse".into(),
        SlotId::PadType => "Pad type".into(),
        SlotId::StreamMute => "Mute".into(),
        SlotId::SwapScreens => "Swap".into(),
        SlotId::Host(_) => "Power".into(),
        SlotId::Shortcut(id) => cfg
            .shortcut(id)
            .map(|s| chord_chip(&s.keys))
            .unwrap_or_default(),
    }
}

/// A slot's full name and its availability note on this shell, from the shared catalogue.
fn describe(cfg: &OverlayConfig, slot: &SlotId) -> (String, String) {
    let id = slot.id();
    catalogue(cfg, RingPlatform::Desktop)
        .into_iter()
        .flat_map(|g| g.entries)
        .find(|e| e.id == id)
        .map(|e| (e.label, e.note))
        .unwrap_or((id, String::new()))
}

/// Disc `k`'s centre on the stage.
fn disc_centre(k: usize) -> (f64, f64) {
    let (dx, dy) = slot_offset(k, RING_RADIUS);
    (STAGE_W / 2.0 + dx as f64, STAGE_H / 2.0 + dy as f64)
}

/// The disc under a point on the stage, within slop.
fn disc_at(x: f64, y: f64) -> Option<usize> {
    let r = SLOT_DIAMETER as f64 / 2.0 * HIT_SLOP;
    (0..RING_SLOTS).find(|&k| {
        let (cx, cy) = disc_centre(k);
        (x - cx).hypot(y - cy) <= r
    })
}

// ---- keys ----

/// The virtual key a chord name stands for, for the capture accelerators.
fn virtual_key(name: &str) -> Option<VirtualKey> {
    Some(match name {
        "escape" => VirtualKey::Escape,
        "tab" => VirtualKey::Tab,
        "space" => VirtualKey::Space,
        "enter" => VirtualKey::Enter,
        "backspace" => VirtualKey::Back,
        "delete" => VirtualKey::Delete,
        "insert" => VirtualKey::Insert,
        "home" => VirtualKey::Home,
        "end" => VirtualKey::End,
        "pageup" => VirtualKey::PageUp,
        "pagedown" => VirtualKey::PageDown,
        "up" => VirtualKey::Up,
        "down" => VirtualKey::Down,
        "left" => VirtualKey::Left,
        "right" => VirtualKey::Right,
        "printscreen" => VirtualKey::Snapshot,
        "pause" => VirtualKey::Pause,
        "capslock" => VirtualKey::CapitalLock,
        n => {
            let b = n.as_bytes();
            return match b {
                [c @ b'a'..=b'z'] => Some(VirtualKey((VirtualKey::A.0) + (c - b'a') as i32)),
                [c @ b'0'..=b'9'] => Some(VirtualKey((VirtualKey::Number0.0) + (c - b'0') as i32)),
                [b'f', rest @ ..] if !rest.is_empty() => n[1..]
                    .parse::<i32>()
                    .ok()
                    .filter(|f| (1..=12).contains(f))
                    .map(|f| VirtualKey(VirtualKey::F1.0 + f - 1)),
                _ => None,
            };
        }
    })
}

/// The modifier combinations the capture listens for — Ctrl, Alt and Shift in every mix. The
/// Windows key is the toggle's: the shell never sees a chord that Windows itself owns.
fn modifier_masks() -> [u32; 8] {
    let (c, m, s) = (
        VirtualKeyModifiers::Control.0,
        VirtualKeyModifiers::Menu.0,
        VirtualKeyModifiers::Shift.0,
    );
    [0, c, m, s, c | m, c | s, m | s, c | m | s]
}

fn mods_of(mask: u32) -> [bool; 4] {
    [
        mask & VirtualKeyModifiers::Control.0 != 0,
        mask & VirtualKeyModifiers::Menu.0 != 0,
        mask & VirtualKeyModifiers::Shift.0 != 0,
        false,
    ]
}

// ---- the section ----

/// The Quick actions section: the ring, its picker, the shortcuts and their editor.
pub(super) fn quick_actions_section(props: &Props, cx: &mut RenderCx) -> Element {
    let (ui, set_ui) = cx.use_state(Ui::default());
    let cfg = current(props);
    let mut parts: Vec<Element> = vec![
        text_block(
            "Click a button to change it, or drag one onto another to swap the two. \
             Ctrl+Alt+Shift+O, a two-finger twist or Select+A opens this dial in a stream.",
        )
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .wrap()
        .max_width(560.0)
        .horizontal_alignment(HorizontalAlignment::Left)
        .into(),
        ring(props, &cfg, &ui, &set_ui),
        ring_label(&cfg, &ui),
        legend_row(props, &cfg, &ui, &set_ui),
    ];
    if let Some(k) = ui.selected {
        parts.push(picker(props, &cfg, k, &ui, &set_ui));
    }
    parts.push(match &ui.draft {
        Some(d) => shortcut_editor(props, &cfg, d, &ui, &set_ui),
        None => shortcuts(props, &cfg, &ui, &set_ui),
    });
    vstack(parts).spacing(14.0).into()
}

/// A disc's Lucide mark, as a glyph of the icon font at exactly `size`. Not [`lucide::icon`]:
/// that hands the mark to a control to size and tint, and a disc is neither — it is our own
/// dark surface on both themes, so the ink is white whatever the theme says, and the size is
/// the ring's geometry rather than a button's. `dim` is the unavailable slot, faded not hidden.
fn mark(name: &str, size: f64, dim: bool) -> Element {
    let ink = Color {
        a: if dim { 100 } else { 242 },
        r: 255,
        g: 255,
        b: 255,
    };
    text_block(lucide::glyph(name))
        .font_family(lucide::FAMILY)
        .font_size(size)
        .foreground(ink)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// A disc's face: the slot's Lucide mark, or a stacked keycap for a shortcut. The mark comes
/// from the SHARED slot table, so a disc here carries exactly what the in-stream ring draws for
/// the same slot. Only an id the table cannot know — an unknown host action, a future slot —
/// falls back to its short word, which is what the console does too.
fn disc_face(cfg: &OverlayConfig, slot: Option<&SlotId>, dim: bool) -> Element {
    let ink = if dim {
        Color {
            a: 100,
            r: 255,
            g: 255,
            b: 255,
        }
    } else {
        Color {
            a: 242,
            r: 255,
            g: 255,
            b: 255,
        }
    };
    let Some(slot) = slot else {
        return mark("plus", ICON_DIP * 0.8, true);
    };
    if let SlotId::Shortcut(id) = slot {
        let keys = cfg.shortcut(id).map(|s| s.keys.clone()).unwrap_or_default();
        return keycap(&keys, ink, 9.0, 14.0);
    }
    if let Some(name) = slot_icon(&slot.id(), "") {
        return mark(name, ICON_DIP, dim);
    }
    text_block(short_label(cfg, slot))
        .font_size(12.0)
        .semibold()
        .foreground(ink)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// A chord as a stacked keycap: the modifiers small on top, the key large under them.
fn keycap(keys: &[String], ink: Color, small: f64, big: f64) -> Element {
    let (mods, key) = match keys.split_last() {
        Some((key, mods)) => (
            mods.iter()
                .map(|k| key_legend(k))
                .collect::<Vec<_>>()
                .join(" "),
            key_legend(key),
        ),
        None => (String::new(), String::new()),
    };
    let mut lines: Vec<Element> = Vec::new();
    if !mods.is_empty() {
        lines.push(
            text_block(mods)
                .font_size(small)
                .foreground(ink)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    lines.push(
        text_block(key)
            .font_size(big)
            .bold()
            .foreground(ink)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    );
    vstack(lines)
        .spacing(0.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// A round disc of the in-stream ring's surface: dark, translucent, a white hairline.
fn disc(face: Element, diameter: f64, raised: bool, selected: bool) -> Border {
    border(face)
        .width(diameter)
        .height(diameter)
        .corner_radius(diameter / 2.0)
        .background(Color {
            a: if raised { 190 } else { 140 },
            r: 0,
            g: 0,
            b: 0,
        })
        .border_brush(if selected {
            Color {
                a: 230,
                r: 255,
                g: 255,
                b: 255,
            }
        } else {
            Color {
                a: 46,
                r: 255,
                g: 255,
                b: 255,
            }
        })
        .border_thickness(uniform(if selected { 2.0 } else { 1.0 }))
}

/// The ring on its stage: the discs at the ring's geometry, the inert centre, and the stage's
/// pointer handlers — a carry lives on the stage, not the disc, because a pointer that leaves
/// a disc stops reporting to it.
fn ring(props: &Props, cfg: &OverlayConfig, ui: &Ui, set_ui: &SetState<Ui>) -> Element {
    let mut children: Vec<Element> = Vec::new();
    // The stage is a flat card face, like every other card on this shell. It used to be a
    // gradient, baked to a BMP because reactor's brushes are flat — decoration that read as
    // a different app, and the same reason the console's editor dropped its own.
    children.push(
        border(vstack(Vec::<Element>::new()))
            .background(ThemeRef::CardBackground)
            .border_brush(ThemeRef::CardStroke)
            .border_thickness(uniform(1.0))
            .corner_radius(22.0)
            .width(STAGE_W)
            .height(STAGE_H)
            .canvas_left(0.0)
            .canvas_top(0.0)
            .into(),
    );
    // The centre: what the sheet opens from in-stream — not editable here, dimmed and inert.
    // It wears the ring's own `more` mark, so the editor's centre is the centre people press.
    let cd = CENTRE_DIAMETER as f64;
    children.push(
        disc(mark("ellipsis", ICON_DIP, true), cd, false, false)
            .tooltip("More \u{2014} the rest of the actions, in the stream")
            .canvas_left(STAGE_W / 2.0 - cd / 2.0)
            .canvas_top(STAGE_H / 2.0 - cd / 2.0)
            .into(),
    );
    let d = SLOT_DIAMETER as f64;
    for k in 0..RING_SLOTS {
        let (mut x, mut y) = disc_centre(k);
        let carried = ui.drag.filter(|dr| dr.slot == k && dr.moved());
        if let Some(dr) = carried {
            x += dr.dx;
            y += dr.dy;
        }
        let slot = cfg.ring[k].as_ref();
        let (label, note) = slot
            .map(|s| describe(cfg, s))
            .unwrap_or(("Empty".into(), String::new()));
        let face = disc_face(cfg, slot, !note.is_empty());
        let mut el: Element = disc(face, d, carried.is_some(), ui.selected == Some(k))
            .tooltip(if note.is_empty() {
                label.clone()
            } else {
                format!("{label} — {note}")
            })
            .canvas_left(x - d / 2.0)
            .canvas_top(y - d / 2.0)
            .into();
        if carried.is_some() {
            el = el.canvas_z_index(5);
        }
        children.push(el);
    }

    let press = {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        move |info: PointerEventInfo| {
            let mut u = ui.clone();
            u.drag = disc_at(info.x, info.y).map(|slot| Drag {
                slot,
                x0: info.x,
                y0: info.y,
                dx: 0.0,
                dy: 0.0,
            });
            set_ui.call(u);
        }
    };
    let moved = {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        move |info: PointerEventInfo| {
            if let Some(mut dr) = ui.drag
                && info.is_left_button_pressed
            {
                dr.dx = info.x - dr.x0;
                dr.dy = info.y - dr.y0;
                let mut u = ui.clone();
                u.drag = Some(dr);
                set_ui.call(u);
                return;
            }
            // Otherwise the pointer is only passing over: name the disc it is on, in the band
            // under the ring. Only on a CHANGE — a set per pointer sample would re-render the
            // whole section on every mouse move across the stage.
            let hover = disc_at(info.x, info.y);
            if hover != ui.hover {
                let mut u = ui.clone();
                u.hover = hover;
                set_ui.call(u);
            }
        }
    };
    let release = {
        let (props, cfg, ui, set_ui) = (props.clone(), cfg.clone(), ui.clone(), set_ui.clone());
        move |info: PointerEventInfo| {
            let Some(dr) = ui.drag else { return };
            let mut u = ui.clone();
            u.drag = None;
            if dr.moved() {
                if let Some(j) = disc_at(info.x, info.y).filter(|&j| j != dr.slot) {
                    let mut next = cfg.clone();
                    next.ring.swap(dr.slot, j);
                    write(&props, &next);
                }
            } else {
                // A click: open the picker on this disc (again on the open one closes it).
                u.selected = if ui.selected == Some(dr.slot) {
                    None
                } else {
                    Some(dr.slot)
                };
            }
            set_ui.call(u);
        }
    };
    let leave = {
        let (ui, set_ui) = (ui.clone(), set_ui.clone());
        move || {
            if ui.drag.is_some() || ui.hover.is_some() {
                let mut u = ui.clone();
                u.drag = None;
                u.hover = None;
                set_ui.call(u);
            }
        }
    };
    Canvas::new(children)
        .width(STAGE_W)
        .height(STAGE_H)
        .background(hit_test_backstop())
        .on_pointer_pressed(press)
        .on_pointer_moved(moved)
        .on_pointer_released(release)
        .on_pointer_exited(leave)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

/// The band under the ring: the full name of the disc the pointer is on, or of the one the
/// picker is open over. The in-stream ring carries the same band under its own discs, and it is
/// where the name went when the discs became marks. It keeps its height whatever it says, so
/// the ring does not hop as the pointer crosses a disc.
fn ring_label(cfg: &OverlayConfig, ui: &Ui) -> Element {
    let text = ui
        .hover
        .or(ui.selected)
        .and_then(|k| cfg.ring[k].as_ref())
        .map(|slot| {
            let (label, note) = describe(cfg, slot);
            if note.is_empty() {
                label
            } else {
                format!("{label} \u{2014} {note}")
            }
        })
        .unwrap_or_default();
    text_block(text)
        .font_size(13.0)
        .semibold()
        .height(20.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

/// Six buttons under the ring, one per disc, so a keyboard and a screen reader reach every
/// slot the pointer reaches.
fn legend_row(props: &Props, cfg: &OverlayConfig, ui: &Ui, set_ui: &SetState<Ui>) -> Element {
    let _ = props;
    let buttons: Vec<Element> = (0..RING_SLOTS)
        .map(|k| {
            let slot = cfg.ring[k].as_ref();
            let (label, _) = slot
                .map(|s| describe(cfg, s))
                .unwrap_or(("Empty".into(), String::new()));
            let short = slot.map_or("Empty".to_string(), |s| short_label(cfg, s));
            let mut b = button(short)
                .tooltip(label.clone())
                .automation_name(format!("{}: {label}", CLOCK[k]))
                .on_click({
                    let (ui, set_ui) = (ui.clone(), set_ui.clone());
                    move || {
                        let mut u = ui.clone();
                        u.selected = if ui.selected == Some(k) {
                            None
                        } else {
                            Some(k)
                        };
                        set_ui.call(u);
                    }
                });
            if ui.selected == Some(k) {
                b = b.accent();
            } else {
                b = b.subtle();
            }
            b.into()
        })
        .collect();
    hstack(buttons)
        .spacing(6.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

/// The picker for one disc: the catalogue by group, the current pick marked, a note where a
/// slot cannot work on this shell, and the swap targets for a keyboard.
fn picker(props: &Props, cfg: &OverlayConfig, k: usize, ui: &Ui, set_ui: &SetState<Ui>) -> Element {
    let current = cfg.ring[k].as_ref().map(SlotId::id).unwrap_or_default();
    // No title and no Done: the band above already names the slot being edited, the disc is
    // ringed white while its picker is open, and a pick closes the card — the GTK popover's
    // behaviour, which needed neither either.
    let mut parts: Vec<Element> = Vec::new();
    for group in catalogue(cfg, RingPlatform::Desktop) {
        parts.push(section(group.title));
        let mut row: Vec<Element> = Vec::new();
        for entry in group.entries {
            let is_current = entry.id == current;
            let unavailable =
                !entry.note.is_empty() && !entry.id.starts_with("host:") && !is_current;
            let text = if entry.note.is_empty() {
                entry.label.clone()
            } else {
                format!("{} — {}", entry.label, entry.note)
            };
            let mut b = button(text).on_click({
                let (props, cfg, id) = (props.clone(), cfg.clone(), entry.id.clone());
                let (ui, set_ui) = (ui.clone(), set_ui.clone());
                move || {
                    let mut next = cfg.clone();
                    next.ring[k] = SlotId::parse(&id);
                    write(&props, &next);
                    let mut u = ui.clone();
                    u.selected = None;
                    set_ui.call(u);
                }
            });
            b = if is_current {
                b.accent()
            } else {
                b.subtle().enabled(!unavailable)
            };
            row.push(b.into());
        }
        parts.push(wrap_row(row));
    }
    card(vstack(parts).spacing(8.0)).into()
}

/// Buttons that wrap into rows of at most five, so a group never runs off the card.
fn wrap_row(buttons: Vec<Element>) -> Element {
    let rows: Vec<Element> = buttons
        .chunks(5)
        .map(|chunk| hstack(chunk.to_vec()).spacing(6.0).into())
        .collect();
    vstack(rows).spacing(6.0).into()
}

/// The shortcuts under the ring: a row each with its keycap, name and legend; Add; Reset.
fn shortcuts(props: &Props, cfg: &OverlayConfig, ui: &Ui, set_ui: &SetState<Ui>) -> Element {
    let mut rows: Vec<Element> = Vec::new();
    for sc in &cfg.shortcuts {
        let chip = chord_chip(&sc.keys);
        let ink = Color {
            a: 242,
            r: 255,
            g: 255,
            b: 255,
        };
        let face = disc(keycap(&sc.keys, ink, 8.0, 12.0), 40.0, false, false);
        let title = if sc.label.is_empty() {
            chip.clone()
        } else {
            sc.label.clone()
        };
        let mut text: Vec<Element> = vec![text_block(title)
            .vertical_alignment(VerticalAlignment::Center)
            .into()];
        if !sc.label.is_empty() {
            text.push(
                text_block(chip)
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText)
                    .into(),
            );
        }
        rows.push(
            hstack((
                face,
                vstack(text)
                    .spacing(2.0)
                    .vertical_alignment(VerticalAlignment::Center),
                button("Edit").subtle().on_click({
                    let (ui, set_ui, sc) = (ui.clone(), set_ui.clone(), sc.clone());
                    move || {
                        let mut u = ui.clone();
                        u.draft = Some(Draft::of(&sc));
                        u.capture = false;
                        set_ui.call(u);
                    }
                }),
            ))
            .spacing(12.0)
            .into(),
        );
    }
    // The reset is two presses, and it says so on the button — the console's own armed row,
    // and the same wording every other client's reset carries.
    let reset = button(if ui.reset_armed {
        "Press again to reset"
    } else {
        "Reset to default"
    })
    // One mark either way: the accent fill below is what says it is armed, and the glyph takes
    // the on-accent brush by itself.
    .icon(lucide::icon("rotate-cw"))
    .tooltip("Restores the platform dial and removes the shortcuts")
    .on_click({
        let (props, ui, set_ui) = (props.clone(), ui.clone(), set_ui.clone());
        move || {
            if ui.reset_armed {
                commit(
                    &props.ctx,
                    &props.scope,
                    "overlay_actions",
                    (props.rev, &props.set_rev),
                    &props.set_status,
                    |s| s.overlay_actions.clear(),
                );
            }
            let mut u = ui.clone();
            u.reset_armed = !ui.reset_armed;
            set_ui.call(u);
        }
    });
    let actions = hstack((
        button("Add shortcut").icon(lucide::icon("plus")).on_click({
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            move || {
                let mut u = ui.clone();
                u.draft = Some(Draft::default());
                u.capture = false;
                u.reset_armed = false;
                set_ui.call(u);
            }
        }),
        if ui.reset_armed {
            reset.accent()
        } else {
            reset
        },
    ))
    .spacing(8.0)
    .into();
    let mut parts: Vec<Element> = vec![section("Shortcuts")];
    if rows.is_empty() {
        parts.push(
            text_block("A new shortcut takes the first empty dial slot.")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
                .into(),
        );
    }
    parts.extend(rows);
    parts.push(actions);
    card(vstack(parts).spacing(10.0)).into()
}

/// The shortcut editor: the disc as the ring will draw it, the name, the modifiers as
/// toggles, the key on a keyboard-shaped grid or captured from the real keyboard, Save,
/// Remove, Cancel.
fn shortcut_editor(
    props: &Props,
    cfg: &OverlayConfig,
    draft: &Draft,
    ui: &Ui,
    set_ui: &SetState<Ui>,
) -> Element {
    let keys = draft.chord.keys();
    let chip = chord_chip(&keys);
    let ink = Color {
        a: 242,
        r: 255,
        g: 255,
        b: 255,
    };
    let preview = hstack((
        disc(
            keycap(&keys, ink, 9.0, 14.0),
            SLOT_DIAMETER as f64,
            false,
            false,
        ),
        vstack((
            text_block(if chip.is_empty() {
                "Pick a key".to_string()
            } else {
                chip.clone()
            })
            .font_size(20.0)
            .semibold(),
            text_block("How the dial will draw it")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(2.0)
        .vertical_alignment(VerticalAlignment::Center),
    ))
    .spacing(16.0);

    let set_draft = |ui: &Ui, set_ui: &SetState<Ui>, f: &dyn Fn(&mut Draft)| {
        let mut u = ui.clone();
        if let Some(d) = u.draft.as_mut() {
            f(d);
        }
        set_ui.call(u);
    };

    let name = text_box(draft.label.clone())
        .header("Name")
        .placeholder_text("Optional — e.g. Task Manager")
        .on_text_changed({
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            move |text: String| set_draft(&ui, &set_ui, &|d| d.label = text.clone())
        })
        .max_width(320.0)
        .horizontal_alignment(HorizontalAlignment::Left);

    let mut mods: Vec<Element> = CHORD_MODIFIERS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            toggle_button(key_legend(m), draft.chord.mods[i])
                .on_checked({
                    let (ui, set_ui) = (ui.clone(), set_ui.clone());
                    move |on: bool| set_draft(&ui, &set_ui, &|d| d.chord.mods[i] = on)
                })
                .into()
        })
        .collect();
    mods.push(
        toggle_button("Press the chord…", ui.capture)
            .on_checked({
                let (ui, set_ui) = (ui.clone(), set_ui.clone());
                move |on: bool| {
                    let mut u = ui.clone();
                    u.capture = on;
                    set_ui.call(u);
                }
            })
            .tooltip(
                "Then press the keys on your keyboard (Ctrl, Alt and Shift; Win stays a toggle)",
            )
            .margin(edges(12.0, 0.0, 0.0, 0.0))
            .into(),
    );

    let grid: Vec<Element> = KEY_GRID
        .iter()
        .map(|row| {
            let keys: Vec<Element> = row
                .iter()
                .map(|name| {
                    toggle_button(key_legend(name), draft.chord.key.as_deref() == Some(*name))
                        .on_checked({
                            let (ui, set_ui) = (ui.clone(), set_ui.clone());
                            move |on: bool| {
                                if on {
                                    set_draft(&ui, &set_ui, &|d| {
                                        d.chord.key = Some(name.to_string())
                                    });
                                }
                            }
                        })
                        .into()
                })
                .collect();
            hstack(keys)
                .spacing(4.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into()
        })
        .collect();

    let mut actions: Vec<Element> = vec![button(if draft.id.is_some() {
        "Save"
    } else {
        "Add shortcut"
    })
    .accent()
    .enabled(draft.chord.key.is_some())
    .on_click({
        let (props, cfg, ui, set_ui, d) = (
            props.clone(),
            cfg.clone(),
            ui.clone(),
            set_ui.clone(),
            draft.clone(),
        );
        move || {
            let mut next = cfg.clone();
            next.upsert_shortcut(d.id.as_deref(), &d.label, d.chord.keys());
            write(&props, &next);
            let mut u = ui.clone();
            u.draft = None;
            u.capture = false;
            set_ui.call(u);
        }
    })
    .into()];
    if let Some(id) = &draft.id {
        actions.push(
            button("Remove shortcut")
                .on_click({
                    let (props, cfg, ui, set_ui, id) = (
                        props.clone(),
                        cfg.clone(),
                        ui.clone(),
                        set_ui.clone(),
                        id.clone(),
                    );
                    move || {
                        let mut next = cfg.clone();
                        next.remove_shortcut(&id);
                        write(&props, &next);
                        let mut u = ui.clone();
                        u.draft = None;
                        u.capture = false;
                        set_ui.call(u);
                    }
                })
                .into(),
        );
    }
    actions.push(
        button("Cancel")
            .subtle()
            .on_click({
                let (ui, set_ui) = (ui.clone(), set_ui.clone());
                move || {
                    let mut u = ui.clone();
                    u.draft = None;
                    u.capture = false;
                    set_ui.call(u);
                }
            })
            .into(),
    );

    let mut el: Element = card(
        vstack((
            section(if draft.id.is_some() {
                "Shortcut"
            } else {
                "New shortcut"
            }),
            preview,
            name,
            text_block("Hold the modifiers marked on, then the key.")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            hstack(mods).spacing(6.0),
            vstack(grid).spacing(4.0),
            hstack(actions).spacing(8.0),
        ))
        .spacing(12.0),
    )
    .into();

    // Pressing the chord on the real keyboard fills the modifiers and the key in one go: while
    // the capture is on, an accelerator per key and modifier mix on this card takes it.
    if ui.capture {
        for row in KEY_GRID {
            for name in row {
                let Some(vk) = virtual_key(name) else {
                    continue;
                };
                for mask in modifier_masks() {
                    let (ui, set_ui) = (ui.clone(), set_ui.clone());
                    el = el.keyboard_accelerator(KeyboardAccelerator::new(
                        vk,
                        VirtualKeyModifiers(mask),
                        move || {
                            let mut u = ui.clone();
                            if let Some(d) = u.draft.as_mut() {
                                // Win is never captured from the keyboard: its toggle stands.
                                let win = d.chord.mods[3];
                                d.chord.mods = mods_of(mask);
                                d.chord.mods[3] = win;
                                d.chord.key = Some(name.to_string());
                            }
                            u.capture = false;
                            set_ui.call(u);
                        },
                    ));
                }
            }
        }
    }
    el
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every disc the ring can hold draws as a mark, not a word, and that mark has a glyph.
    /// The editor has no fallback to fall back TO now that the words are gone — a slot whose
    /// mark is missing renders an empty disc.
    #[test]
    fn every_slot_on_the_ring_draws_a_glyph() {
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        for slot in cfg.ring.iter().flatten() {
            let id = slot.id();
            let name = slot_icon(&id, "").unwrap_or_else(|| panic!("{id} has no mark"));
            assert!(
                !lucide::glyph(name).is_empty(),
                "{id}: the font has no '{name}'"
            );
        }
        // The centre and the empty slot draw marks of their own, outside the ring's contents.
        assert_eq!(slot_icon("more", ""), Some("ellipsis"));
        assert!(!lucide::glyph("ellipsis").is_empty());
        assert!(!lucide::glyph("plus").is_empty());
    }

    /// A press on a disc is that disc, a press between them is nothing, and the drag slop
    /// separates a click from a carry.
    #[test]
    fn discs_are_hit_within_slop_and_a_carry_needs_travel() {
        let (x, y) = disc_centre(0);
        assert_eq!(disc_at(x, y), Some(0));
        assert_eq!(
            disc_at(x + 30.0, y),
            Some(0),
            "just past the edge still hits"
        );
        assert_eq!(
            disc_at(STAGE_W / 2.0, STAGE_H / 2.0),
            None,
            "the centre is inert"
        );
        let d = Drag {
            slot: 0,
            x0: 0.0,
            y0: 0.0,
            dx: 3.0,
            dy: 3.0,
        };
        assert!(!d.moved());
        let d = Drag { dx: 20.0, ..d };
        assert!(d.moved());
    }

    /// Every key on the grid has a virtual key for the capture, and a draft reads a shortcut
    /// back the way the editor wrote it.
    #[test]
    fn the_grid_maps_to_virtual_keys_and_the_draft_round_trips() {
        for row in KEY_GRID {
            for name in row {
                assert!(virtual_key(name).is_some(), "{name}");
            }
        }
        assert_eq!(virtual_key("a"), Some(VirtualKey::A));
        assert_eq!(virtual_key("0"), Some(VirtualKey::Number0));
        assert_eq!(virtual_key("f12"), Some(VirtualKey::F12));
        assert_eq!(virtual_key("win"), None);
        let sc = Shortcut {
            id: "s1".into(),
            label: "Tasks".into(),
            keys: vec!["ctrl".into(), "shift".into(), "escape".into()],
        };
        let d = Draft::of(&sc);
        assert_eq!(d.chord.mods, [true, false, true, false]);
        assert_eq!(d.chord.keys(), sc.keys);
        assert_eq!(
            mods_of(VirtualKeyModifiers::Control.0 | VirtualKeyModifiers::Shift.0),
            [true, false, true, false]
        );
    }
}
