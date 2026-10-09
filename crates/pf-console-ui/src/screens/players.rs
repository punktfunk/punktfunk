//! Controllers: the connected pads as cards, a Test card for the live input test where the
//! host can run it ([`super::input_test`]), the keyboards and mice the host sees, and on
//! Android a last "Not showing?" card that opens the grants and tests only the host can
//! perform ([`super::grants`]). The fourth tab.
//!
//! Cards are one focus row. OK on a pad is its rumble test. Only devices the OS classifies
//! as a gamepad are forwarded; adapters often enumerate as something else, so each card's
//! identity line is the support answer.
//!
//! A rumble pulse on a real device stays with the host via [`ConsoleCmd::PadAction`]; the
//! desktop has no device handle, so it lists pads and nothing more.
//! Seat order joins when `seats.rs` lands (`console-ui-redesign.md` §2).

use crate::el::{Axis, El, Group, Id, Tree};
use crate::glyphs::{device_icon, Hint};
use crate::model::{ConsoleCmd, OtherDevice};
use crate::platform::Platform;
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{accent, edge, fg, Fonts, PanelStroke, W};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

/// Card size and gap, design units.
const CARD_W: f64 = 300.0;
const CARD_H: f64 = 164.0;
const CARD_GAP: f64 = 24.0;
const CARD_CORNER: f64 = 22.0;
/// Device mark box, dp: the size the tells inside each pad outline are drawn for.
const MARK: f64 = 44.0;

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

/// Grant/test rows, Android's alone: the desktop has no USB capture and no grants.
pub(crate) const PASSTHROUGH: [(PadAction, &str, &str); 4] = [
    (
        PadAction::Sc2Bluetooth,
        "Steam Controller 2 over Bluetooth",
        "Grant",
    ),
    (PadAction::Sc2Usb, "Steam Controller 2 over USB", "Grant"),
    (PadAction::DsUsb, "DualSense / DualShock over USB", "Grant"),
    (PadAction::DsHaptics, "DualSense haptics self-test", "Test"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Pad(usize),
    /// The inert card that stands in for no pads, so the row is never empty.
    NoPads,
    /// The live input test, after the pads.
    Test,
    /// A keyboard, mouse or other device the host reports. Never a pad to the host.
    Other(usize),
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
    if !ctx.pads.is_empty() && can_test(ctx.device.platform) {
        all.push(Target::Test);
    }
    all.extend((0..others.len()).map(Target::Other));
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
        Target::Test => Id::new("pad-test", 0),
        Target::Other(i) => Id::new("other-device", i),
        Target::Grants => Id::new("grants", 0),
    }
}

/// Only a host with a device handle can pulse a motor.
fn can_rumble(pad: &PadInfo, platform: Platform) -> bool {
    pad.rumble && matches!(platform, Platform::Android | Platform::Apple)
}

pub(crate) struct PlayersScreen {
    tree: Tree,
    /// Set by the shell each frame from what the host reports.
    pub(crate) others: Vec<OtherDevice>,
}

impl PlayersScreen {
    pub(crate) fn new() -> PlayersScreen {
        PlayersScreen {
            tree: Tree::new(),
            others: Vec::new(),
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

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
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
            MenuEvent::Confirm => activate(at, ctx, fx),
            _ => None,
        }
    }

    /// Hover focuses; a press acts on what it lands on.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
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

    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        Some(match self.focused(ctx) {
            Target::Pad(i) => format!("{}, {}", ctx.pads[i].name, pad_detail(&ctx.pads[i])),
            Target::NoPads => "No controller connected".into(),
            Target::Test => "Test controller".into(),
            Target::Other(i) => self.others.get(i)?.name.clone(),
            Target::Grants => "Controller not showing? Opens the access list".into(),
        })
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        Vec::new()
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let all = targets(ctx, &self.others);
        // The first target takes focus once; a card that goes reseats it in the tree.
        if self.tree.focus().is_none() {
            self.tree.set_focus(Some(target_id(all[0], ctx.pads)));
        }
        let (platform, pads) = (ctx.device.platform, ctx.pads);
        let (cw, ch, gap) = (CARD_W * k, CARD_H * k, CARD_GAP * k);
        let cards = &all;
        let row_w = cards.len() as f64 * (cw + gap) - gap;
        let w = f64::from(rect.width());
        // The viewport reaches past the cards so the plate's lift is not clipped: its shadow
        // sits 10 down with a 12 blur, so the foot needs the plate's reach, not the air.
        let air = 28.0 * k;
        let reach = 64.0 * k;
        // On the margin; past the screen's width the row scrolls to keep focus in view.
        let x0 = air + edge(k);
        let strip = Id::new("cards", 0);
        let focus_x = cards
            .iter()
            .position(|t| Some(target_id(*t, pads)) == self.tree.focus())
            .map(|i| x0 + i as f64 * (cw + gap) + cw / 2.0);
        let max = (x0 * 2.0 + row_w - w).max(0.0);
        let offset = focus_x.map_or(0.0, |x| (x - w / 2.0).clamp(0.0, max));
        self.tree.set_offset(strip, offset as f32);
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
                El::paint(move |canvas, r| card(canvas, fonts, t, pads, others, platform, r, k))
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
        self.tree.paint_focus(canvas, frame, k as f32, dt, cheap);
    }

    /// What the focus is. Grant rows run under it on a short screen.
    pub(crate) fn foot(&self, ctx: &Ctx) -> String {
        detail(self.focused(ctx), ctx, &self.others)
    }
}

/// OK on `t`: a rumble pulse for a pad with a motor the host can reach; the grants list
/// for the last card. Anything else is a thud.
fn activate(t: Target, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
    match t {
        Target::Pad(i) if can_rumble(&ctx.pads[i], ctx.device.platform) => {
            fx.cmds.push(ConsoleCmd::PadAction {
                action: PadAction::Rumble.id().to_string(),
                pad_key: ctx.pads[i].key.clone(),
            });
            Some(MenuPulse::Confirm)
        }
        Target::Pad(_) | Target::NoPads | Target::Other(_) => Some(MenuPulse::Boundary),
        Target::Test => {
            fx.push(Screen::InputTest(super::input_test::InputTestScreen::new()));
            Some(MenuPulse::Confirm)
        }
        Target::Grants => {
            fx.push(Screen::Grants(super::grants::GrantsScreen::new()));
            Some(MenuPulse::Confirm)
        }
    }
}

/// A pad card: its family mark, name, what it streams as, battery, and the test OK runs.
#[allow(clippy::too_many_arguments)]
fn card(
    canvas: &Canvas,
    fonts: &Fonts,
    t: Target,
    pads: &[PadInfo],
    others: &[OtherDevice],
    platform: Platform,
    r: Rect,
    k: f64,
) {
    crate::theme::panel(
        canvas,
        r,
        CARD_CORNER as f32,
        None,
        PanelStroke::Gradient,
        k as f32,
    );
    crate::theme::panel_highlight(canvas, r, CARD_CORNER as f32, k as f32);
    let pad = 20.0 * k;
    let (l, t0) = (f64::from(r.left) + pad, f64::from(r.top) + pad);
    let max_w = f64::from(r.width()) - 2.0 * pad;
    let base = f64::from(r.bottom) - pad;
    let mark_cy = t0 + 16.0 * k;
    // An icon, a title and a line: the Test card, a device that is not a pad.
    let plain = |icon: &str, title: &str, line: &str, ink| {
        if let Some(icon) = crate::icons::by_name(icon) {
            let box_px = (MARK * 0.62 * k) as f32;
            let x = (l + f64::from(box_px) / 2.0) as f32;
            crate::icons::draw_icon(canvas, icon, x, mark_cy as f32, box_px, ink);
        }
        let size = 21.0 * k;
        fonts.draw_clipped(
            canvas,
            title,
            l,
            base - 22.0 * k,
            W::Bold,
            size,
            fg(1.0),
            max_w,
        );
        fonts.draw_clipped(canvas, line, l, base, W::Regular, 13.0 * k, fg(0.55), max_w);
    };
    match t {
        Target::Test => {
            return plain(
                "gamepad-2",
                "Test controller",
                "See every button and stick",
                accent(1.0),
            );
        }
        Target::Other(i) => {
            let Some(d) = others.get(i) else { return };
            let icon = match d.kind.as_str() {
                "keyboard" => "keyboard",
                "mouse" => "mouse",
                "remote" => "tv",
                _ => "pointer",
            };
            return plain(icon, &d.name, "Not a controller", fg(0.5));
        }
        _ => {}
    }
    if t == Target::Grants {
        if let Some(icon) = crate::icons::by_name("circle-help") {
            let box_px = (MARK * 0.62 * k) as f32;
            crate::icons::draw_icon(
                canvas,
                icon,
                (l + f64::from(box_px) / 2.0) as f32,
                mark_cy as f32,
                box_px,
                accent(1.0),
            );
        }
        fonts.draw_clipped(
            canvas,
            "Not showing?",
            l,
            base - 22.0 * k,
            W::Bold,
            21.0 * k,
            fg(1.0),
            max_w,
        );
        fonts.draw_clipped(
            canvas,
            "Some controllers need access first",
            l,
            base,
            W::Regular,
            13.0 * k,
            fg(0.55),
            max_w,
        );
        return;
    }
    let Target::Pad(i) = t else {
        // No pad: the key device that drives instead.
        let mark = device_icon(None, platform);
        crate::glyphs::pad_mark(canvas, mark, l, mark_cy, MARK * k, k, fg(0.5));
        fonts.draw_clipped(
            canvas,
            "No controller",
            l,
            base - 22.0 * k,
            W::Bold,
            21.0 * k,
            fg(0.8),
            max_w,
        );
        fonts.draw_clipped(
            canvas,
            "Connect one to see it here",
            l,
            base,
            W::Regular,
            13.0 * k,
            fg(0.55),
            max_w,
        );
        return;
    };
    let p = &pads[i];
    let mark = device_icon(Some(p.pref), platform);
    crate::glyphs::pad_mark(canvas, mark, l, mark_cy, MARK * k, k, accent(1.0));
    if let Some(b) = p.battery {
        crate::glyphs::battery_pip(
            canvas,
            f64::from(r.right) - pad - 22.0 * k,
            t0 + 16.0 * k,
            22.0 * k,
            k,
            b,
        );
    }
    let kind = p.kind_label();
    let status = match (p.forwarded, kind.is_empty()) {
        (false, _) => "Not forwarded".to_string(),
        (true, true) => "Streams as Xbox 360".to_string(),
        (true, false) => format!("Streams as {kind}"),
    };
    let test = if can_rumble(p, platform) {
        "OK tests rumble"
    } else {
        ""
    };
    fonts.draw_clipped(
        canvas,
        &p.name,
        l,
        base - 44.0 * k,
        W::Bold,
        17.0 * k,
        fg(1.0),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        &status,
        l,
        base - 22.0 * k,
        W::Regular,
        13.0 * k,
        fg(0.7),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        test,
        l,
        base,
        W::SemiBold,
        12.0 * k,
        accent(1.0),
        max_w,
    );
}

fn detail(t: Target, ctx: &Ctx, others: &[OtherDevice]) -> String {
    match t {
        Target::Test => "Shows every button, trigger and stick as the controller sends it, to \
                         tell a worn button from a setting. Hold B to finish."
            .into(),
        Target::Other(i) => (others.get(i))
            .map(|d| {
                let what = "Streams take it as keyboard or mouse input, not as a pad.";
                if d.detail.is_empty() {
                    what.into()
                } else {
                    format!("{} \u{2014} {what}", d.detail)
                }
            })
            .unwrap_or_default(),
        Target::NoPads => "Punktfunk only forwards devices the system classifies as a gamepad or \
                           joystick — a pad behind an adapter or hub may enumerate with the \
                           adapter's identity, or not at all."
            .into(),
        Target::Pad(i) => pad_detail(&ctx.pads[i]),
        Target::Grants => "A wired DualSense or Steam Controller 2 needs USB access before it can \
                           be captured, and a Steam Controller 2 over Bluetooth needs Bluetooth \
                           access to be seen at all. OK lists the grants and a haptics test."
            .into(),
    }
}

fn pad_detail(pad: &PadInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !pad.detail.is_empty() {
        parts.push(pad.detail.clone());
    }
    if !pad.forwarded {
        parts.push("not forwarded — not classified as a gamepad".into());
    }
    let kind = pad.kind_label();
    parts.push(format!(
        "streams as {}",
        if kind.is_empty() { "Xbox 360" } else { kind }
    ));
    if let Some(b) = pad.battery {
        parts.push(if b.charging {
            format!("battery {} %, charging", b.percent)
        } else {
            format!("battery {} %", b.percent)
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

    /// Render once so the tree has rects, then send `evs`; the outboxes, in order.
    fn drive(
        screen: &mut PlayersScreen,
        platform: Platform,
        pads: &[PadInfo],
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
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_xywh(0.0, 64.0, 1280.0, 650.0);
        screen.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, &mut ctx);
        evs.iter()
            .map(|ev| {
                let mut fx = Outbox::default();
                let pulse = screen.menu(*ev, &mut ctx, &mut fx);
                (fx, pulse)
            })
            .collect()
    }

    fn rumble(key: &str) -> ConsoleCmd {
        ConsoleCmd::PadAction {
            action: "rumble".into(),
            pad_key: key.into(),
        }
    }

    #[test]
    fn ok_on_a_card_asks_the_host_for_a_rumble_pulse() {
        let pads = [pad("DualSense", true), pad("Edge", true)];
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Apple,
            &pads,
            &[
                MenuEvent::Confirm,
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Confirm,
            ],
        );
        assert_eq!(out[0].0.cmds, vec![rumble("054c:0ce6:DualSense")]);
        assert_eq!(
            out[2].0.cmds,
            vec![rumble("054c:0ce6:Edge")],
            "right reaches the next card"
        );
    }

    #[test]
    fn a_pad_the_host_cannot_pulse_thuds() {
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Android,
            &[pad("Adapter", false)],
            &[MenuEvent::Confirm],
        );
        assert!(out[0].0.cmds.is_empty());
        assert!(matches!(out[0].1, Some(MenuPulse::Boundary)));
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Desktop,
            &[pad("DualSense", true)],
            &[MenuEvent::Confirm],
        );
        assert!(
            out[0].0.cmds.is_empty(),
            "the desktop has no device handle to pulse"
        );
    }

    /// Right past the last pad lands on Test, whose OK opens the input test; on Android the
    /// "Not showing?" card follows and opens the grants list. Off Android, Test ends the row.
    #[test]
    fn right_from_the_cards_reaches_test_then_the_grants_card() {
        let pads = [pad("DualSense", true)];
        let mut s = PlayersScreen::new();
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
        assert!(
            matches!(out[2].0.nav, Some(crate::screens::Nav::Push(ref b))
            if matches!(**b, Screen::InputTest(_)))
        );
        assert!(out[4].0.cmds.is_empty());
        assert!(
            matches!(out[4].0.nav, Some(crate::screens::Nav::Push(ref b))
            if matches!(**b, Screen::Grants(_)))
        );
        let mut s = PlayersScreen::new();
        let out = drive(
            &mut s,
            Platform::Apple,
            &pads,
            &[
                MenuEvent::Move(MenuDir::Right),
                MenuEvent::Move(MenuDir::Right),
            ],
        );
        assert!(matches!(out[0].1, Some(MenuPulse::Move)), "onto Test");
        assert!(
            matches!(out[1].1, Some(MenuPulse::Boundary)),
            "no grants off Android"
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
