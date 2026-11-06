//! Controllers: the connected pads as cards, each drawn as itself at rest, the keyboards and
//! mice the host sees folded into one card, and on Android a last "Not showing?" card that
//! opens the grants and tests only the host can perform ([`super::grants`]). The fourth tab.
//!
//! Cards are one focus row that glides to keep the focused card in view and takes a
//! finger's swipe. OK, or a hold, on a pad opens its menu ([`super::pad_menu`]): the live
//! input test where the host can run it ([`super::input_test`]) and a rumble pulse where it
//! holds the device. Only devices the OS classifies as a gamepad are forwarded; adapters
//! often enumerate as something else, so each card's identity line is the support answer.
//!
//! A rumble pulse on a real device stays with the host via
//! [`crate::model::ConsoleCmd::PadAction`]; the
//! desktop has no device handle, so it lists pads and nothing more.
//! Seat order joins when `seats.rs` lands (`console-ui-redesign.md` §2).

use super::input_test::{art_for, draw_pad, Drawing};
use crate::el::{Axis, El, Group, Id, Tree};
use crate::glyphs::{device_icon, Hint};
use crate::model::{OtherDevice, PadTestState};
use crate::platform::Platform;
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::theme::{Fonts, Frame, PanelStroke, W};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

/// Card size and gap, design units.
const CARD_W: f64 = 300.0;
const CARD_H: f64 = 300.0;
const CARD_GAP: f64 = 24.0;
const CARD_CORNER: f64 = 22.0;
/// Key-device silhouette box, dp, on the no-pads card.
const MARK: f64 = 44.0;
/// The pad drawing's widest, dp; the room above the card's text clamps it by height.
const ART_W: f64 = 240.0;

/// Host-only pad work: a permission dialog or a real device handle. Neither
/// exists on this side of the bridge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PadAction {
    /// Diagnostic: is the focused pad's motor wired.
    Rumble,
    /// Always shown: without `BLUETOOTH_CONNECT` a BLE SC2 is absent, not idle —
    /// detection cannot run, so hiding the row behind it would hide the grant.
    Sc2Bluetooth,
    Sc2Usb,
    DsUsb,
    /// Ungated by session: a stream that is not sending haptics must not hide
    /// the pad-audio test that rules the pad out.
    DsHaptics,
}

impl PadAction {
    /// Stable id the host matches on (JNI inside [`ConsoleCmd::PadAction`]).
    pub(crate) fn id(self) -> &'static str {
        match self {
            PadAction::Rumble => "rumble",
            PadAction::Sc2Bluetooth => "sc2_bluetooth",
            PadAction::Sc2Usb => "sc2_usb",
            PadAction::DsUsb => "ds_usb",
            PadAction::DsHaptics => "ds_haptics",
        }
    }
}

/// A row's label or verb.
pub(crate) type Text = fn() -> &'static str;

/// Grant/test rows, Android's alone: the desktop has no USB capture and no grants.
pub(crate) const PASSTHROUGH: [(PadAction, Text, Text); 4] = {
    use pf_l10n::{client as m, console as c};
    [
        (
            PadAction::Sc2Bluetooth,
            m::pad_sc2_bluetooth,
            c::console_pad_grant,
        ),
        (
            PadAction::Sc2Usb,
            c::console_pad_sc2_usb,
            c::console_pad_grant,
        ),
        (
            PadAction::DsUsb,
            c::console_pad_ds_usb,
            c::console_pad_grant,
        ),
        (
            PadAction::DsHaptics,
            c::console_pad_ds_haptics,
            c::console_pad_test,
        ),
    ]
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Pad(usize),
    /// The inert card that stands in for no pads, so the row is never empty.
    NoPads,
    /// Every keyboard, mouse or other device the host reports, in one card: never pads to
    /// the host, and not what this row is about.
    Others,
    /// The last card on Android: OK opens the grants and tests.
    Grants,
}

/// The hosts that send a pad's readings for the input test.
fn can_test(platform: Platform) -> bool {
    matches!(platform, Platform::Android | Platform::Apple)
}

fn targets(ctx: &Ctx, others: &[OtherDevice]) -> Vec<Target> {
    let mut all: Vec<Target> = if ctx.pads.is_empty() {
        vec![Target::NoPads]
    } else {
        (0..ctx.pads.len()).map(Target::Pad).collect()
    };
    if !others.is_empty() {
        all.push(Target::Others);
    }
    if ctx.device.platform == Platform::Android {
        all.push(Target::Grants);
    }
    all
}

/// The element id of a target: pads by key, so focus follows a pad through a reorder.
fn target_id(t: Target, pads: &[PadInfo]) -> Id {
    match t {
        Target::Pad(i) => Id::new(&pads[i].key, 0),
        Target::NoPads => Id::new("no-pads", 0),
        Target::Others => Id::new("other-devices", 0),
        Target::Grants => Id::new("grants", 0),
    }
}

/// Only a host with a device handle can pulse a motor.
fn can_rumble(pad: &PadInfo, platform: Platform) -> bool {
    pad.rumble && matches!(platform, Platform::Android | Platform::Apple)
}

/// The card row's scroll.
fn strip() -> Id {
    Id::new("cards", 0)
}

pub(crate) struct PlayersScreen {
    tree: Tree,
    /// Set by the shell each frame from what the host reports.
    pub(crate) others: Vec<OtherDevice>,
    /// The first frame seats the row on its focus; later frames glide.
    snap: bool,
    /// One parsed drawing per pad family on the row; the row redraws every frame.
    drawings: Vec<Drawing>,
}

impl PlayersScreen {
    pub(crate) fn new() -> PlayersScreen {
        PlayersScreen {
            tree: Tree::new(),
            others: Vec::new(),
            snap: true,
            drawings: Vec::new(),
        }
    }

    /// The focused target: the tree's, or the first when focus left with its pad.
    fn focused(&self, ctx: &Ctx) -> Target {
        let all = targets(ctx, &self.others);
        self.tree
            .focus()
            .and_then(|id| all.iter().copied().find(|t| target_id(*t, ctx.pads) == id))
            .unwrap_or(all[0])
    }
}

impl ScreenView for PlayersScreen {
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let at = self.focused(ctx);
        self.tree.set_focus(Some(target_id(at, ctx.pads)));
        match ev {
            MenuEvent::Back => {
                fx.pop();
                None
            }
            // Up past the cards is the tab strip's: the shell reads the Boundary.
            MenuEvent::Move(dir) => match self.tree.move_focus(dir) {
                Some(_) => Some(MenuPulse::Move),
                None => Some(MenuPulse::Boundary),
            },
            // A hold (OK held, Y, a long press) opens the card's menu as OK does.
            MenuEvent::Confirm | MenuEvent::Secondary => activate(at, ctx, fx),
            _ => None,
        }
    }

    /// Hover focuses; a press acts on what it lands on.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let Some(id) = self.tree.hit(p.x as f32, p.y as f32) else {
            return false;
        };
        let Some(t) = targets(ctx, &self.others)
            .into_iter()
            .find(|t| target_id(*t, ctx.pads) == id)
        else {
            return false;
        };
        self.tree.set_focus(Some(id));
        if p.kind == PointerKind::Press {
            activate(t, ctx, fx);
        }
        true
    }

    /// A finger swipe on the row.
    fn pan(&mut self, p: Pointer) -> bool {
        self.tree.drag(strip(), p)
    }

    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        Some(match self.focused(ctx) {
            Target::Pad(i) => format!("{}, {}", ctx.pads[i].name, pad_detail(&ctx.pads[i])),
            Target::NoPads => pf_l10n::console::console_pad_none_connected_a11y().into(),
            Target::Others => others_lines(&self.others).0,
            Target::Grants => pf_l10n::console::console_pad_grants_card_a11y().into(),
        })
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        Vec::new()
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
        let all = targets(ctx, &self.others);
        // The first target takes focus once; a card that goes reseats it in the tree.
        if self.tree.focus().is_none() {
            self.tree.set_focus(Some(target_id(all[0], ctx.pads)));
        }
        let (platform, pads) = (ctx.device.platform, ctx.pads);
        for art in pads.iter().map(|p| art_for(Some(p.pref))) {
            if !self.drawings.iter().any(|d| std::ptr::eq(d.art, art)) {
                self.drawings.push(Drawing::new(art));
            }
        }
        let drawings = &self.drawings;
        let (cw, ch, gap) = (CARD_W * k, CARD_H * k, CARD_GAP * k);
        let cards = &all;
        let row_w = cards.len() as f64 * (cw + gap) - gap;
        let w = f64::from(rect.width());
        // The viewport reaches past the cards so the plate's lift is not clipped: its shadow
        // sits 10 down with a 12 blur, so the foot needs the plate's reach, not the air.
        let air = 28.0 * k;
        let reach = 64.0 * k;
        // On the margin; past the screen's width the row glides to centre the focused card.
        // A swipe parks it until focus moves.
        let x0 = air + ui.edge(k);
        let strip = strip();
        let focus_x = cards
            .iter()
            .position(|t| Some(target_id(*t, pads)) == self.tree.focus())
            .map(|i| x0 + i as f64 * (cw + gap) + cw / 2.0);
        let max = (x0 * 2.0 + row_w - w).max(0.0);
        let want = focus_x.map_or(0.0, |x| (x - w / 2.0).clamp(0.0, max));
        self.tree.follow(
            strip,
            want as f32,
            std::mem::take(&mut self.snap) || ui.reduce_motion,
        );
        self.tree.tick(dt as f32);
        let top = 24.0 * k;
        let row = El::scroll(strip, Axis::Horizontal)
            .group(Group::Row)
            .id(strip)
            .child(El::column().place(Rect::from_xywh(0.0, 0.0, (x0 * 2.0 + row_w) as f32, 1.0)))
            .children(cards.iter().enumerate().map(|(i, t)| {
                let r = Rect::from_xywh(
                    (x0 + i as f64 * (cw + gap)) as f32,
                    air as f32,
                    cw as f32,
                    ch as f32,
                );
                let t = *t;
                let others = &self.others;
                El::paint(move |canvas, r| {
                    card(canvas, ui, fonts, t, pads, others, drawings, platform, r, k)
                })
                .id(target_id(t, pads))
                .focusable((CARD_CORNER * k) as f32)
                .place(r)
            }))
            .place(Rect::from_xywh(
                -air as f32,
                (top - air) as f32,
                (w + air) as f32,
                (ch + air + reach) as f32,
            ));
        let root = El::column().child(row);
        let frame = self.tree.layout(root, rect);
        let cheap = super::settings::rows::reduce_ui_res(
            ctx.settings,
            ctx.device.platform,
            ctx.device.fallback_ui,
        );
        self.tree
            .paint_focus(canvas, ui, frame, k as f32, dt, cheap);
    }

    /// Why the no-pads and grants cards exist; a pad's card says all it has.
    fn foot(&self, ctx: &Ctx) -> Option<std::borrow::Cow<'static, str>> {
        detail(self.focused(ctx))
    }

    fn title(&self) -> String {
        pf_l10n::client::pad_title().into()
    }
}

/// OK on `t`: a pad's menu where the host can test or pulse it; the grants list for the
/// last card. Anything else is a thud.
fn activate(t: Target, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
    match t {
        Target::Pad(i) => {
            let p = &ctx.pads[i];
            let platform = ctx.device.platform;
            let (test, rumble) = (can_test(platform), can_rumble(p, platform));
            if !test && !rumble {
                return Some(MenuPulse::Boundary);
            }
            fx.push(Screen::PadMenu(super::pad_menu::PadMenuScreen::new(
                p, test, rumble,
            )));
            Some(MenuPulse::Confirm)
        }
        Target::NoPads | Target::Others => Some(MenuPulse::Boundary),
        Target::Grants => {
            fx.push(Screen::Grants(super::grants::GrantsScreen::new()));
            Some(MenuPulse::Confirm)
        }
    }
}

/// A pad card: the pad drawn at rest, its name, what it streams as, and battery. The other
/// cards are an icon, a title and a line.
#[allow(clippy::too_many_arguments)]
fn card(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    t: Target,
    pads: &[PadInfo],
    others: &[OtherDevice],
    drawings: &[Drawing],
    platform: Platform,
    r: Rect,
    k: f64,
) {
    crate::theme::panel(
        canvas,
        ui,
        r,
        CARD_CORNER as f32,
        None,
        PanelStroke::Edge,
        k as f32,
    );
    let pad = 20.0 * k;
    let (l, t0) = (f64::from(r.left) + pad, f64::from(r.top) + pad);
    let max_w = f64::from(r.width()) - 2.0 * pad;
    let base = f64::from(r.bottom) - pad;
    let mark_cy = t0 + 16.0 * k;
    // An icon, a title and a line: a card that is not a pad.
    let plain = |icon: &str, title: &str, line: &str, ink| {
        if let Some(icon) = crate::icons::by_name(icon) {
            let box_px = (MARK * 0.62 * k) as f32;
            let x = (l + f64::from(box_px) / 2.0) as f32;
            crate::icons::draw_icon(canvas, icon, x, mark_cy as f32, box_px, k as f32, ink);
        }
        let size = 21.0 * k;
        fonts.draw_clipped(
            canvas,
            title,
            l,
            base - 22.0 * k,
            W::Bold,
            size,
            ui.fg(1.0),
            max_w,
        );
        fonts.draw_clipped(
            canvas,
            line,
            l,
            base,
            W::Regular,
            13.0 * k,
            ui.fg(0.55),
            max_w,
        );
    };
    match t {
        Target::Others => {
            let icon = match others.first().map(|d| d.kind.as_str()) {
                Some("mouse") => "mouse",
                Some("remote") => "tv",
                Some("keyboard") => "keyboard",
                _ => "pointer",
            };
            let (title, line) = others_lines(others);
            return plain(icon, &title, &line, ui.fg(0.5));
        }
        Target::Grants => {
            return plain(
                "circle-help",
                pf_l10n::console::console_pad_grants_card_title(),
                pf_l10n::console::console_pad_grants_card_line(),
                ui.accent(1.0),
            );
        }
        _ => {}
    }
    let Target::Pad(i) = t else {
        // No pad: the key device that drives instead.
        let mark = device_icon(None, platform);
        crate::glyphs::pad_mark(canvas, mark, l, mark_cy, MARK * k, k, ui.fg(0.5));
        fonts.draw_clipped(
            canvas,
            pf_l10n::console::console_pad_none_title(),
            l,
            base - 22.0 * k,
            W::Bold,
            21.0 * k,
            ui.fg(0.8),
            max_w,
        );
        fonts.draw_clipped(
            canvas,
            pf_l10n::console::console_pad_none_line(),
            l,
            base,
            W::Regular,
            13.0 * k,
            ui.fg(0.55),
            max_w,
        );
        return;
    };
    let p = &pads[i];
    let art = art_for(Some(p.pref));
    if let Some(d) = drawings.iter().find(|d| std::ptr::eq(d.art, art)) {
        // The pad fills the room above its text block, centred, at rest.
        let room_h = base - 44.0 * k - t0;
        let scale = (ART_W * k / f64::from(art.w)).min(room_h / f64::from(art.h));
        let (w, h) = (f64::from(art.w) * scale, f64::from(art.h) * scale);
        let x = f64::from(r.center_x()) - w / 2.0;
        let y = t0 + (room_h - h) / 2.0;
        draw_pad(
            canvas,
            ui,
            fonts,
            d,
            &PadTestState::default(),
            x,
            y,
            scale,
            k,
        );
    }
    if let Some(b) = p.battery {
        crate::glyphs::battery_pip(
            canvas,
            ui,
            f64::from(r.right) - pad - 22.0 * k,
            t0 + 16.0 * k,
            22.0 * k,
            k,
            b,
        );
    }
    let status = match p.forwarded {
        false => pf_l10n::console::console_pad_not_forwarded().to_string(),
        true => pf_l10n::console::console_pad_streams_as(streams_as(p)),
    };
    fonts.draw_clipped(
        canvas,
        &p.name,
        l,
        base - 22.0 * k,
        W::Bold,
        17.0 * k,
        ui.fg(1.0),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        &status,
        l,
        base,
        W::Regular,
        13.0 * k,
        ui.fg(0.7),
        max_w,
    );
}

/// A device's kind as a word, where the host could tell.
fn kind_word(kind: &str) -> Option<&'static str> {
    use pf_l10n::console as c;
    match kind {
        "keyboard" => Some(c::console_pad_kind_keyboard()),
        "mouse" => Some(c::console_pad_kind_mouse()),
        "remote" => Some(c::console_pad_kind_remote()),
        _ => None,
    }
}

/// The folded card's title and line: one device by its name, more by their count; the line
/// names the kinds the host could tell, once each, and stays empty when it could not.
fn others_lines(others: &[OtherDevice]) -> (String, String) {
    let mut kinds: Vec<&str> = Vec::new();
    for d in others.iter().filter(|d| kind_word(&d.kind).is_some()) {
        if !kinds.contains(&d.kind.as_str()) {
            kinds.push(&d.kind);
        }
    }
    let words: Vec<&str> = kinds.into_iter().filter_map(kind_word).collect();
    let line = words.join(" \u{b7} ");
    match others {
        [one] => (one.name.clone(), line),
        _ => (
            pf_l10n::console::console_pad_other_devices(others.len() as u64),
            line,
        ),
    }
}

/// What a pad streams as: its kind, or the Xbox 360 pad a plain one becomes.
fn streams_as(pad: &PadInfo) -> &'static str {
    match pad.kind_label() {
        "" => "Xbox 360",
        kind => kind,
    }
}

fn detail(t: Target) -> Option<std::borrow::Cow<'static, str>> {
    use pf_l10n::console as c;
    match t {
        Target::Pad(_) | Target::Others => None,
        Target::NoPads => Some(c::console_pad_none_detail().into()),
        // The card is Android's, where OK confirms.
        Target::Grants => Some(
            format!(
                "{} {}",
                c::console_pad_grants_detail(),
                c::console_pad_grants_detail_open("OK")
            )
            .into(),
        ),
    }
}

fn pad_detail(pad: &PadInfo) -> String {
    use pf_l10n::console as c;
    let mut parts: Vec<String> = Vec::new();
    if !pad.detail.is_empty() {
        parts.push(pad.detail.clone());
    }
    if !pad.forwarded {
        parts.push(c::console_pad_not_forwarded_reason_a11y().into());
    }
    parts.push(c::console_pad_streams_as(streams_as(pad)));
    if let Some(b) = pad.battery {
        parts.push(if b.charging {
            c::console_pad_battery_charging_a11y(b.percent)
        } else {
            c::console_pad_battery_a11y(b.percent)
        });
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;
    use punktfunk_core::config::GamepadPref;

    fn pad(name: &str, rumble: bool) -> PadInfo {
        PadInfo {
            name: name.into(),
            key: format!("054c:0ce6:{name}"),
            pref: GamepadPref::DualSense,
            steam_virtual: false,
            battery: None,
            detail: "054C:0CE6 · gamepad".into(),
            forwarded: true,
            rumble,
        }
    }

    /// One frame `dt` long at 1280 × 800, then `evs`; the outboxes, in order.
    fn step(
        screen: &mut PlayersScreen,
        platform: Platform,
        pads: &[PadInfo],
        fonts: &Fonts,
        dt: f64,
        evs: &[MenuEvent],
    ) -> Vec<(Outbox, Option<MenuPulse>)> {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            platform,
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            pads,
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_xywh(0.0, 64.0, 1280.0, 650.0);
        screen.render(surface.canvas(), rect, 1.0, dt, fonts, &mut ctx);
        evs.iter()
            .map(|ev| {
                let mut fx = Outbox::default();
                let pulse = screen.menu(*ev, &mut ctx, &mut fx);
                (fx, pulse)
            })
            .collect()
    }

    /// Render once so the tree has rects, then send `evs`; the outboxes, in order.
    fn drive(
        screen: &mut PlayersScreen,
        platform: Platform,
        pads: &[PadInfo],
        evs: &[MenuEvent],
    ) -> Vec<(Outbox, Option<MenuPulse>)> {
        let fonts = crate::theme::build_fonts().unwrap();
        step(screen, platform, pads, &fonts, 1.0 / 60.0, evs)
    }

    /// Focus moves at once; the row glides after it rather than jumping, and lands.
    #[test]
    fn the_row_glides_to_keep_the_focused_card_in_view() {
        let pads: Vec<PadInfo> = (0..6).map(|i| pad(&format!("Pad {i}"), true)).collect();
        let fonts = crate::theme::build_fonts().unwrap();
        let mut s = PlayersScreen::new();
        let right = [MenuEvent::Move(MenuDir::Right); 5];
        step(&mut s, Platform::Apple, &pads, &fonts, 1.0 / 60.0, &right);
        assert_eq!(s.tree.offset(strip()), 0.0, "the first frame seats the row");
        step(&mut s, Platform::Apple, &pads, &fonts, 1.0 / 60.0, &[]);
        let early = s.tree.offset(strip());
        assert!(early > 0.0, "one frame in it has moved: {early}");
        assert!(s.tree.following(strip()));
        for _ in 0..180 {
            step(&mut s, Platform::Apple, &pads, &fonts, 1.0 / 60.0, &[]);
        }
        let landed = s.tree.offset(strip());
        assert!(landed > early, "and keeps going: {early} → {landed}");
        assert!(!s.tree.following(strip()), "then rests");
    }

    fn other(name: &str, kind: &str) -> OtherDevice {
        OtherDevice {
            name: name.into(),
            kind: kind.into(),
            detail: String::new(),
        }
    }

    fn opens(fx: &Outbox, menu: bool) -> bool {
        matches!(fx.nav, Some(crate::screens::Nav::Push(ref b))
            if matches!(**b, Screen::PadMenu(_)) == menu)
    }

    /// OK, or a hold, on a pad the host can test or pulse opens its menu; Right reaches the
    /// next pad. A desktop has neither, so OK thuds there.
    #[test]
    fn ok_or_a_hold_on_a_pad_opens_its_menu() {
        let pads = [pad("DualSense", true), pad("Edge", true)];
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Apple,
            &pads,
            &[
                MenuEvent::Confirm,
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Secondary,
            ],
        );
        assert!(opens(&out[0].0, true));
        assert!(opens(&out[2].0, true), "right reaches the next card");
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Android,
            &[pad("Adapter", false)],
            &[MenuEvent::Confirm],
        );
        assert!(
            opens(&out[0].0, true),
            "Android tests a pad it cannot pulse"
        );
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Desktop,
            &[pad("DualSense", true)],
            &[MenuEvent::Confirm],
        );
        assert!(out[0].0.nav.is_none() && out[0].0.cmds.is_empty());
        assert!(matches!(out[0].1, Some(MenuPulse::Boundary)));
    }

    /// Right past the last pad lands on the one card for every other device, which OK
    /// leaves alone; on Android the "Not showing?" card follows and opens the grants list.
    /// Off Android, the others' card ends the row.
    #[test]
    fn right_from_the_cards_reaches_the_others_then_the_grants_card() {
        let pads = [pad("DualSense", true)];
        let mut s = PlayersScreen::new();
        s.others = vec![other("Keyboard", "keyboard"), other("Mouse", "mouse")];
        let out = drive(
            &mut s,
            Platform::Android,
            &pads,
            &[
                MenuEvent::Move(MenuDir::Up),
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Confirm,
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Confirm,
            ],
        );
        assert!(matches!(out[0].1, Some(MenuPulse::Boundary)));
        assert!(matches!(out[2].1, Some(MenuPulse::Boundary)), "others thud");
        assert!(out[4].0.cmds.is_empty());
        assert!(
            matches!(out[4].0.nav, Some(crate::screens::Nav::Push(ref b))
            if matches!(**b, Screen::Grants(_)))
        );
        let mut s = PlayersScreen::new();
        s.others = vec![other("Keyboard", "keyboard"), other("Mouse", "mouse")];
        let out = drive(
            &mut s,
            Platform::Apple,
            &pads,
            &[
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Move(MenuDir::Right),
            ],
        );
        assert!(matches!(out[0].1, Some(MenuPulse::Move)), "onto the others");
        assert!(
            matches!(out[1].1, Some(MenuPulse::Boundary)),
            "no grants off Android"
        );
    }

    /// Several other devices fold into one card named by count; one keeps its name.
    #[test]
    fn other_devices_fold_into_one_card() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let ctx = Ctx::test(&mut settings, &library);
        let mut s = PlayersScreen::new();
        s.others = vec![other("Keyboard", "keyboard"), other("Mouse", "mouse")];
        assert_eq!(
            targets(&ctx, &s.others),
            vec![Target::NoPads, Target::Others]
        );
        assert_eq!(
            s.announcement(&ctx).as_deref(),
            Some("No controller connected")
        );
        assert!(
            s.foot(&ctx).is_some(),
            "the no-pads card explains itself on the foot"
        );
        assert_eq!(
            others_lines(&s.others),
            (
                "2 other devices".to_string(),
                "Keyboard \u{b7} Mouse".to_string()
            )
        );
        s.others.pop();
        assert_eq!(
            others_lines(&s.others),
            ("Keyboard".to_string(), "Keyboard".to_string())
        );
        s.others[0].kind = "other".into();
        assert_eq!(
            others_lines(&s.others).1,
            "",
            "an unknown kind says nothing"
        );
    }

    #[test]
    fn with_no_pads_an_inert_card_stands_in() {
        let mut s = PlayersScreen::new();
        let out = drive(&mut s, Platform::Android, &[], &[MenuEvent::Confirm]);
        assert!(out[0].0.cmds.is_empty(), "the empty card does nothing");
        assert!(matches!(out[0].1, Some(MenuPulse::Boundary)));
    }
}
