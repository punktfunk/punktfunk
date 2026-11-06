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
use super::{AppCtx, AsyncSetState};
use pf_client_core::overlay_actions::{
    catalogue, chord_chip, key_legend, slot_icon, Chord, OverlayConfig, RingPlatform, Shortcut,
    SlotId, CHORD_MODIFIERS, KEY_GRID, RING_SLOTS,
};
use pf_client_core::ring::{slot_offset, CENTRE_DIAMETER, RING_RADIUS, SLOT_DIAMETER};
use pf_fluent::Palette;
use pf_l10n::{client, common, desktop};
use punktfunk_core::input::key_vk;
use std::rc::Rc;
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

/// The hour each disc sits at on the ring, which a screen reader reads out.
const CLOCK_HOURS: [u8; RING_SLOTS] = [12, 2, 4, 6, 8, 10];

/// What the settings page hands the editor: the app, its palette, the scope being edited, the
/// revision counter every commit bumps (the page repaints on it) and the status line a failed
/// save lands on.
#[derive(Clone)]
pub(super) struct Props {
    pub ctx: Arc<AppCtx>,
    pub pal: Palette,
    pub scope: String,
    pub rev: u64,
    pub set_rev: AsyncSetState<u64>,
    pub set_status: AsyncSetState<String>,
}

impl PartialEq for Props {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ctx, &other.ctx)
            && self.pal == other.pal
            && self.scope == other.scope
            && self.rev == other.rev
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
pub(super) struct Ui {
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
        SlotId::EndStream => client::ring_end_stream_short().into(),
        SlotId::EndGame => client::ring_end_game_short().into(),
        SlotId::DisconnectLinger => client::ring_disconnect_linger_short().into(),
        SlotId::TouchMode => client::ring_touch_mode_short().into(),
        SlotId::Keyboard => client::ring_keyboard_short().into(),
        SlotId::Stats => client::ring_stats_short().into(),
        SlotId::Mic => client::ring_mic_short().into(),
        SlotId::Pad => client::ring_pad_short().into(),
        SlotId::SendText => client::ring_send_text_short().into(),
        SlotId::Guide => client::ring_guide_short().into(),
        SlotId::Qam => client::ring_qam_short().into(),
        SlotId::PadMouse => client::ring_pad_mouse_short().into(),
        SlotId::PadType => client::ring_pad_type_short().into(),
        SlotId::StreamMute => client::ring_stream_mute_short().into(),
        SlotId::SwapScreens => client::ring_swap_screens_short().into(),
        SlotId::Host(_) => client::ring_host_short().into(),
        SlotId::Shortcut(id) => cfg
            .shortcut(id)
            .map(|s| chord_chip(&s.keys))
            .unwrap_or_default(),
    }
}

/// A slot's name with its availability note after it, where there is one.
fn with_note(label: &str, note: &str) -> String {
    if note.is_empty() {
        label.to_string()
    } else {
        format!("{label} \u{2014} {note}")
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

/// A key event's Ctrl, Alt and Shift as a chord's modifier row. The Windows key is the
/// toggle's: the shell never sees a chord that Windows itself owns.
fn mods_of(m: InputModifiers) -> [bool; 4] {
    [
        m.contains(InputModifiers::CONTROL),
        m.contains(InputModifiers::ALT),
        m.contains(InputModifiers::SHIFT),
        false,
    ]
}

/// The grid key a pressed virtual key is, by the VK the session injects for each name.
fn grid_key(vk: u32) -> Option<&'static str> {
    KEY_GRID
        .iter()
        .flat_map(|row| row.iter().copied())
        .find(|name| key_vk(name).map(u32::from) == Some(vk))
}

// ---- the section ----

/// Where the editor sends its next state.
type SetUi = Callback<Ui>;

/// The Quick actions section: the ring, its picker, the shortcuts and their editor. Its
/// message is the editor's whole next state.
pub(super) struct QuickActions {
    ui: Ui,
}

impl Component for QuickActions {
    type Input = Props;
    type Message = Ui;

    fn create(_props: &Props, _cx: &ComponentContext<Self>) -> Self {
        QuickActions { ui: Ui::default() }
    }

    fn update(&mut self, ui: Ui, _cx: &ComponentContext<Self>) {
        self.ui = ui;
    }

    fn view(&self, props: &Props, cx: &mut ViewContext<'_, Self>) -> View {
        let (ui, set_ui) = (&self.ui, &cx.forward());
        let cfg = current(props);
        let mut parts: Vec<View> = vec![
            muted(
                &props.pal,
                format!(
                    "{} {}",
                    desktop::desktop_ring_editor_hint(),
                    desktop::desktop_ring_editor_open_hint("Ctrl+Alt+Shift+O", "Select+A"),
                ),
            )
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .max_width(560.0)
            .horizontal_alignment(HorizontalAlignment::Left)
            .into(),
            ring(props, &cfg, ui, set_ui),
            ring_label(&cfg, ui),
            legend_row(&cfg, ui, set_ui),
        ];
        if let Some(k) = ui.selected {
            parts.push(picker(props, &cfg, k, ui, set_ui));
        }
        parts.push(match &ui.draft {
            Some(d) => shortcut_editor(props, &cfg, d, ui, set_ui),
            None => shortcuts(props, &cfg, ui, set_ui),
        });
        vstack(parts).spacing(14.0).into()
    }
}

/// A disc's Lucide mark, as a glyph of the icon font at exactly `size`. Not [`lucide::icon`]:
/// that hands the mark to a control to size and tint, and a disc is neither — it is our own
/// dark surface on both themes, so the ink is white whatever the theme says, and the size is
/// the ring's geometry rather than a button's. `dim` is the unavailable slot, faded not hidden.
fn mark(name: &str, size: f64, dim: bool) -> View {
    let ink = Color::argb(if dim { 100 } else { 242 }, 255, 255, 255);
    text(lucide::glyph(name))
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
fn disc_face(cfg: &OverlayConfig, slot: Option<&SlotId>, dim: bool) -> View {
    let ink = Color::argb(if dim { 100 } else { 242 }, 255, 255, 255);
    let Some(slot) = slot else {
        return mark("plus", ICON_DIP * 0.8, true);
    };
    if let SlotId::Shortcut(id) = slot {
        let keys = cfg.shortcut(id).map(|s| s.keys.clone()).unwrap_or_default();
        return keycap(&keys, ink, 9.0, 14.0);
    }
    if let Some(name) = slot_icon(&slot.id(), false) {
        return mark(name, ICON_DIP, dim);
    }
    text(short_label(cfg, slot))
        .font_size(12.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .foreground(ink)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// A chord as a stacked keycap: the modifiers small on top, the key large under them.
fn keycap(keys: &[String], ink: Color, small: f64, big: f64) -> View {
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
    let mut lines: Vec<View> = Vec::new();
    if !mods.is_empty() {
        lines.push(
            text(mods)
                .font_size(small)
                .foreground(ink)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    lines.push(
        text(key)
            .font_size(big)
            .font_weight(FontWeight::BOLD)
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
fn disc(face: View, diameter: f64, raised: bool, selected: bool) -> Border {
    Border::new()
        .content(face)
        .width(diameter)
        .height(diameter)
        .corner_radius(diameter / 2.0)
        .background(Color::argb(if raised { 190 } else { 140 }, 0, 0, 0))
        .border_brush(Color::argb(if selected { 230 } else { 46 }, 255, 255, 255))
        .border_thickness(uniform(if selected { 2.0 } else { 1.0 }))
}

/// The ring on its stage: the discs at the ring's geometry, the inert centre, and the stage's
/// pointer handlers — a carry lives on the stage, not the disc, because a pointer that leaves
/// a disc stops reporting to it. The carried disc is drawn last, so it rides over the others.
fn ring(props: &Props, cfg: &OverlayConfig, ui: &Ui, set_ui: &SetUi) -> View {
    // The stage is a flat card face, like every other card on this shell.
    let mut children: Vec<View> = vec![blank(STAGE_W, STAGE_H)
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(22.0)
        .into()];
    // The centre: what the sheet opens from in-stream — not editable here, dimmed and inert.
    // It wears the ring's own `more` mark, so the editor's centre is the centre people press.
    let cd = CENTRE_DIAMETER as f64;
    children.push(
        disc(mark("ellipsis", ICON_DIP, true), cd, false, false)
            .canvas_left(STAGE_W / 2.0 - cd / 2.0)
            .canvas_top(STAGE_H / 2.0 - cd / 2.0)
            .tooltip(desktop::desktop_ring_centre_tooltip()),
    );
    let d = SLOT_DIAMETER as f64;
    let mut carried_disc = None;
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
            .unwrap_or((client::ring_slot_empty().into(), String::new()));
        let face = disc_face(cfg, slot, !note.is_empty());
        let el = disc(face, d, carried.is_some(), ui.selected == Some(k))
            .canvas_left(x - d / 2.0)
            .canvas_top(y - d / 2.0)
            .tooltip(with_note(&label, &note));
        if carried.is_some() {
            carried_disc = Some(el);
        } else {
            children.push(el);
        }
    }
    children.extend(carried_disc);

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
        move |_: PointerEventInfo| {
            if ui.drag.is_some() || ui.hover.is_some() {
                let mut u = ui.clone();
                u.drag = None;
                u.hover = None;
                set_ui.call(u);
            }
        }
    };
    Border::new()
        .content(
            Canvas::new()
                .children(children)
                .width(STAGE_W)
                .height(STAGE_H),
        )
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
fn ring_label(cfg: &OverlayConfig, ui: &Ui) -> View {
    let line = ui
        .hover
        .or(ui.selected)
        .and_then(|k| cfg.ring[k].as_ref())
        .map(|slot| {
            let (label, note) = describe(cfg, slot);
            with_note(&label, &note)
        })
        .unwrap_or_default();
    text(line)
        .font_size(13.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .height(20.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

/// Six buttons under the ring, one per disc, so a keyboard and a screen reader reach every
/// slot the pointer reaches.
fn legend_row(cfg: &OverlayConfig, ui: &Ui, set_ui: &SetUi) -> View {
    let buttons: Vec<View> = (0..RING_SLOTS)
        .map(|k| {
            let slot = cfg.ring[k].as_ref();
            let (label, _) = slot
                .map(|s| describe(cfg, s))
                .unwrap_or((client::ring_slot_empty().into(), String::new()));
            let short = slot.map_or(client::ring_slot_empty().to_string(), |s| {
                short_label(cfg, s)
            });
            let b = button(short)
                .automation_name(&label)
                .automation_help_text(client::ring_slot_clock(CLOCK_HOURS[k]))
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
            b.style(if ui.selected == Some(k) {
                ButtonStyle::Accent
            } else {
                ButtonStyle::Subtle
            })
            .tooltip(label)
        })
        .collect();
    hstack(buttons)
        .spacing(6.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

/// The picker for one disc: the catalogue by group, the current pick marked, a note where a
/// slot cannot work on this shell, and the swap targets for a keyboard.
fn picker(props: &Props, cfg: &OverlayConfig, k: usize, ui: &Ui, set_ui: &SetUi) -> View {
    let current = cfg.ring[k].as_ref().map(SlotId::id).unwrap_or_default();
    // No title and no Done: the band above already names the slot being edited, the disc is
    // ringed white while its picker is open, and a pick closes the card — the GTK popover's
    // behaviour, which needed neither either.
    let mut parts: Vec<View> = Vec::new();
    for group in catalogue(cfg, RingPlatform::Desktop) {
        parts.push(section(&props.pal, group.title));
        let mut row: Vec<View> = Vec::new();
        for entry in group.entries {
            let is_current = entry.id == current;
            let unavailable =
                !entry.note.is_empty() && !entry.id.starts_with("host:") && !is_current;
            let line = with_note(&entry.label, &entry.note);
            let mut b = button(line).on_click({
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
                b.style(ButtonStyle::Accent)
            } else {
                b.style(ButtonStyle::Subtle).is_enabled(!unavailable)
            };
            row.push(b.into());
        }
        parts.push(wrap_row(row));
    }
    card(vstack(parts).spacing(8.0)).into()
}

/// Buttons that wrap into rows of at most five, so a group never runs off the card.
fn wrap_row(buttons: Vec<View>) -> View {
    let rows: Vec<View> = buttons
        .chunks(5)
        .map(|chunk| hstack(chunk.to_vec()).spacing(6.0).into())
        .collect();
    vstack(rows).spacing(6.0).into()
}

/// The shortcuts under the ring: a row each with its keycap, name and legend; Add; Reset.
fn shortcuts(props: &Props, cfg: &OverlayConfig, ui: &Ui, set_ui: &SetUi) -> View {
    let mut rows: Vec<View> = Vec::new();
    for sc in &cfg.shortcuts {
        let chip = chord_chip(&sc.keys);
        let ink = Color::argb(242, 255, 255, 255);
        let face = disc(keycap(&sc.keys, ink, 8.0, 12.0), 40.0, false, false);
        let title = if sc.label.is_empty() {
            chip.clone()
        } else {
            sc.label.clone()
        };
        let mut lines: Vec<View> = vec![text(title)
            .vertical_alignment(VerticalAlignment::Center)
            .into()];
        if !sc.label.is_empty() {
            lines.push(muted(&props.pal, chip).font_size(12.0).into());
        }
        rows.push(
            hstack((
                face,
                vstack(lines)
                    .spacing(2.0)
                    .vertical_alignment(VerticalAlignment::Center),
                button(common::common_edit())
                    .style(ButtonStyle::Subtle)
                    .on_click({
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
    // One mark either way: the accent fill is what says it is armed.
    let reset = icon_button(
        "rotate-cw",
        if ui.reset_armed {
            client::ring_reset_confirm()
        } else {
            client::ring_reset()
        },
    )
    .style(if ui.reset_armed {
        ButtonStyle::Accent
    } else {
        ButtonStyle::Default
    })
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
    })
    .tooltip(desktop::desktop_ring_reset_tooltip());
    let actions = hstack((
        icon_button("plus", client::ring_shortcut_add()).on_click({
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            move || {
                let mut u = ui.clone();
                u.draft = Some(Draft::default());
                u.capture = false;
                u.reset_armed = false;
                set_ui.call(u);
            }
        }),
        reset,
    ))
    .spacing(8.0)
    .into();
    let mut parts: Vec<View> = vec![section(&props.pal, client::ring_editor_shortcuts())];
    if rows.is_empty() {
        parts.push(
            muted(&props.pal, client::ring_editor_shortcuts_hint())
                .font_size(12.0)
                .text_wrapping(TextWrapping::Wrap)
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
    set_ui: &SetUi,
) -> View {
    let pal = &props.pal;
    let keys = draft.chord.keys();
    let chip = chord_chip(&keys);
    let ink = Color::argb(242, 255, 255, 255);
    let preview = hstack((
        disc(
            keycap(&keys, ink, 9.0, 14.0),
            SLOT_DIAMETER as f64,
            false,
            false,
        ),
        vstack((
            text(if chip.is_empty() {
                client::ring_shortcut_no_key().to_string()
            } else {
                chip.clone()
            })
            .font_size(20.0)
            .font_weight(FontWeight::SEMI_BOLD),
            muted(pal, client::ring_shortcut_preview()).font_size(12.0),
        ))
        .spacing(2.0)
        .vertical_alignment(VerticalAlignment::Center),
    ))
    .spacing(16.0);

    let set_draft = |ui: &Ui, set_ui: &SetUi, f: &dyn Fn(&mut Draft)| {
        let mut u = ui.clone();
        if let Some(d) = u.draft.as_mut() {
            f(d);
        }
        set_ui.call(u);
    };

    let name = TextBox::new()
        .text(&draft.label)
        .header(client::ring_shortcut_name())
        .placeholder_text(client::ring_shortcut_name_placeholder())
        .on_text_changed({
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            move |line: Rc<str>| set_draft(&ui, &set_ui, &|d| d.label = line.to_string())
        })
        .max_width(320.0)
        .horizontal_alignment(HorizontalAlignment::Left);

    let mut mods: Vec<View> = CHORD_MODIFIERS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            toggle(key_legend(m), draft.chord.mods[i], move |on| {
                set_draft(&ui, &set_ui, &|d| d.chord.mods[i] = on)
            })
            .into()
        })
        .collect();
    mods.push(
        {
            let (ui, set_ui) = (ui.clone(), set_ui.clone());
            toggle(desktop::desktop_shortcut_capture(), ui.capture, move |on| {
                let mut u = ui.clone();
                u.capture = on;
                set_ui.call(u);
            })
        }
        .margin(edges(12.0, 0.0, 0.0, 0.0))
        .tooltip(desktop::desktop_shortcut_capture_tooltip()),
    );

    let grid: Vec<View> = KEY_GRID
        .iter()
        .map(|row| {
            let keys: Vec<View> = row
                .iter()
                .map(|name| {
                    let (ui, set_ui) = (ui.clone(), set_ui.clone());
                    let on = draft.chord.key.as_deref() == Some(*name);
                    toggle(key_legend(name), on, move |on| {
                        if on {
                            set_draft(&ui, &set_ui, &|d| d.chord.key = Some(name.to_string()));
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

    let mut actions: Vec<View> = vec![button(if draft.id.is_some() {
        common::common_save()
    } else {
        common::common_add()
    })
    .style(ButtonStyle::Accent)
    .is_enabled(draft.chord.key.is_some())
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
            button(client::ring_shortcut_remove())
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
        button(common::common_cancel())
            .style(ButtonStyle::Subtle)
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

    let editor = card(
        vstack((
            section(
                pal,
                if draft.id.is_some() {
                    client::ring_shortcut_title()
                } else {
                    client::ring_shortcut_new_title()
                },
            ),
            preview,
            name,
            muted(pal, client::ring_shortcut_modifiers_hint()).font_size(12.0),
            hstack(mods).spacing(6.0),
            vstack(grid).spacing(4.0),
            hstack(actions).spacing(8.0),
        ))
        .spacing(12.0),
    );
    if !ui.capture {
        return editor.into();
    }
    // Pressing the chord on the real keyboard fills the modifiers and the key in one go: while
    // the capture is on, the card takes the first press of a key on the grid.
    let (ui, set_ui) = (ui.clone(), set_ui.clone());
    editor
        .on_preview_key_down(RoutedCallback::new(move |k: KeyEventInfo| {
            let Some(name) = grid_key(k.key.0) else {
                return false;
            };
            let mut u = ui.clone();
            if let Some(d) = u.draft.as_mut() {
                // Win is never captured from the keyboard: its toggle stands.
                let win = d.chord.mods[3];
                d.chord.mods = mods_of(k.modifiers);
                d.chord.mods[3] = win;
                d.chord.key = Some(name.to_string());
            }
            u.capture = false;
            set_ui.call(u);
            true
        }))
        .into()
}

/// A toggle button that hands its handler the new state.
fn toggle(label: impl Into<View>, on: bool, changed: impl Fn(bool) + 'static) -> ToggleButton {
    ToggleButton::new()
        .content(label)
        .is_checked(on)
        .on_is_checked_changed(move |v: Option<bool>| changed(v == Some(true)))
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
            let name = slot_icon(&id, false).unwrap_or_else(|| panic!("{id} has no mark"));
            assert!(
                !lucide::glyph(name).is_empty(),
                "{id}: the font has no '{name}'"
            );
        }
        // The centre and the empty slot draw marks of their own, outside the ring's contents.
        assert_eq!(slot_icon("more", false), Some("ellipsis"));
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

    /// A pressed key finds its grid name, and a draft reads a shortcut back the way the editor
    /// wrote it.
    #[test]
    fn a_press_finds_its_grid_key_and_the_draft_round_trips() {
        for row in KEY_GRID {
            for name in row {
                let vk = key_vk(name).map(u32::from);
                assert_eq!(vk.and_then(grid_key), Some(*name), "{name}");
            }
        }
        assert_eq!(grid_key(VirtualKey::A.0), Some("a"));
        assert_eq!(
            grid_key(VirtualKey::CONTROL.0),
            None,
            "a modifier alone is no key"
        );
        let sc = Shortcut {
            id: "s1".into(),
            label: "Tasks".into(),
            keys: vec!["ctrl".into(), "shift".into(), "escape".into()],
        };
        let d = Draft::of(&sc);
        assert_eq!(d.chord.mods, [true, false, true, false]);
        assert_eq!(d.chord.keys(), sc.keys);
        let mut m = InputModifiers::CONTROL;
        m |= InputModifiers::SHIFT;
        assert_eq!(mods_of(m), [true, false, true, false]);
    }
}
