//! The punktfunk mark: two discs and the lens where they overlap, on the theme's accent.
//!
//! Geometry is the app icon's (`Punktfunk_App-Icon_512.svg`): equal discs of radius a third of
//! the box, centres offset by `(r, -r)`, and a highlight that is NOT their overlap — it is the
//! lens between the deep disc and a copy of itself shifted one radius down the axis, so it runs
//! from the deep rim to the deep centre with its tips 60° either side of the axis, lit from
//! nothing at the rim to the foreground at the centre. The colours follow [`crate::theme`]'s
//! accent rather than the brand violet, so the mark reads on every palette like the chrome.
//!
//! [`draw`] also plays the website's one-shot entrance (`BrandMark.tsx`): the discs orbit in
//! antiphase on an axis into the screen — swelling toward and away from the viewer with a small
//! diagonal sway — and settle exactly onto the resting lens, which fades and scales in over the
//! tail. The caller owns the clock: pass [`intro_progress`] of the seconds since the mark first
//! showed, or `1.0` for the resting mark. Honours [`crate::theme::reduce_motion`].

use skia_safe::{gradient, Canvas, ClipOp, Color4f, Path, Point, TileMode};

use crate::theme::{accent, fg, fill, reduce_motion};

/// How long the entrance takes. [`draw`] at `intro >= 1.0` is the resting mark.
pub const INTRO_SECS: f32 = 1.3;

/// Depth amplitude of the orbit — how much each disc swells and shrinks.
const R_DEPTH: f32 = 0.34;
/// Perspective distance; smaller means stronger scaling.
const PERSP: f32 = 1.05;
/// In-plane breathing along the lens axis, gone at rest.
const SWAY: f32 = 0.06;
/// The website's sway is in its 1000-unit viewBox, where the mark spans 583.6; per box side.
const SWAY_UNITS: f32 = 1000.0 / 583.6;
/// The lens fades in over this slice of the entrance, once the discs are nearly home.
const LENS_FROM: f32 = 0.6;
const LENS_LEN: f32 = 0.45 / INTRO_SECS;

/// Entrance progress after `elapsed` seconds, `0.0..=1.0`.
pub fn intro_progress(elapsed_secs: f32) -> f32 {
    (elapsed_secs / INTRO_SECS).clamp(0.0, 1.0)
}

/// The website's `cubic-bezier(0.22, 1, 0.36, 1)`: a quintic ease-out.
fn ease_out_quint(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u * u * u
}

/// One disc's place at orbit angle `a`: `(dx, dy, scale)` relative to rest, in box sides.
/// `sign` is `1.0` for the light disc and `-1.0` for the deep one — antiphase.
fn orbit(a: f32, sign: f32) -> (f32, f32, f32) {
    let z = sign * a.sin() * R_DEPTH;
    let p = PERSP / (PERSP - z);
    let mag = sign * SWAY * (a.cos() - 1.0) * SWAY_UNITS * p;
    (
        mag * -std::f32::consts::FRAC_1_SQRT_2,
        mag * std::f32::consts::FRAC_1_SQRT_2,
        p,
    )
}

/// Accent moved `t` of the way toward the foreground — the light disc and the lens.
fn toward_fg(t: f32) -> Color4f {
    let (a, g) = (accent(1.0), fg(1.0));
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Color4f::new(mix(a.r, g.r), mix(a.g, g.g), mix(a.b, g.b), 1.0)
}

/// Draws the mark with its box's top-left corner at `(x, y)` and side `side`. `intro` is the
/// entrance's progress (`1.0` = at rest, see [`intro_progress`]).
pub fn draw(canvas: &Canvas, x: f32, y: f32, side: f32, intro: f32) {
    let intro = if reduce_motion() { 1.0 } else { intro };
    let r = side / 3.0;
    let light = (x + r, y + 2.0 * r);
    let deep = (x + 2.0 * r, y + r);
    let a = ease_out_quint(intro) * std::f32::consts::TAU;
    let mut paint = fill(accent(1.0));
    paint.set_anti_alias(true);
    for (centre, sign, colour) in [(light, 1.0, toward_fg(0.4)), (deep, -1.0, accent(1.0))] {
        let (dx, dy, scale) = orbit(a, sign);
        paint.set_color4f(colour, None);
        canvas.draw_circle(
            (centre.0 + dx * side, centre.1 + dy * side),
            r * scale,
            &paint,
        );
    }
    // The highlight fades and scales in about its own centre once the discs are nearly home —
    // the crisp shape only reads against settled discs.
    let lens = ease_out_quint(((intro - LENS_FROM) / LENS_LEN).clamp(0.0, 1.0));
    if lens <= 0.0 {
        return;
    }
    let shift = r * std::f32::consts::FRAC_1_SQRT_2;
    let rim = (deep.0 - shift, deep.1 + shift);
    let (cx, cy) = ((rim.0 + deep.0) / 2.0, (rim.1 + deep.1) / 2.0);
    let grow = 0.6 + 0.4 * lens;
    canvas.save();
    canvas.translate((cx, cy));
    canvas.scale((grow, grow));
    canvas.translate((-cx, -cy));
    canvas.clip_path(&Path::circle(rim, r, None), ClipOp::Intersect, true);
    let mut from = toward_fg(0.68);
    from.a = 0.0;
    let mut to = fg(1.0);
    to.a = lens;
    let mut glow = fill(to);
    glow.set_anti_alias(true);
    glow.set_shader(gradient::shaders::linear_gradient(
        (Point::new(rim.0, rim.1), Point::new(deep.0, deep.1)),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&[from, to], TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_circle(deep, r, &glow);
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full orbit lands on the resting geometry, so the animated mark ends as the static one.
    #[test]
    fn the_orbit_returns_to_rest() {
        for sign in [1.0, -1.0] {
            let (dx, dy, scale) = orbit(std::f32::consts::TAU, sign);
            assert!(
                dx.abs() < 1e-5 && dy.abs() < 1e-5,
                "sway must vanish at rest"
            );
            assert!((scale - 1.0).abs() < 1e-5, "depth must vanish at rest");
        }
        // Mid-orbit the two discs move against each other.
        let (_, _, l) = orbit(std::f32::consts::FRAC_PI_2, 1.0);
        let (_, _, d) = orbit(std::f32::consts::FRAC_PI_2, -1.0);
        assert!(l > 1.0 && d < 1.0);
        assert_eq!(intro_progress(0.0), 0.0);
        assert_eq!(intro_progress(INTRO_SECS * 2.0), 1.0);
    }
}
