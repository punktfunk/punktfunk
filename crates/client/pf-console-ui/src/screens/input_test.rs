//! A live controller test: the driving pad drawn large, each button lit while held, the sticks
//! tilting and the triggers filling, and the last thing that moved named. Raised by the
//! Controllers tab's Test card.
//!
//! While it is on top the shell asks the host for [`PadTestState`]s and the host stops
//! turning the pad into menu moves ([`crate::model::ConsoleCmd::PadTest`]), so B shows as a
//! button instead of backing out. Holding B for [`HOLD_TO_LEAVE`] leaves; a remote or keyboard
//! Back leaves at once.

use crate::glyphs::{Hint, HintKey};
use crate::model::PadTestState;
use crate::pad_art::{self, PadArt, Part};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::{accent, card_face, fg, fill, on_accent, over, shaded, stroke, Fonts, W};
use crate::widgets::blurb;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use punktfunk_core::config::GamepadPref;
use skia_safe::utils::parse_path;
use skia_safe::{
    gradient, BlurStyle, Canvas, Color4f, MaskFilter, PaintCap, PaintJoin, Path, Point, Rect,
    TileMode,
};

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
    /// The drawing for `pref`, parsed once per controller rather than every frame.
    drawing: Option<Drawing>,
    pub(crate) done: bool,
}

impl InputTestScreen {
    pub(crate) fn new() -> InputTestScreen {
        InputTestScreen {
            state: PadTestState::default(),
            last: None,
            b_since: None,
            pref: None,
            drawing: None,
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
}

impl ScreenView for InputTestScreen {
    /// Only a remote or keyboard reaches here while the test is on: its Back leaves.
    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
        }
        None
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![Hint::new(HintKey::Back, "Hold to finish")]
    }

    fn render(
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
        let art = art_for(self.pref);
        if !self
            .drawing
            .as_ref()
            .is_some_and(|d| std::ptr::eq(d.art, art))
        {
            self.drawing = Some(Drawing::new(art));
        }
        let Some(drawing) = &self.drawing else { return };
        let line = 40.0 * k;
        let gap = 16.0 * k;
        let avail_w = (f64::from(rect.width()) * 0.8).min(760.0 * k);
        let avail_h = f64::from(rect.bottom - below.top) - line - 2.0 * gap;
        let scale = (avail_w / f64::from(art.w)).min(avail_h / f64::from(art.h));
        let (w, h) = (f64::from(art.w) * scale, f64::from(art.h) * scale);
        let x = f64::from(rect.center_x()) - w / 2.0;
        let top = f64::from(below.top) + gap;
        draw_pad(canvas, fonts, drawing, &self.state, x, top, scale, k);
        let last = match &self.last {
            Some(input) => format!("Last input \u{2014} {input}"),
            None => "Press a button or move a stick".into(),
        };
        let size = 15.0 * k;
        let tw = f64::from(fonts.measure(&last, W::Regular, size));
        let y = top + h + gap + line * 0.5;
        let cx = f64::from(rect.center_x());
        fonts.draw(canvas, &last, cx - tw / 2.0, y, W::Regular, size, fg(0.6));
    }

    fn title(&self) -> String {
        "Controller test".into()
    }

    /// The whole screen is the test: no tap falls through to the backdrop.
    fn pointer(&mut self, _p: Pointer, _ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        true
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

    /// A stick's deflection, −1…1 each, +y down.
    fn tilt(&self, stick: &str) -> (f32, f32) {
        let (x, y) = if stick == "LS" {
            ("LX", "LY")
        } else {
            ("RX", "RY")
        };
        (self.axis(x).clamp(-1.0, 1.0), self.axis(y).clamp(-1.0, 1.0))
    }
}

/// The drawing for a pad family. Auto is the host's default build, the Xbox 360; an unknown
/// pad draws as the One.
fn art_for(pref: Option<GamepadPref>) -> &'static PadArt {
    let kind = match pref {
        None => "xboxone",
        Some(GamepadPref::Auto) => "xbox360",
        Some(p) => p.as_str(),
    };
    (pad_art::art(kind).or_else(|| pad_art::art("xbox360"))).expect("xbox360 has a master")
}

/// A [`PadArt`] with its path data parsed, one entry per part.
struct Drawing {
    art: &'static PadArt,
    paths: Vec<Option<Path>>,
}

impl Drawing {
    fn new(art: &'static PadArt) -> Drawing {
        let paths = (art.parts.iter())
            .map(|part| match part {
                Part::Body(d)
                | Part::Panel(d)
                | Part::Line(d)
                | Part::Button(_, d)
                | Part::Trigger(_, d)
                | Part::Glyph { d, .. }
                | Part::Mark { d, .. } => parse_path::from_svg(d),
                Part::Stick { .. } | Part::Label { .. } => None,
            })
            .collect();
        Drawing { art, paths }
    }
}

/// A cap's radius and its travel, as fractions of the well's.
const CAP: f32 = 0.72;
const TRAVEL: f32 = 0.42;

/// `drawing` at `scale` px per millimetre, top-left at `(x, y)`, lit from `state`. Every fill
/// is the foreground over an opaque card face, so a bumper hides the trigger behind it. The
/// web console's `PadDiagram.tsx` draws the same parts with the same tones.
#[allow(clippy::too_many_arguments)]
fn draw_pad(
    canvas: &Canvas,
    fonts: &Fonts,
    drawing: &Drawing,
    state: &PadTestState,
    x: f64,
    y: f64,
    scale: f64,
    k: f64,
) {
    let s = scale as f32;
    // One screen pixel, in millimetres.
    let px = k as f32 / s;
    let base = card_face(0.12);
    let tone = |share: f32| over(fg(share), base);
    let black = |share: f32| over(Color4f::new(0.0, 0.0, 0.0, share), base);
    let rim = |color: Color4f, width: f32| {
        let mut p = stroke(color, width * px);
        p.set_stroke_join(PaintJoin::Round);
        p
    };
    let ink = |on: bool| if on { on_accent() } else { tone(0.75) };
    let glow = || {
        let mut p = fill(accent(0.9));
        p.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 1.5 * px, None));
        p
    };

    canvas.save();
    canvas.translate((x as f32, y as f32));
    canvas.scale((s, s));
    for (part, path) in drawing.art.parts.iter().zip(&drawing.paths) {
        match (part, path) {
            (Part::Body(_), Some(path)) => {
                // The shell catches the light from above.
                let b = path.bounds();
                let mut shell = shaded();
                shell.set_shader(gradient::shaders::linear_gradient(
                    (Point::new(0.0, b.top), Point::new(0.0, b.bottom)),
                    &gradient::Gradient::new(
                        gradient::Colors::new_evenly_spaced(
                            &[tone(0.11), tone(0.04)],
                            TileMode::Clamp,
                            None,
                        ),
                        gradient::Interpolation::default(),
                    ),
                    None,
                ));
                canvas.draw_path(path, &shell);
                canvas.draw_path(path, &rim(tone(0.32), 1.5));
            }
            (Part::Panel(_), Some(path)) => {
                canvas.draw_path(path, &fill(black(0.25)));
            }
            (Part::Line(_), Some(path)) => {
                canvas.draw_path(path, &rim(tone(0.22), 1.0));
            }
            (Part::Button(id, _), Some(path)) => {
                let on = state.held(id);
                if on {
                    canvas.draw_path(path, &glow());
                }
                canvas.draw_path(path, &fill(if on { accent(1.0) } else { tone(0.14) }));
                canvas.draw_path(path, &rim(if on { accent(1.0) } else { tone(0.3) }, 1.0));
            }
            (Part::Trigger(id, _), Some(path)) => {
                let pull = if state.held(id) {
                    1.0
                } else {
                    state.axis(id).clamp(0.0, 1.0)
                };
                canvas.draw_path(path, &fill(tone(0.14)));
                if pull > 0.0 {
                    // Fills from the tip as it is pulled.
                    let b = path.bounds();
                    canvas.save();
                    canvas.clip_path(path, None, true);
                    let lit = Rect::from_ltrb(b.left, b.top, b.right, b.top + b.height() * pull);
                    canvas.draw_rect(lit, &fill(accent(1.0)));
                    canvas.restore();
                }
                let edge = if pull > 0.0 { accent(1.0) } else { tone(0.3) };
                canvas.draw_path(path, &rim(edge, 1.0));
            }
            (&Part::Stick { id, cx, cy, r, pad }, _) => {
                let on = state.held(id);
                let (dx, dy) = state.tilt(id);
                let travel = r * if pad { 0.8 } else { TRAVEL };
                let well = if on && pad {
                    over(accent(0.4), base)
                } else {
                    black(0.32)
                };
                canvas.draw_circle((cx, cy), r, &fill(well));
                canvas.draw_circle((cx, cy), r, &rim(tone(0.18), 1.0));
                let cap = Point::new(cx + dx * travel, cy + dy * travel);
                if pad {
                    if dx != 0.0 || dy != 0.0 {
                        canvas.draw_circle(cap, r * 0.16, &fill(accent(1.0)));
                    }
                    continue;
                }
                if on {
                    canvas.draw_circle(cap, r * CAP, &glow());
                }
                let face = if on { accent(1.0) } else { tone(0.24) };
                canvas.draw_circle(cap, r * CAP, &fill(face));
                let edge = if on { accent(1.0) } else { tone(0.45) };
                canvas.draw_circle(cap, r * CAP, &rim(edge, 1.0));
                // The cap's dish.
                let dish = if on { on_accent() } else { tone(0.12) };
                canvas.draw_circle(cap, r * CAP * 0.58, &rim(dish, 1.0));
            }
            (Part::Glyph { on, w, .. }, Some(path)) => {
                let mut p = stroke(ink(state.held(on)), *w);
                p.set_stroke_cap(PaintCap::Round);
                p.set_stroke_join(PaintJoin::Round);
                canvas.draw_path(path, &p);
            }
            (Part::Mark { on, .. }, Some(path)) => {
                canvas.draw_path(path, &fill(ink(state.held(on))));
            }
            (
                &Part::Label {
                    on,
                    x,
                    y,
                    size,
                    text,
                },
                _,
            ) => {
                let size = f64::from(size);
                let w = f64::from(fonts.measure(text, W::SemiBold, size));
                let (lx, ly) = (f64::from(x) - w / 2.0, f64::from(y) + size * 0.36);
                fonts.draw(canvas, text, lx, ly, W::SemiBold, size, ink(state.held(on)));
            }
            // A path that does not parse draws nothing; the master tests keep that out.
            (_, None) => {}
        }
    }
    canvas.restore();
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

    /// Every pad kind has its own drawing; only Auto borrows one.
    #[test]
    fn every_pad_kind_has_a_drawing() {
        for v in 1..=18 {
            let pref = GamepadPref::from_u8(v);
            assert_ne!(pref, GamepadPref::Auto, "{v} is a kind");
            assert!(
                pad_art::art(pref.as_str()).is_some(),
                "{pref:?} has no master"
            );
        }
    }

    /// Every part parses and stays in its box, and every pad can light what a test asks for.
    #[test]
    fn every_master_parses_and_carries_the_core_controls() {
        for v in 1..=18 {
            let pref = GamepadPref::from_u8(v);
            let art = art_for(Some(pref));
            let d = Drawing::new(art);
            let inside = |r: Rect| {
                r.left >= -0.01
                    && r.top >= -0.01
                    && r.right <= art.w + 0.01
                    && r.bottom <= art.h + 0.01
            };
            let mut ids = Vec::new();
            for (part, path) in art.parts.iter().zip(&d.paths) {
                match part {
                    &Part::Stick { id, cx, cy, r, .. } => {
                        ids.push(id);
                        let cap = r * (CAP + TRAVEL);
                        assert!(
                            inside(Rect::from_ltrb(cx - cap, cy - cap, cx + cap, cy + cap)),
                            "{pref:?} {id}"
                        );
                    }
                    Part::Label { .. } => {}
                    Part::Button(id, _) | Part::Trigger(id, _) => {
                        ids.push(id);
                        let p = path
                            .as_ref()
                            .unwrap_or_else(|| panic!("{pref:?} {id} parses"));
                        assert!(
                            inside(p.compute_tight_bounds()),
                            "{pref:?} {id} leaves the box"
                        );
                    }
                    _ => {
                        let p = path
                            .as_ref()
                            .unwrap_or_else(|| panic!("{pref:?} part parses"));
                        assert!(inside(p.compute_tight_bounds()), "{pref:?} leaves the box");
                    }
                }
            }
            for id in [
                "A", "B", "X", "Y", "LB", "RB", "LT", "RT", "LS", "RS", "Up", "Down", "Left",
                "Right", "Start", "Guide",
            ] {
                assert!(ids.contains(&id), "{pref:?} has no {id}");
            }
        }
    }

    /// `PF_CONSOLE_DUMP=dir cargo test -p pf-console-ui --lib dump_pad_test -- --ignored`
    /// writes each pad mid-test for a look.
    #[test]
    #[ignore]
    fn dump_pad_test() {
        let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP");
        let fonts = crate::theme::build_fonts().unwrap();
        let s = state(
            &["A", "RB", "Up", "Start", "LS", "R4", "Touchpad"],
            &[
                ("LX", -0.7),
                ("LY", 0.5),
                ("RX", 0.9),
                ("LT", 0.4),
                ("RT", 1.0),
            ],
        );
        for v in 1..=18 {
            let pref = GamepadPref::from_u8(v);
            let d = Drawing::new(art_for(Some(pref)));
            let scale = 600.0 / f64::from(d.art.w);
            let h = (f64::from(d.art.h) * scale) as i32 + 40;
            let mut surface = skia_safe::surfaces::raster_n32_premul((640, h)).unwrap();
            surface
                .canvas()
                .clear(skia_safe::Color::from_rgb(18, 20, 28));
            draw_pad(surface.canvas(), &fonts, &d, &s, 20.0, 20.0, scale, 1.0);
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/pad-{}.png", pref.as_str()), png.as_bytes()).unwrap();
        }
    }
}
