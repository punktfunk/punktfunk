//! Coverflow geometry and the 4×4 card transform (GTK launcher / Apple coverflow parity).

/// The shelf's largest 2:3 cover, design units. The height fits the field between
/// [`SHELF_COVER_MIN`] and this, as the Apple coverflow's does.
pub const POSTER_W: f64 = 240.0;
pub const POSTER_H: f64 = 360.0;
pub const SHELF_COVER_MIN: f64 = 140.0;
/// Air between two covers on the shelf, and a cover's corner, design units.
pub const SHELF_SPACING: f64 = 34.0;
pub const SHELF_CORNER: f64 = 16.0;
/// One step off focus a cover keeps `1 − RECEDE_SCALE` of its size and `1 − RECEDE_FADE`
/// of its opacity, turned [`ROTATE_DEG`].
pub const RECEDE_SCALE: f64 = 0.24;
pub const RECEDE_FADE: f64 = 0.38;
/// Side-cover yaw about the edge facing focus; the outer edge swings toward the eye.
pub const ROTATE_DEG: f64 = 38.0;
/// The shelf's eye sits `cover height / SHELF_EYE` away (SwiftUI's perspective 0.55).
pub const SHELF_EYE: f64 = 0.55;
/// Perspective depth for the launch hold's tilt, px (CSS `perspective()` semantics).
pub const PERSPECTIVE: f64 = 800.0;

/// `T(cx,cy) · P(depth) · Ry(angle) · S(s) · T(-w/2,-h/2)`: card-local (0..w, 0..h) → screen.
/// Rotation is about the card's vertical centre. Row-major for `Canvas::concat_44`.
#[allow(clippy::too_many_arguments)]
pub fn card_matrix(
    cx: f64,
    cy: f64,
    angle_deg: f64,
    scale: f64,
    w: f64,
    h: f64,
    depth: f64,
) -> [f32; 16] {
    let t1 = translate(cx, cy);
    let p = perspective(depth);
    let r = rotate_y(angle_deg.to_radians());
    let s = scale_xy(scale);
    let t2 = translate(-w / 2.0, -h / 2.0);
    let m = mat_mul(&mat_mul(&mat_mul(&mat_mul(&t1, &p), &r), &s), &t2);
    core::array::from_fn(|i| m[i] as f32)
}

/// A shelf cover's transform, card-local (0..w, 0..h) to screen: scaled about its centre,
/// then turned `angle_deg` about its vertical edge at `pivot_x` (0 or `w`, the side facing
/// focus) with the eye `depth` px away. A positive turn swings the left edge toward the eye.
pub fn shelf_matrix(
    (cx, cy): (f64, f64),
    (w, h): (f64, f64),
    scale: f64,
    angle_deg: f64,
    pivot_x: f64,
    depth: f64,
) -> [f64; 16] {
    let place = translate(cx - w / 2.0, cy - h / 2.0);
    let turn = mat_mul(
        &mat_mul(
            &mat_mul(&translate(pivot_x, h / 2.0), &perspective(depth)),
            &rotate_y(angle_deg.to_radians()),
        ),
        &translate(-pivot_x, -h / 2.0),
    );
    let grow = mat_mul(
        &mat_mul(&translate(w / 2.0, h / 2.0), &scale_xy(scale)),
        &translate(-w / 2.0, -h / 2.0),
    );
    mat_mul(&mat_mul(&place, &turn), &grow)
}

/// Card-local `(x, y)` through `m` to screen, perspective divide included.
pub fn project(m: &[f64; 16], x: f64, y: f64) -> (f64, f64) {
    let w = m[12] * x + m[13] * y + m[15];
    (
        (m[0] * x + m[1] * y + m[3]) / w,
        (m[4] * x + m[5] * y + m[7]) / w,
    )
}

fn translate(x: f64, y: f64) -> [f64; 16] {
    let mut m = identity();
    m[3] = x;
    m[7] = y;
    m
}

fn perspective(d: f64) -> [f64; 16] {
    let mut m = identity();
    m[14] = -1.0 / d; // row 3, col 2 — w' = 1 − z/d (CSS convention)
    m
}

fn rotate_y(rad: f64) -> [f64; 16] {
    let (s, c) = rad.sin_cos();
    let mut m = identity();
    m[0] = c;
    m[2] = s;
    m[8] = -s;
    m[10] = c;
    m
}

fn scale_xy(s: f64) -> [f64; 16] {
    let mut m = identity();
    m[0] = s;
    m[5] = s;
    m
}

fn identity() -> [f64; 16] {
    let mut m = [0.0; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m
}

fn mat_mul(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut out = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Focused card (angle 0, scale 1) maps its centre to (cx, cy) exactly.
    #[test]
    fn card_matrix_centers_the_focused_card() {
        let m = card_matrix(640.0, 400.0, 0.0, 1.0, POSTER_W, POSTER_H, PERSPECTIVE);
        // Apply to the card-local center (w/2, h/2, 0, 1).
        let (x, y) = (POSTER_W as f32 / 2.0, POSTER_H as f32 / 2.0);
        let px = m[0] * x + m[1] * y + m[3];
        let py = m[4] * x + m[5] * y + m[7];
        let pw = m[12] * x + m[13] * y + m[15];
        assert!((px / pw - 640.0).abs() < 0.01, "{}", px / pw);
        assert!((py / pw - 400.0).abs() < 0.01, "{}", py / pw);
    }

    /// Right-side card: inner (left) edge recedes; projected x compresses toward the centre.
    #[test]
    fn side_card_inner_edge_recedes() {
        let flat = card_matrix(900.0, 400.0, 0.0, 1.0, POSTER_W, POSTER_H, PERSPECTIVE);
        let tilted = card_matrix(
            900.0,
            400.0,
            -ROTATE_DEG,
            1.0,
            POSTER_W,
            POSTER_H,
            PERSPECTIVE,
        );
        let project = |m: &[f32; 16], x: f32, y: f32| {
            let px = m[0] * x + m[1] * y + m[3];
            let pw = m[12] * x + m[13] * y + m[15];
            px / pw
        };
        // The inner edge is x=0 in card space. Perspective divide: receding (w < 1 side)
        // pushes it AWAY from the vanishing center — the edge reads as farther.
        let flat_left = project(&flat, 0.0, POSTER_H as f32 / 2.0);
        let tilt_left = project(&tilted, 0.0, POSTER_H as f32 / 2.0);
        let flat_right = project(&flat, POSTER_W as f32, POSTER_H as f32 / 2.0);
        let tilt_right = project(&tilted, POSTER_W as f32, POSTER_H as f32 / 2.0);
        // Tilt narrows the card's projected width (it turned away from the viewer).
        assert!((tilt_right - tilt_left) < (flat_right - flat_left) * 0.95);
    }

    /// Unturned, a cover is its rect. Turned about the edge facing focus, that edge stays
    /// put and the outer edge swings toward the eye, taller than the inner one.
    #[test]
    fn a_shelf_cover_turns_about_the_edge_facing_focus() {
        let (w, h) = (POSTER_W, POSTER_H);
        let depth = h / SHELF_EYE;
        let flat = shelf_matrix((640.0, 400.0), (w, h), 1.0, 0.0, w, depth);
        let (x, y) = super::project(&flat, 0.0, 0.0);
        assert!((x - (640.0 - w / 2.0)).abs() < 1e-6 && (y - (400.0 - h / 2.0)).abs() < 1e-6);
        // Left of focus: the right edge faces it and is the pivot.
        let left = shelf_matrix((300.0, 400.0), (w, h), 1.0, ROTATE_DEG, w, depth);
        let (px, py) = super::project(&left, w, 0.0);
        assert!((px - (300.0 + w / 2.0)).abs() < 1e-6 && (py - (400.0 - h / 2.0)).abs() < 1e-6);
        let tall = |m: &[f64; 16], x: f64| super::project(m, x, h).1 - super::project(m, x, 0.0).1;
        assert!(
            tall(&left, 0.0) > tall(&left, w),
            "the outer edge comes forward"
        );
    }
}
