//! A live controller test: the driving pad's family drawn large, each button lit while held,
//! the sticks tilting and the triggers filling, and the last thing that moved named. Raised
//! by the Controllers tab's Test card.
//!
//! While it is on top the shell asks the host for [`PadTestState`]s and the host stops
//! turning the pad into menu moves ([`crate::model::ConsoleCmd::PadTest`]), so B shows as a
//! button instead of backing out. Holding B for [`HOLD_TO_LEAVE`] leaves; a remote or keyboard
//! Back leaves at once.

use crate::glyphs::{self, GlyphStyle, Hint, HintKey};
use crate::icons::{self, Icon};
use crate::model::PadTestState;
use crate::screens::{Ctx, Outbox};
use crate::theme::{accent, fg, fill, on_accent, stroke, Fonts, W};
use crate::widgets::blurb;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use punktfunk_core::config::GamepadPref;
use skia_safe::{Canvas, Point, RRect, Rect};

pub(crate) const HOLD_TO_LEAVE: f64 = 1.0;
/// An axis has to move this far to count as the last input.
const AXIS_NOTICE: f32 = 0.25;

pub(crate) struct InputTestScreen {
    state: PadTestState,
    last: Option<String>,
    /// When B went down, while it stays down.
    b_since: Option<f64>,
    /// The driving pad's family, set by the shell each frame.
    pub(crate) pref: Option<GamepadPref>,
    pub(crate) done: bool,
}

impl InputTestScreen {
    pub(crate) fn new() -> InputTestScreen {
        InputTestScreen {
            state: PadTestState::default(),
            last: None,
            b_since: None,
            pref: None,
            done: false,
        }
    }

    /// The host's latest reading, at shell time `t`.
    pub(crate) fn set_state(&mut self, state: PadTestState, t: f64) {
        if let Some(b) = state.held.iter().find(|b| !self.state.held.contains(b)) {
            self.last = Some(format!("{b} pressed"));
        }
        for (name, v) in &state.axes {
            if (v - self.state.axis(name)).abs() >= AXIS_NOTICE {
                self.last = Some(format!("{name} {v:+.2}"));
            }
        }
        let b_down = state.held.iter().any(|b| b == "B");
        self.b_since = if b_down {
            Some(self.b_since.unwrap_or(t))
        } else {
            None
        };
        if self.b_since.is_some_and(|since| t - since >= HOLD_TO_LEAVE) {
            self.done = true;
        }
        self.state = state;
    }

    /// Only a remote or keyboard reaches here while the test is on: its Back leaves.
    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        _ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
        }
        None
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![Hint::new(HintKey::Back, "Hold to finish")]
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        _dt: f64,
        fonts: &Fonts,
        _ctx: &mut Ctx,
    ) {
        let below = blurb(
            canvas,
            fonts,
            "Every button and stick on the controller shows here. Hold B to finish.",
            rect,
            k,
        );
        let line = 40.0 * k;
        let avail_h = f64::from(rect.bottom - below.top) - line;
        // The drawing spans about 20 of its 24 units in height.
        let box_px = (f64::from(rect.width()) * 0.8)
            .min(avail_h * 24.0 / 20.0)
            .min(640.0 * k);
        let x = f64::from(rect.center_x()) - box_px / 2.0;
        let top = f64::from(below.top) - box_px / 24.0;
        draw_pad(canvas, fonts, self.pref, &self.state, x, top, box_px, k);
        let last = match &self.last {
            Some(input) => format!("Last input \u{2014} {input}"),
            None => "Press a button or move a stick".into(),
        };
        let size = 15.0 * k;
        let w = f64::from(fonts.measure(&last, W::Regular, size));
        let y = top + box_px * 20.5 / 24.0 + line * 0.6;
        let cx = f64::from(rect.center_x());
        fonts.draw(canvas, &last, cx - w / 2.0, y, W::Regular, size, fg(0.6));
    }
}

impl PadTestState {
    fn axis(&self, name: &str) -> f32 {
        (self.axes.iter())
            .find(|(n, _)| n == name)
            .map_or(0.0, |(_, v)| *v)
    }

    fn held(&self, name: &str) -> bool {
        self.held.iter().any(|b| b == name)
    }
}

/// Where a family's controls sit on its outline, in the outline's 24-unit box.
#[derive(Clone, Copy)]
struct Layout {
    icon: Icon,
    ls: (f32, f32),
    rs: (f32, f32),
    /// The stick wells' radius.
    well: f32,
    dpad: (f32, f32),
    face: (f32, f32),
    back: (f32, f32),
    start: (f32, f32),
    guide: (f32, f32),
    /// The left shoulder's centre x and the body's top under it; the right mirrors it.
    shoulder: (f32, f32),
}

fn layout(pref: Option<GamepadPref>) -> Layout {
    use GamepadPref as P;
    let xbox_one = Layout {
        icon: icons::PAD_XBOX_ONE,
        ls: (6.8, 9.1),
        rs: (14.2, 12.9),
        well: 1.5,
        dpad: (9.8, 12.9),
        face: (17.2, 9.1),
        back: (10.3, 8.4),
        start: (13.7, 8.4),
        guide: (12.0, 6.5),
        shoulder: (6.2, 4.7),
    };
    let dualsense = Layout {
        icon: icons::PAD_DUALSENSE,
        ls: (8.6, 12.4),
        rs: (15.4, 12.4),
        dpad: (5.0, 9.2),
        face: (19.0, 9.2),
        back: (6.8, 6.4),
        start: (17.2, 6.4),
        guide: (12.0, 12.4),
        shoulder: (5.4, 4.9),
        ..xbox_one
    };
    match pref {
        Some(P::XboxOne | P::SteamController2Puck) | None => xbox_one,
        Some(P::Auto | P::Xbox360) => Layout {
            icon: icons::PAD_XBOX_360,
            ls: (6.4, 9.4),
            rs: (15.4, 12.8),
            dpad: (8.6, 12.8),
            face: (17.6, 9.4),
            back: (9.6, 8.0),
            start: (14.4, 8.0),
            guide: (12.0, 8.0),
            shoulder: (5.8, 4.1),
            ..xbox_one
        },
        Some(P::XboxElite) => Layout {
            icon: icons::PAD_XBOX_ELITE,
            ..xbox_one
        },
        Some(P::DualShock4) => Layout {
            icon: icons::PAD_DUALSHOCK_4,
            dpad: (5.2, 9.3),
            face: (18.8, 9.3),
            back: (8.4, 6.9),
            start: (15.6, 6.9),
            shoulder: (5.4, 5.2),
            ..dualsense
        },
        Some(P::DualSense) => dualsense,
        Some(P::DualSenseEdge) => Layout {
            icon: icons::PAD_DUALSENSE_EDGE,
            ..dualsense
        },
        Some(P::SwitchPro) => Layout {
            icon: icons::PAD_SWITCH_PRO,
            ls: (6.3, 8.6),
            rs: (14.7, 12.3),
            dpad: (9.3, 12.3),
            face: (17.7, 8.6),
            back: (10.3, 7.2),
            start: (13.7, 7.2),
            guide: (13.4, 9.5),
            shoulder: (5.8, 4.5),
            ..xbox_one
        },
        // The left pad is the d-pad, the right pad the right stick.
        Some(P::SteamController) => Layout {
            icon: icons::PAD_STEAM_CONTROLLER,
            ls: (8.9, 12.4),
            rs: (17.7, 8.6),
            dpad: (6.3, 8.6),
            face: (15.2, 12.3),
            back: (10.3, 8.9),
            start: (13.7, 8.9),
            guide: (12.0, 7.0),
            shoulder: (5.6, 4.4),
            ..xbox_one
        },
        Some(P::SteamController2) => Layout {
            icon: icons::PAD_STEAM_CONTROLLER_2,
            ls: (9.0, 8.0),
            rs: (15.0, 8.0),
            well: 1.2,
            dpad: (4.9, 9.6),
            face: (19.1, 9.6),
            back: (10.9, 5.9),
            start: (13.1, 5.9),
            guide: (12.0, 10.4),
            shoulder: (5.4, 4.4),
        },
        Some(P::SteamDeck) => Layout {
            icon: icons::PAD_STEAM_DECK,
            ls: (4.1, 9.6),
            rs: (19.9, 9.6),
            well: 1.1,
            dpad: (4.0, 13.4),
            face: (20.0, 13.4),
            back: (6.1, 7.6),
            start: (17.9, 7.6),
            guide: (6.0, 16.3),
            shoulder: (4.4, 6.6),
        },
    }
}

/// The controller `box_px` wide with its outline box's top-left at `(x, y)`, lit from `state`.
/// Sticks read −1…1 with +y down; triggers 0…1.
#[allow(clippy::too_many_arguments)]
fn draw_pad(
    canvas: &Canvas,
    fonts: &Fonts,
    pref: Option<GamepadPref>,
    state: &PadTestState,
    x: f64,
    y: f64,
    box_px: f64,
    k: f64,
) {
    let l = layout(pref);
    let f = (box_px / 24.0) as f32;
    let (ox, oy) = (x as f32, y as f32);
    let at = |(u, v): (f32, f32)| Point::new(ox + u * f, oy + v * f);
    let rr = |c: Point, w: f32, h: f32, r: f32| {
        let rect = Rect::from_xywh(c.x - w * f / 2.0, c.y - h * f / 2.0, w * f, h * f);
        RRect::new_rect_xy(rect, r * f, r * f)
    };
    let lit = |on: bool| fill(if on { accent(1.0) } else { fg(0.1) });

    // Shoulders: a bumper on the body's top edge, the trigger above it filling as pulled.
    for (right, bumper, trigger) in [(false, "LB", "LT"), (true, "RB", "RT")] {
        let cx = if right {
            24.0 - l.shoulder.0
        } else {
            l.shoulder.0
        };
        let top = l.shoulder.1;
        canvas.draw_rrect(
            rr(at((cx, top - 0.75)), 3.2, 0.8, 0.4),
            &lit(state.held(bumper)),
        );
        let track = rr(at((cx, top - 2.2)), 2.5, 1.4, 0.45);
        canvas.draw_rrect(track, &fill(fg(0.1)));
        let pull = if state.held(trigger) {
            1.0
        } else {
            state.axis(trigger).clamp(0.0, 1.0)
        };
        if pull > 0.0 {
            canvas.save();
            let r = track.rect();
            canvas.clip_rect(
                Rect::from_ltrb(r.left, r.bottom - r.height() * pull, r.right, r.bottom),
                None,
                true,
            );
            canvas.draw_rrect(track, &fill(accent(1.0)));
            canvas.restore();
        }
    }
    canvas.draw_rrect(rr(at(l.back), 1.0, 0.55, 0.275), &lit(state.held("Back")));
    canvas.draw_rrect(rr(at(l.start), 1.0, 0.55, 0.275), &lit(state.held("Start")));
    canvas.draw_circle(at(l.guide), 0.7 * f, &lit(state.held("Guide")));
    // The d-pad: four arms round an unlit centre.
    canvas.draw_rrect(rr(at(l.dpad), 0.72, 0.72, 0.1), &fill(fg(0.1)));
    for (name, dx, dy) in [
        ("Up", 0.0, -1.0),
        ("Down", 0.0, 1.0),
        ("Left", -1.0, 0.0),
        ("Right", 1.0, 0.0),
    ] {
        let c = at((l.dpad.0 + dx * 0.8, l.dpad.1 + dy * 0.8));
        let (w, h) = if dx == 0.0 { (0.72, 0.9) } else { (0.9, 0.72) };
        canvas.draw_rrect(rr(c, w, h, 0.18), &lit(state.held(name)));
    }
    let style = GlyphStyle::from_pref(pref);
    for (name, dx, dy) in [
        ("A", 0.0, 1.0),
        ("B", 1.0, 0.0),
        ("X", -1.0, 0.0),
        ("Y", 0.0, -1.0),
    ] {
        let c = at((l.face.0 + dx * 1.3, l.face.1 + dy * 1.3));
        let on = state.held(name);
        canvas.draw_circle(c, 0.62 * f, &lit(on));
        let ink = if on { on_accent() } else { fg(0.85) };
        glyphs::draw_face(canvas, fonts, name, style, c, 0.62 * f, ink);
    }

    let weight = (2.2 * k) as f32;
    let (cx, cy) = (ox + 12.0 * f, oy + 12.0 * f);
    icons::draw_icon_weight(canvas, l.icon, cx, cy, box_px as f32, weight, fg(0.5));
    // Each stick: its well, and a cap that tilts with the axes and lights on the click.
    for (well, ax, ay, click) in [(l.ls, "LX", "LY", "LS"), (l.rs, "RX", "RY", "RS")] {
        canvas.draw_circle(at(well), l.well * f, &stroke(fg(0.5), weight));
        let travel = l.well * 0.5;
        let (dx, dy) = (
            state.axis(ax).clamp(-1.0, 1.0),
            state.axis(ay).clamp(-1.0, 1.0),
        );
        let cap = at((well.0 + dx * travel, well.1 + dy * travel));
        let on = state.held(click);
        canvas.draw_circle(
            cap,
            l.well * 0.62 * f,
            &fill(if on { accent(1.0) } else { fg(0.28) }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(held: &[&str], axes: &[(&str, f32)]) -> PadTestState {
        PadTestState {
            held: held.iter().map(|b| (*b).to_string()).collect(),
            axes: axes.iter().map(|(n, v)| ((*n).to_string(), *v)).collect(),
        }
    }

    /// A press or a real stick move becomes the last input; stick noise does not.
    #[test]
    fn the_last_input_is_what_moved() {
        let mut s = InputTestScreen::new();
        s.set_state(state(&["A"], &[]), 0.0);
        assert_eq!(s.last.as_deref(), Some("A pressed"));
        s.set_state(state(&["A"], &[("LX", 0.1)]), 0.1);
        assert_eq!(s.last.as_deref(), Some("A pressed"), "noise");
        s.set_state(state(&[], &[("LX", 0.8)]), 0.2);
        assert_eq!(s.last.as_deref(), Some("LX +0.80"));
    }

    /// B held a second finishes; a tap does not, and letting go restarts the count.
    #[test]
    fn holding_b_finishes() {
        let mut s = InputTestScreen::new();
        s.set_state(state(&["B"], &[]), 0.0);
        s.set_state(state(&[], &[]), 0.5);
        s.set_state(state(&["B"], &[]), 0.6);
        s.set_state(state(&["B"], &[]), 1.4);
        assert!(!s.done, "only 0.8 s since B went down again");
        s.set_state(state(&["B"], &[]), 1.7);
        assert!(s.done);
    }

    /// Every family's controls stay inside its outline box.
    #[test]
    fn every_layout_fits_the_box() {
        use GamepadPref as P;
        for pref in [
            None,
            Some(P::Auto),
            Some(P::Xbox360),
            Some(P::XboxOne),
            Some(P::XboxElite),
            Some(P::DualShock4),
            Some(P::DualSense),
            Some(P::DualSenseEdge),
            Some(P::SwitchPro),
            Some(P::SteamController),
            Some(P::SteamController2),
            Some(P::SteamController2Puck),
            Some(P::SteamDeck),
        ] {
            let l = layout(pref);
            let spots = [l.ls, l.rs, l.dpad, l.face, l.back, l.start, l.guide];
            for (u, v) in spots {
                assert!(
                    (2.0..22.0).contains(&u) && (2.0..22.0).contains(&v),
                    "{pref:?}"
                );
            }
            assert!(l.shoulder.1 - 2.9 >= 0.0, "{pref:?} trigger leaves the box");
        }
    }

    /// `PF_CONSOLE_DUMP=dir cargo test -p pf-console-ui --lib dump_pad_test -- --ignored`
    /// writes each family mid-test for a look.
    #[test]
    #[ignore]
    fn dump_pad_test() {
        use GamepadPref as P;
        let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP");
        let fonts = crate::theme::build_fonts().unwrap();
        let s = state(
            &["A", "RB", "Up", "Start", "LS"],
            &[
                ("LX", -0.7),
                ("LY", 0.5),
                ("RX", 0.9),
                ("LT", 0.4),
                ("RT", 1.0),
            ],
        );
        let prefs = [
            ("xbox360", P::Xbox360),
            ("xboxone", P::XboxOne),
            ("elite", P::XboxElite),
            ("ds4", P::DualShock4),
            ("dualsense", P::DualSense),
            ("edge", P::DualSenseEdge),
            ("switch", P::SwitchPro),
            ("steam", P::SteamController),
            ("steam2", P::SteamController2),
            ("deck", P::SteamDeck),
        ];
        for (name, pref) in prefs {
            let mut surface = skia_safe::surfaces::raster_n32_premul((640, 560)).unwrap();
            surface
                .canvas()
                .clear(skia_safe::Color::from_rgb(18, 20, 28));
            draw_pad(
                surface.canvas(),
                &fonts,
                Some(pref),
                &s,
                20.0,
                0.0,
                600.0,
                1.0,
            );
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/pad-{name}.png"), png.as_bytes()).unwrap();
        }
    }
}
