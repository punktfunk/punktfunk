//! Library model and coverflow math — everything the overlay shares that is not Skia.
//!
//! Games, phase, incoming art, generation and fetch epoch live in [`LibraryShared`].
//! Fetch threads write; the renderer drains per frame. Cursor and grid arithmetic are
//! ported from the GTK launcher and tested here. Geometry, the 4×4 card transform, and
//! the mesh-gradient palettes sit alongside.
//!
//! Rendering is `skia_overlay`. [`PALETTES`] is the one palette table; native pickers read
//! its ids and names over their bridge.

use skia_safe::{ConditionallySend, Image};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

// --- Geometry (GTK launcher / Apple coverflow parity) ---

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
/// Refused-move recoil: the kick against the push, design units/s. A velocity, not a
/// displacement, so the list eases out and springs back rather than jumping.
pub const BUMP_V: f64 = 380.0;
/// Mount entrance ([`crate::anim::Entrance`]): arrival scale, rise (design units), yaw. Shared with the home carousel.
pub const ENTER_SCALE: f64 = 0.96;
pub const ENTER_RISE: f64 = 12.0;
pub const ENTER_TURN_DEG: f64 = 62.0;
pub const JUMP: i32 = 5;

// Semi-implicit Euler, not eased: velocity carries across retargets.
/// Cursor chase: ζ ≈ 0.85 — settles in ~0.3 s with a whisker of overshoot.
pub const SPRING_K: f64 = 200.0;
pub const SPRING_C: f64 = 24.0;
/// Boundary recoil: soft and underdamped (ζ ≈ 0.4) — a rubbery bounce, two visible
/// wobbles, ~0.5 s to rest.
pub const BUMP_K: f64 = 260.0;
pub const BUMP_C: f64 = 13.0;

fn spring_step(pos: f64, vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let vel = vel + (k * (target - pos) - c * vel) * dt;
    (pos + vel * dt, vel)
}

/// One frame of a damped spring, in ≤ 8 ms substeps so a stalled frame stays inside the integrator's stability bound.
pub fn spring_advance(
    mut pos: f64,
    mut vel: f64,
    target: f64,
    k: f64,
    c: f64,
    dt: f64,
) -> (f64, f64) {
    let n = (dt / 0.008).ceil().max(1.0) as usize;
    let h = dt / n as f64;
    for _ in 0..n {
        (pos, vel) = spring_step(pos, vel, target, k, c, h);
    }
    (pos, vel)
}

/// `clamp` lands jumps on the ends; a plain step refuses to leave them.
#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    Moved(i32),
    Boundary,
}

pub fn step_cursor(cursor: i32, len: usize, delta: i32, clamp: bool) -> StepResult {
    if len == 0 {
        return StepResult::Boundary;
    }
    let max = len as i32 - 1;
    let target = if clamp {
        (cursor + delta).clamp(0, max)
    } else {
        cursor + delta
    };
    if target == cursor || target < 0 || target > max {
        StepResult::Boundary
    } else {
        StepResult::Moved(target)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LibraryView {
    Shelf,
    #[default]
    Grid,
}

impl LibraryView {
    /// Persisted `library_view`. Unset or unknown (a newer client's name) is the grid.
    pub fn parse(s: &str) -> LibraryView {
        match s {
            "shelf" => LibraryView::Shelf,
            _ => LibraryView::Grid,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            LibraryView::Shelf => "shelf",
            LibraryView::Grid => "grid",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LibraryView::Shelf => "Shelf",
            LibraryView::Grid => "Grid",
        }
    }

    pub const ALL: [LibraryView; 2] = [LibraryView::Shelf, LibraryView::Grid];
}

/// Grid cell: same 2:3 as the poster at ~⅔ size, so three rows plus a readable detail band at 800-tall.
pub const GRID_W: f64 = 150.0;
pub const GRID_H: f64 = 225.0;
pub const GRID_GAP: f64 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDir {
    Left,
    Right,
    Up,
    Down,
    PageBack,
    PageForward,
}

/// Shoulder jump, rows (≈ one screen).
pub const GRID_PAGE_ROWS: i32 = 3;

/// Layout both cursor math and the renderer read.
///
/// The launcher prefix occupies its own rows; the games section restarts at column 0.
/// A uniform `index % cols` grid only agrees when `launchers` is a multiple of `cols`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridShape {
    /// Cells per row, from the last frame actually drawn — not derived twice from two widths.
    pub cols: usize,
    /// Filtered count; the cursor indexes this.
    pub len: usize,
    /// Where the games section starts, or 0 when the field is one continuous run.
    pub split: usize,
}

impl GridShape {
    /// `launchers` is the leading run. Split only when both halves exist; otherwise a plain grid.
    pub fn new(len: usize, cols: usize, launchers: usize) -> GridShape {
        let split = if launchers > 0 && launchers < len {
            launchers
        } else {
            0
        };
        GridShape { cols, len, split }
    }

    /// First row of the games section; ignore when `split == 0`.
    pub fn split_row(&self) -> usize {
        self.split.div_ceil(self.cols.max(1))
    }

    pub fn cell_of(&self, i: usize) -> (usize, usize) {
        let cols = self.cols.max(1);
        if self.split > 0 && i >= self.split {
            let j = i - self.split;
            (self.split_row() + j / cols, j % cols)
        } else {
            (i / cols, i % cols)
        }
    }

    pub fn rows(&self) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 {
            self.split_row() + (self.len - self.split).div_ceil(cols)
        } else {
            self.len.div_ceil(cols)
        }
    }

    pub fn row_start(&self, row: usize) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 && row >= self.split_row() {
            self.split + (row - self.split_row()) * cols
        } else {
            row * cols
        }
    }

    /// Cells this row holds. The last launcher row ends at `split`; the last field row at `len`.
    pub fn row_len(&self, row: usize) -> usize {
        let start = self.row_start(row);
        let end = if self.split > 0 && row + 1 == self.split_row() {
            self.split
        } else {
            self.len
        };
        end.saturating_sub(start).min(self.cols.max(1))
    }
}

/// Grid cursor against the shape the renderer is drawing.
///
/// Horizontal: walk the row, refuse at that row's true ends (no wrap). Vertical and page:
/// change row only, carrying `col_hint` clamped into the target row. Only leaving the
/// grid is a boundary. A short row is a layout accident — Down clamps onto the last title.
/// `col_hint` is the column last chosen ([`grid_col_hint`]), so a two-wide launcher row
/// is reversible.
pub fn grid_step(cursor: i32, shape: GridShape, col_hint: usize, dir: GridDir) -> StepResult {
    if shape.len == 0 || shape.cols == 0 {
        return StepResult::Boundary;
    }
    // Outside the field: library shortened. Nearest real cell, so the next press heals it.
    let (row, col) = shape.cell_of((cursor.max(0) as usize).min(shape.len - 1));
    let moved = |i: usize| {
        if i as i32 == cursor {
            StepResult::Boundary
        } else {
            StepResult::Moved(i as i32)
        }
    };
    match dir {
        GridDir::Left => {
            if col == 0 {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col - 1)
            }
        }
        GridDir::Right => {
            if col + 1 >= shape.row_len(row) {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col + 1)
            }
        }
        GridDir::Up | GridDir::Down | GridDir::PageBack | GridDir::PageForward => {
            let (d, paging) = match dir {
                GridDir::Up => (-1, false),
                GridDir::Down => (1, false),
                GridDir::PageBack => (-GRID_PAGE_ROWS, true),
                _ => (GRID_PAGE_ROWS, true),
            };
            let target = (row as i32 + d).clamp(0, shape.rows() as i32 - 1) as usize;
            if target == row {
                // Step at the edge refuses. Page is clamped `step_cursor`: land on this row's end.
                if !paging {
                    return StepResult::Boundary;
                }
                let c = if d > 0 { shape.row_len(row) - 1 } else { 0 };
                return moved(shape.row_start(row) + c);
            }
            let c = col_hint.min(shape.row_len(target) - 1);
            moved(shape.row_start(target) + c)
        }
    }
}

/// Remembered column after a move: a horizontal step chooses it; a vertical step only borrows it.
pub fn grid_col_hint(shape: GridShape, prev: usize, dir: GridDir, landed: i32) -> usize {
    match dir {
        GridDir::Left | GridDir::Right => shape.cell_of(landed.max(0) as usize).1,
        _ => prev,
    }
}

// --- 4×4 matrix (row-major) — coverflow card transform ---

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

// --- Mesh-gradient background (Swift `GamepadScreenBackground` MeshGradient) ---

/// 16 mesh colours, row-major 4×4 sRGB. Verbatim Swift `meshColors`.
pub const MESH_COLORS: [(f64, f64, f64); 16] = [
    (0.075, 0.060, 0.160),
    (0.34, 0.27, 0.72),
    (0.30, 0.26, 0.74),
    (0.075, 0.060, 0.160),
    (0.42, 0.20, 0.54),
    (0.49, 0.39, 0.95),
    (0.28, 0.31, 0.84),
    (0.16, 0.26, 0.64),
    (0.45, 0.23, 0.60),
    (0.53, 0.31, 0.75),
    (0.35, 0.35, 0.91),
    (0.19, 0.28, 0.70),
    (0.075, 0.060, 0.160),
    (0.22, 0.18, 0.54),
    (0.24, 0.20, 0.58),
    (0.075, 0.060, 0.160),
];

// --- Background palettes -------------------------------------------------------------------

/// One background colour family.
///
/// `stops` is several distinct hues, not one hue at several brightnesses. The 4×4 mesh
/// samples that ramp diagonally with [`CELL_RAMP`]. [`Palette::accent`] is the focus wash;
/// [`Palette::light`] flips ink ([`crate::theme::Ink`]).
pub struct Palette {
    /// The stored `ui_palette` value (see `trust::Settings::ui_palette`).
    pub id: &'static str,
    pub name: &'static str,
    /// Colour ramp, dark end first: the field's gradient ([`field_sksl`]) and the mesh.
    /// `None` = [`MESH_COLORS`] verbatim for the mesh and [`VIOLET_FIELD`] for the field.
    pub stops: Option<&'static [(f64, f64, f64)]>,
    /// The field's ground — what the corners settle onto and what the calm mix lifts toward.
    pub ground: (f64, f64, f64),
    pub accent: (f64, f64, f64),
    /// Pale field: dark ink, white scrims.
    pub light: bool,
}

/// Per-cell ramp offset on top of the diagonal `0.5·(x + y)`. Nudges stop a pure diagonal from banding.
#[rustfmt::skip]
pub(crate) const CELL_RAMP: [f64; 16] = [
     0.10, -0.06,  0.04, -0.12,
    -0.08,  0.14, -0.10,  0.06,
     0.06, -0.12,  0.16, -0.04,
    -0.10,  0.08, -0.06,  0.12,
];

/// Brand default, 19 more dark fields, then 16 pale. Dark → light is cycle order. An `id` is a
/// stored `ui_palette` value: renaming one orphans saved choices.
#[rustfmt::skip]
pub const PALETTES: [Palette; 36] = [
    // --- dark fields (white ink) ---
    Palette {
        // The brand default: a bright periwinkle field with lavender pools, still white ink.
        id: "violet", name: "Violet", stops: None,
        ground: (0.510, 0.470, 0.960), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        // First two stops are (0,0,0): OLED pixels off, not dark grey. Ground is black so calm lifts to nothing.
        // Id stays `"oled"` — stored `ui_palette` key; renaming orphans saved choices.
        id: "oled", name: "Eclipse",
        stops: Some(&[
            (0.00, 0.00, 0.00), (0.00, 0.00, 0.00), (0.01, 0.02, 0.10),
            (0.045, 0.016, 0.115), (0.12, 0.024, 0.13),
        ]),
        ground: (0.000, 0.000, 0.000), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        // Nothing at all: every pixel off. The accent is the only colour on it.
        id: "void", name: "Void",
        stops: Some(&[
            (0.00, 0.00, 0.00), (0.00, 0.00, 0.00), (0.00, 0.00, 0.00),
            (0.00, 0.00, 0.00), (0.00, 0.00, 0.00),
        ]),
        ground: (0.000, 0.000, 0.000), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        id: "graphite", name: "Graphite",
        stops: Some(&[
            (0.06, 0.07, 0.11), (0.15, 0.18, 0.25), (0.30, 0.31, 0.35),
            (0.45, 0.42, 0.38), (0.60, 0.56, 0.49),
        ]),
        ground: (0.055, 0.055, 0.070), accent: (0.78, 0.80, 0.86), light: false,
    },
    Palette {
        // Cool neutral: blue-grey warming to stone at the top.
        id: "slate", name: "Slate",
        stops: Some(&[
            (0.06, 0.08, 0.11), (0.14, 0.18, 0.24), (0.24, 0.30, 0.38),
            (0.40, 0.44, 0.48), (0.60, 0.58, 0.52),
        ]),
        ground: (0.060, 0.080, 0.110), accent: (0.60, 0.80, 1.00), light: false,
    },
    Palette {
        // Indigo shadow, cobalt body, a teal break.
        id: "midnight", name: "Midnight",
        stops: Some(&[
            (0.05, 0.02, 0.16), (0.05, 0.11, 0.36), (0.08, 0.24, 0.60),
            (0.14, 0.42, 0.80), (0.36, 0.76, 0.86),
        ]),
        ground: (0.030, 0.050, 0.160), accent: (0.40, 0.72, 1.00), light: false,
    },
    Palette {
        // Ultraviolet into an electric blue and cyan.
        id: "electric", name: "Electric",
        stops: Some(&[
            (0.02, 0.00, 0.10), (0.14, 0.02, 0.50), (0.30, 0.10, 0.95),
            (0.10, 0.45, 1.00), (0.20, 0.90, 1.00),
        ]),
        ground: (0.020, 0.000, 0.100), accent: (0.45, 0.85, 1.00), light: false,
    },
    Palette {
        // Deep water rising through teal to foam.
        id: "ocean", name: "Ocean",
        stops: Some(&[
            (0.01, 0.05, 0.14), (0.02, 0.18, 0.40), (0.02, 0.40, 0.62),
            (0.05, 0.66, 0.72), (0.55, 0.92, 0.80),
        ]),
        ground: (0.010, 0.050, 0.140), accent: (0.45, 0.95, 0.90), light: false,
    },
    Palette {
        // Deep teal into green, then a violet curtain.
        id: "aurora", name: "Aurora",
        stops: Some(&[
            (0.02, 0.07, 0.11), (0.03, 0.24, 0.28), (0.05, 0.46, 0.40),
            (0.14, 0.60, 0.72), (0.44, 0.42, 0.86),
        ]),
        ground: (0.020, 0.070, 0.110), accent: (0.36, 0.90, 0.78), light: false,
    },
    Palette {
        // Deep water into jade and a leaf-green lift.
        id: "jade", name: "Jade",
        stops: Some(&[
            (0.02, 0.07, 0.10), (0.03, 0.22, 0.19), (0.05, 0.40, 0.34),
            (0.16, 0.58, 0.46), (0.60, 0.84, 0.52),
        ]),
        ground: (0.020, 0.070, 0.100), accent: (0.52, 0.90, 0.62), light: false,
    },
    Palette {
        // Saturated green, forest floor to lime.
        id: "emerald", name: "Emerald",
        stops: Some(&[
            (0.01, 0.07, 0.04), (0.02, 0.26, 0.12), (0.04, 0.50, 0.22),
            (0.16, 0.74, 0.34), (0.60, 0.92, 0.40),
        ]),
        ground: (0.010, 0.070, 0.040), accent: (0.55, 1.00, 0.55), light: false,
    },
    Palette {
        // Plum shadow, crimson body, a coral edge.
        id: "crimson", name: "Crimson",
        stops: Some(&[
            (0.10, 0.02, 0.10), (0.34, 0.03, 0.12), (0.62, 0.06, 0.20),
            (0.86, 0.20, 0.28), (0.98, 0.52, 0.32),
        ]),
        ground: (0.080, 0.020, 0.060), accent: (1.00, 0.42, 0.42), light: false,
    },
    Palette {
        // Violet shadow under a hot pink-red.
        id: "ruby", name: "Ruby",
        stops: Some(&[
            (0.06, 0.00, 0.20), (0.40, 0.02, 0.16), (0.80, 0.06, 0.30),
            (1.00, 0.28, 0.48), (1.00, 0.62, 0.56),
        ]),
        ground: (0.080, 0.000, 0.060), accent: (1.00, 0.50, 0.62), light: false,
    },
    Palette {
        // Black rock, red heat, a yellow glow.
        id: "lava", name: "Lava",
        stops: Some(&[
            (0.06, 0.01, 0.02), (0.42, 0.02, 0.04), (0.86, 0.12, 0.02),
            (1.00, 0.45, 0.02), (1.00, 0.85, 0.20),
        ]),
        ground: (0.060, 0.010, 0.020), accent: (1.00, 0.72, 0.20), light: false,
    },
    Palette {
        // Bronze shadow under copper, a verdigris lift.
        id: "copper", name: "Copper",
        stops: Some(&[
            (0.08, 0.05, 0.04), (0.36, 0.15, 0.08), (0.66, 0.32, 0.14),
            (0.85, 0.56, 0.26), (0.50, 0.78, 0.62),
        ]),
        ground: (0.070, 0.050, 0.040), accent: (1.00, 0.70, 0.36), light: false,
    },
    Palette {
        // Wine shadow into amber and gold.
        id: "amber", name: "Amber",
        stops: Some(&[
            (0.10, 0.02, 0.10), (0.36, 0.16, 0.02), (0.66, 0.36, 0.04),
            (0.88, 0.58, 0.08), (0.92, 0.86, 0.36),
        ]),
        ground: (0.100, 0.040, 0.020), accent: (1.00, 0.80, 0.30), light: false,
    },
    Palette {
        // Indigo through mauve to a peach horizon.
        id: "dusk", name: "Dusk",
        stops: Some(&[
            (0.08, 0.04, 0.14), (0.26, 0.10, 0.34), (0.50, 0.20, 0.48),
            (0.78, 0.38, 0.50), (0.96, 0.62, 0.48),
        ]),
        ground: (0.070, 0.040, 0.120), accent: (1.00, 0.62, 0.56), light: false,
    },
    Palette {
        // Purple climbing to orchid and pink.
        id: "grape", name: "Grape",
        stops: Some(&[
            (0.08, 0.02, 0.16), (0.28, 0.06, 0.48), (0.52, 0.14, 0.78),
            (0.78, 0.30, 0.92), (1.00, 0.55, 0.80),
        ]),
        ground: (0.080, 0.020, 0.160), accent: (0.85, 0.55, 1.00), light: false,
    },
    Palette {
        // Magenta, electric blue and a lime flash on black.
        id: "neon", name: "Neon",
        stops: Some(&[
            (0.05, 0.00, 0.12), (0.40, 0.00, 0.60), (0.90, 0.05, 0.55),
            (0.15, 0.35, 0.95), (0.30, 0.95, 0.55),
        ]),
        ground: (0.050, 0.000, 0.120), accent: (0.40, 1.00, 0.70), light: false,
    },
    Palette {
        // Teal shade, orange sun, a pink bloom.
        id: "tropic", name: "Tropic",
        stops: Some(&[
            (0.02, 0.10, 0.12), (0.02, 0.42, 0.42), (0.95, 0.45, 0.10),
            (0.98, 0.20, 0.45), (0.40, 0.10, 0.55),
        ]),
        ground: (0.020, 0.080, 0.100), accent: (1.00, 0.60, 0.30), light: false,
    },
    // --- pale fields (dark ink) ---
    Palette {
        // Near-white: warm cream, cool blue and a rose tint in turn.
        id: "paper", name: "Paper",
        stops: Some(&[
            (0.99, 0.95, 0.88), (0.91, 0.94, 0.98), (0.98, 0.91, 0.94),
            (0.99, 0.97, 0.89), (0.90, 0.94, 0.99),
        ]),
        ground: (0.970, 0.960, 0.940), accent: (0.42, 0.30, 0.28), light: true,
    },
    Palette {
        // Pale blue through periwinkle to a mint edge.
        id: "sky", name: "Sky",
        stops: Some(&[
            (0.76, 0.87, 1.00), (0.62, 0.78, 0.99), (0.72, 0.76, 0.99),
            (0.84, 0.82, 1.00), (0.86, 0.98, 0.96),
        ]),
        ground: (0.920, 0.950, 1.000), accent: (0.12, 0.30, 0.62), light: true,
    },
    Palette {
        // Saturated sky blue cooling into violet.
        id: "glacier", name: "Glacier",
        stops: Some(&[
            (0.45, 0.70, 1.00), (0.60, 0.80, 1.00), (0.75, 0.85, 1.00),
            (0.85, 0.80, 1.00), (0.95, 0.85, 1.00),
        ]),
        ground: (0.780, 0.880, 1.000), accent: (0.10, 0.20, 0.55), light: true,
    },
    Palette {
        // Lavender and periwinkle, warming to a pink bloom.
        id: "lilac", name: "Lilac",
        stops: Some(&[
            (0.84, 0.76, 0.99), (0.74, 0.70, 0.99), (0.88, 0.74, 0.98),
            (0.98, 0.82, 0.94), (0.96, 0.94, 1.00),
        ]),
        ground: (0.950, 0.920, 0.990), accent: (0.44, 0.24, 0.66), light: true,
    },
    Palette {
        // Vivid purple and periwinkle, a pink edge.
        id: "iris", name: "Iris",
        stops: Some(&[
            (0.60, 0.35, 0.95), (0.55, 0.50, 1.00), (0.65, 0.65, 1.00),
            (0.85, 0.60, 0.98), (1.00, 0.70, 0.90),
        ]),
        ground: (0.800, 0.720, 1.000), accent: (0.25, 0.05, 0.55), light: true,
    },
    Palette {
        // Hot pink cooling into a baby blue.
        id: "bubblegum", name: "Bubblegum",
        stops: Some(&[
            (1.00, 0.50, 0.80), (1.00, 0.62, 0.85), (0.92, 0.70, 0.95),
            (0.70, 0.75, 1.00), (0.60, 0.85, 1.00),
        ]),
        ground: (1.000, 0.780, 0.900), accent: (0.55, 0.05, 0.35), light: true,
    },
    Palette {
        // Pink into coral, fading to a warm cream.
        id: "coral", name: "Coral",
        stops: Some(&[
            (1.00, 0.58, 0.62), (1.00, 0.68, 0.56), (1.00, 0.80, 0.64),
            (0.99, 0.88, 0.72), (1.00, 0.96, 0.80),
        ]),
        ground: (1.000, 0.900, 0.800), accent: (0.68, 0.14, 0.22), light: true,
    },
    Palette {
        // Hot pink into orange, fully saturated.
        id: "flamingo", name: "Flamingo",
        stops: Some(&[
            (1.00, 0.30, 0.60), (1.00, 0.42, 0.55), (1.00, 0.55, 0.45),
            (1.00, 0.70, 0.45), (1.00, 0.85, 0.60),
        ]),
        ground: (1.000, 0.720, 0.660), accent: (0.50, 0.00, 0.20), light: true,
    },
    Palette {
        // Pink into peach and apricot.
        id: "peach", name: "Peach",
        stops: Some(&[
            (1.00, 0.45, 0.60), (1.00, 0.60, 0.44), (1.00, 0.72, 0.52),
            (1.00, 0.82, 0.58), (0.98, 0.92, 0.72),
        ]),
        ground: (1.000, 0.820, 0.660), accent: (0.60, 0.16, 0.10), light: true,
    },
    Palette {
        // Pink, peach, lime and sky in one bag.
        id: "candy", name: "Candy",
        stops: Some(&[
            (1.00, 0.40, 0.70), (1.00, 0.55, 0.60), (1.00, 0.75, 0.40),
            (0.80, 0.90, 0.50), (0.55, 0.85, 0.95),
        ]),
        ground: (1.000, 0.800, 0.750), accent: (0.55, 0.05, 0.30), light: true,
    },
    Palette {
        // Lemon through lime to a pale green.
        id: "lemon", name: "Lemon",
        stops: Some(&[
            (1.00, 0.86, 0.44), (0.98, 0.96, 0.56), (0.70, 0.94, 0.64),
            (0.84, 0.97, 0.78), (0.98, 0.99, 0.90),
        ]),
        ground: (1.000, 0.980, 0.860), accent: (0.30, 0.36, 0.08), light: true,
    },
    Palette {
        // Orange into a full yellow and a green edge.
        id: "sunflower", name: "Sunflower",
        stops: Some(&[
            (1.00, 0.60, 0.10), (1.00, 0.75, 0.10), (1.00, 0.88, 0.20),
            (0.95, 0.95, 0.40), (0.75, 0.92, 0.60),
        ]),
        ground: (1.000, 0.880, 0.400), accent: (0.40, 0.22, 0.00), light: true,
    },
    Palette {
        // Orange, lemon and lime scoops.
        id: "sherbet", name: "Sherbet",
        stops: Some(&[
            (1.00, 0.55, 0.25), (1.00, 0.72, 0.30), (1.00, 0.90, 0.40),
            (0.85, 0.95, 0.50), (0.60, 0.92, 0.70),
        ]),
        ground: (1.000, 0.850, 0.600), accent: (0.45, 0.18, 0.02), light: true,
    },
    Palette {
        // Sage into pale olive and cream.
        id: "sage", name: "Sage",
        stops: Some(&[
            (0.72, 0.84, 0.70), (0.80, 0.90, 0.76), (0.90, 0.94, 0.80),
            (0.96, 0.96, 0.84), (0.99, 0.97, 0.90),
        ]),
        ground: (0.940, 0.960, 0.900), accent: (0.18, 0.36, 0.24), light: true,
    },
    Palette {
        // Grass green into a warm yellow.
        id: "meadow", name: "Meadow",
        stops: Some(&[
            (0.30, 0.80, 0.40), (0.55, 0.90, 0.40), (0.80, 0.95, 0.45),
            (0.95, 0.95, 0.55), (1.00, 0.90, 0.65),
        ]),
        ground: (0.850, 0.950, 0.600), accent: (0.10, 0.35, 0.12), light: true,
    },
    Palette {
        // Turquoise water into a pale green shore.
        id: "lagoon", name: "Lagoon",
        stops: Some(&[
            (0.20, 0.75, 0.80), (0.35, 0.85, 0.85), (0.55, 0.92, 0.80),
            (0.70, 0.95, 0.70), (0.92, 0.98, 0.75),
        ]),
        ground: (0.750, 0.950, 0.900), accent: (0.02, 0.30, 0.35), light: true,
    },
];

/// Palette for `id`, or the brand default. Unknown is a newer client's name, not an empty draw.
pub fn palette(id: &str) -> &'static Palette {
    PALETTES.iter().find(|p| p.id == id).unwrap_or(&PALETTES[0])
}

/// Sample a ramp at `t` ∈ [0, 1], linear between neighbouring stops.
pub fn ramp(stops: &[(f64, f64, f64)], t: f64) -> (f64, f64, f64) {
    match stops.len() {
        0 => (0.0, 0.0, 0.0),
        1 => stops[0],
        n => {
            let x = t.clamp(0.0, 1.0) * (n - 1) as f64;
            let i = (x.floor() as usize).min(n - 2);
            let f = x - i as f64;
            let (a, b) = (stops[i], stops[i + 1]);
            (
                a.0 + (b.0 - a.0) * f,
                a.1 + (b.1 - a.1) * f,
                a.2 + (b.2 - a.2) * f,
            )
        }
    }
}

impl Palette {
    pub fn mesh_colors(&self) -> [(f64, f64, f64); 16] {
        let Some(stops) = self.stops else {
            return MESH_COLORS;
        };
        mesh_colors_of(stops)
    }
}

/// 16 mesh cells from any 5-stop ramp, including the runtime OS field (`shell::build_mesh_os`).
pub(crate) fn mesh_colors_of(stops: &[(f64, f64, f64)]) -> [(f64, f64, f64); 16] {
    core::array::from_fn(|i| {
        let (x, y) = ((i % 4) as f64 / 3.0, (i / 4) as f64 / 3.0);
        ramp(stops, 0.5 * (x + y) + CELL_RAMP[i])
    })
}

/// The brand default's field ramp: the mockup's lavender → periwinkle → magenta.
pub const VIOLET_FIELD: [(f64, f64, f64); 3] =
    [(0.80, 0.60, 0.98), (0.47, 0.44, 1.00), (0.98, 0.12, 0.62)];

/// An sRGB colour in OKLab, as the field's gradient mixes it.
fn oklab((r, g, b): (f64, f64, f64)) -> (f64, f64, f64) {
    let lin = |c: f64| {
        let v = c.max(0.0);
        if v < 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b)
        .max(0.0)
        .cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b)
        .max(0.0)
        .cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b)
        .max(0.0)
        .cbrt();
    (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )
}

/// The field's clock rates: the sphere turns at `t · ROT`, its surface morphs at `t · MORPH`.
const ROT: f64 = 0.03;
const MORPH: f64 = 0.9;

/// What the field's time alone decides, for its uniform block: the three rows of the
/// world-to-sphere rotation, then the primary and warp drift, each a `float4` with `w` unused.
/// Once a render on the CPU instead of once a pixel on the GPU.
pub fn field_motion(t: f64) -> [f32; 20] {
    type V = [f64; 3];
    let norm = |v: V| {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / l, v[1] / l, v[2] / l]
    };
    let wrap = |ph: f64| ph - (ph / std::f64::consts::TAU).floor() * std::f64::consts::TAU;
    let rotate = |p: V, a: V, angle: f64| {
        let (c, s) = (angle.cos(), angle.sin());
        let d = a[0] * p[0] + a[1] * p[1] + a[2] * p[2];
        let x = [
            a[1] * p[2] - a[2] * p[1],
            a[2] * p[0] - a[0] * p[2],
            a[0] * p[1] - a[1] * p[0],
        ];
        [0, 1, 2].map(|i| p[i] * c + x[i] * s + a[i] * d * (1.0 - c))
    };
    let rt = t * ROT;
    let unorient = |p: V| {
        let (c, s) = (0.24f64.cos(), 0.24f64.sin());
        let o = [p[0], c * p[1] - s * p[2], s * p[1] + c * p[2]];
        let o = rotate(o, norm([0.58, -0.69, 0.43]), -wrap(rt * 0.1732051));
        let o = rotate(o, norm([-0.71, 0.29, 0.64]), -wrap(rt * 0.2236068));
        rotate(o, norm([0.36, 0.81, 0.46]), -wrap(rt * 0.287))
    };
    let cols = [
        unorient([1.0, 0.0, 0.0]),
        unorient([0.0, 1.0, 0.0]),
        unorient([0.0, 0.0, 1.0]),
    ];
    let mt = t * MORPH;
    let curved = |rates: V, phases: V| {
        [
            wrap(mt * rates[0] + phases[0]).sin(),
            wrap(mt * rates[1] + phases[1]).sin(),
            wrap(mt * rates[2] + phases[2]).cos(),
        ]
    };
    let drift = |a: V, ka: f64, b: V, kb: f64, c: V, kc: f64| {
        let (a, b) = (norm(a), norm(b));
        [0, 1, 2].map(|i| a[i] * mt * ka + b[i] * mt * kb + c[i] * kc)
    };
    let primary = drift(
        [0.73, -0.41, 0.55],
        0.105,
        [-0.28, 0.91, 0.31],
        0.023,
        curved([0.071, 0.043, 0.029], [0.0, 1.73, 4.11]),
        0.16,
    );
    let warp = drift(
        [-0.46, 0.38, 0.80],
        0.137,
        [0.84, 0.51, -0.18],
        0.031,
        curved([0.089, 0.053, 0.034], [2.21, 5.07, 0.83]),
        0.12,
    );
    let mut out = [0.0f32; 20];
    for row in 0..3 {
        for (col, c) in cols.iter().enumerate() {
            out[row * 4 + col] = c[row] as f32;
        }
    }
    for i in 0..3 {
        out[12 + i] = primary[i] as f32;
        out[16 + i] = warp[i] as f32;
    }
    out
}

/// The camera the field is seen through, from the surface's aspect: `(focal length,
/// sphere scale)`. The Figma shader's cover-zoom rule at its 72 % zoom, so the noise
/// sphere fills any glass the same way it fills the mockup's frame.
pub fn field_camera(aspect: f64) -> (f64, f64) {
    let diagonal = (1.0 + aspect * aspect).sqrt();
    let (safe, cam, base_f) = (0.72, 3.0, 1.73);
    let target_f = base_f * 4.0;
    let required = cam * diagonal / (target_f * target_f + diagonal * diagonal).sqrt();
    let scale = (required / safe).max(0.82);
    let conservative = (cam - 0.001).min(scale * safe);
    let depth = (cam * cam - conservative * conservative).max(0.0001).sqrt();
    let min_cover = (diagonal * depth / (base_f * conservative) * 1.12).clamp(0.5, 10.0);
    let minimum = (min_cover * 0.65).clamp(0.5, 10.0);
    let zoom = (minimum * (10.0 / minimum).powf(0.72)).clamp(minimum, 10.0);
    (base_f * zoom, scale)
}

/// The field as SkSL: the Figma Community "Moving gradient" shader's maths, per pixel
/// (figma.com/community/shader/1676361123401176242; its author and licence belong in the
/// clients' third-party notices). Its vertex
/// stage displaced a sphere by a Perlin height field and coloured each point by that
/// height through an OKLab gradient; here a pixel's view ray meets the sphere, the hit's
/// direction gives the height, the sphere is re-sized by it and hit once more, and the
/// second height picks the colour. Detail 3.33, intensity 4.29, flow 0.26, twist 0.04 and
/// speed 12 % are the mockup's; morph runs at a quarter of its 3.74, which read too fast on
/// a phone. `stops` are the gradient, even spaced.
///
/// `u_tc.y` is the calm mix (0 launcher, 1 form): flatten toward `u_lift` so a screen
/// crossfade never jumps the field. `u_cam` is [`field_camera`].
pub fn field_sksl(ground: (f64, f64, f64), stops: &[(f64, f64, f64)]) -> String {
    let rgb = |c: (f64, f64, f64)| {
        let (l, a, b) = oklab(c);
        format!("float3({l}, {a}, {b})")
    };
    let n = stops.len().max(2);
    let stops: Vec<(f64, f64, f64)> = if stops.len() < 2 {
        vec![ground, ground]
    } else {
        stops.to_vec()
    };
    // One segment per pair of stops, picked by an if-chain: a two-stop ramp is one bare segment.
    let mut segs = String::new();
    for i in 0..n - 1 {
        let (a, b) = (rgb(stops[i]), rgb(stops[i + 1]));
        let seg = format!("a = {a}; b = {b}; f = x - {i}.0;");
        segs.push_str(&if n == 2 {
            format!("    {seg}\n")
        } else if i == 0 {
            format!("    if (x < 1.0) {{ {seg} }}\n")
        } else if i == n - 2 {
            format!("    else {{ {seg} }}\n")
        } else {
            format!("    else if (x < {}.0) {{ {seg} }}\n", i + 1)
        });
    }
    format!(
        "uniform float2 u_res;\n\
         // x = seconds since the shell started, y = the calm mix (0 launcher, 1 form).\n\
         uniform float2 u_tc;\n\
         // rgb = the palette's ground scaled for the calm lift; a is unused (float4 so\n\
         // the uniform block stays 16-byte aligned under any packing rule).\n\
         uniform float4 u_lift;\n\
         // rgb = what the vignette and scrims tend toward (black under a dark palette, white\n\
         // under a pale one — darkening a pastel field would strand the dark text on it), and\n\
         // a = how hard. A pale field needs far less: mixing toward white at the dark field's\n\
         // strength bleaches the chroma straight out of the gradient.\n\
         uniform float4 u_scrim;\n\
         // x = focal length, y = sphere scale (`field_camera`), z = passes over the sphere\n\
         // (1 = the plain sphere's height, 2 = re-hit at that height: the displaced surface).\n\
         uniform float4 u_cam;\n\
         // The sphere's turn (rows of the world-to-sphere rotation) and its drift at this\n\
         // instant: xyz of each, from `field_motion`, so no pixel pays for time alone.\n\
         uniform float4 u_rot0;\n\
         uniform float4 u_rot1;\n\
         uniform float4 u_rot2;\n\
         uniform float4 u_mot;\n\
         uniform float4 u_wmot;\n\
         \n\
         const float DETAIL = 3.33;\n\
         const float INTENSITY = 4.29;\n\
         const float TWIST = 0.04;\n\
         const float WARP = 0.26;\n\
         \n\
         float3 hash33(float3 p) {{\n\
         \x20   float3 q = float3(dot(p, float3(127.1, 311.7, 74.7)),\n\
         \x20                     dot(p, float3(269.5, 183.3, 246.1)),\n\
         \x20                     dot(p, float3(113.5, 271.9, 124.6)));\n\
         \x20   return fract(sin(q) * 43758.5453) * 2.0 - 1.0;\n\
         }}\n\
         float3 smoother(float3 t) {{ return t * t * t * (t * (t * 6.0 - 15.0) + 10.0); }}\n\
         float gradDot(float3 cell, float3 off, float3 local) {{\n\
         \x20   return dot(hash33(cell + off), local - off);\n\
         }}\n\
         float perlin3(float3 p) {{\n\
         \x20   float3 cell = floor(p); float3 local = fract(p); float3 w = smoother(local);\n\
         \x20   float n000 = gradDot(cell, float3(0.0, 0.0, 0.0), local);\n\
         \x20   float n100 = gradDot(cell, float3(1.0, 0.0, 0.0), local);\n\
         \x20   float n010 = gradDot(cell, float3(0.0, 1.0, 0.0), local);\n\
         \x20   float n110 = gradDot(cell, float3(1.0, 1.0, 0.0), local);\n\
         \x20   float n001 = gradDot(cell, float3(0.0, 0.0, 1.0), local);\n\
         \x20   float n101 = gradDot(cell, float3(1.0, 0.0, 1.0), local);\n\
         \x20   float n011 = gradDot(cell, float3(0.0, 1.0, 1.0), local);\n\
         \x20   float n111 = gradDot(cell, float3(1.0, 1.0, 1.0), local);\n\
         \x20   float nx00 = mix(n000, n100, w.x); float nx10 = mix(n010, n110, w.x);\n\
         \x20   float nx01 = mix(n001, n101, w.x); float nx11 = mix(n011, n111, w.x);\n\
         \x20   return mix(mix(nx00, nx10, w.y), mix(nx01, nx11, w.y), w.z) * 1.1547;\n\
         }}\n\
         float3 rotateOctave(float3 p) {{\n\
         \x20   return float3(0.80 * p.y + 0.60 * p.z,\n\
         \x20                 -0.80 * p.x + 0.36 * p.y - 0.48 * p.z,\n\
         \x20                 -0.60 * p.x - 0.48 * p.y + 0.64 * p.z);\n\
         }}\n\
         float fbm(float3 p) {{\n\
         \x20   float3 q = p; float total = 0.0; float amp = 1.0; float weight = 0.0;\n\
         \x20   for (int i = 0; i < 3; i++) {{\n\
         \x20       total += perlin3(q) * amp; weight += amp;\n\
         \x20       q = rotateOctave(q) * 2.02 + float3(3.7, 1.9, 6.3);\n\
         \x20       amp *= 0.48;\n\
         \x20   }}\n\
         \x20   return total / max(weight, 0.0001);\n\
         }}\n\
         float3 warpVector(float3 p) {{\n\
         \x20   return float3(perlin3(p), perlin3(p + float3(5.2, 1.3, 2.8)),\n\
         \x20                 perlin3(p + float3(1.7, 9.2, 4.4)));\n\
         }}\n\
         float heightField(float3 dir) {{\n\
         \x20   float frequency = mix(1.05, 3.4, clamp(DETAIL / 5.0, 0.0, 1.0));\n\
         \x20   float3 p = dir * frequency + float3(1.7, 3.1, 5.3) + u_mot.xyz;\n\
         \x20   float3 warp = warpVector(p * 0.55 + u_wmot.xyz * 0.42 + float3(0.7, -1.1, 0.4)) * WARP;\n\
         \x20   return fbm(p + warp);\n\
         }}\n\
         float3 rotateAxis(float3 p, float3 axis, float angle) {{\n\
         \x20   float c = cos(angle); float s = sin(angle);\n\
         \x20   return p * c + cross(axis, p) * s + axis * dot(axis, p) * (1.0 - c);\n\
         }}\n\
         float torsion(float3 dir) {{\n\
         \x20   float axial = clamp(dot(dir, normalize(float3(-0.68, 0.54, 0.49))), -1.0, 1.0);\n\
         \x20   return axial * (1.5 - 0.5 * axial * axial) * TWIST;\n\
         }}\n\
         float3 linearToSrgb(float3 c) {{\n\
         \x20   float3 v = max(c, float3(0.0));\n\
         \x20   return mix(v * 12.92, 1.055 * pow(v, float3(1.0 / 2.4)) - 0.055, step(float3(0.0031308), v));\n\
         }}\n\
         float3 oklabToLinear(float3 c) {{\n\
         \x20   float lc = c.x + 0.3963377774 * c.y + 0.2158037573 * c.z;\n\
         \x20   float mc = c.x - 0.1055613458 * c.y - 0.0638541728 * c.z;\n\
         \x20   float sc = c.x - 0.0894841775 * c.y - 1.2914855480 * c.z;\n\
         \x20   float l = lc * lc * lc; float m = mc * mc * mc; float s = sc * sc * sc;\n\
         \x20   return float3(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,\n\
         \x20                 -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,\n\
         \x20                 -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s);\n\
         }}\n\
         // The gradient's stops, even spaced and already in OKLab, blended with a smoothstep.\n\
         float3 gradientAt(float t) {{\n\
         \x20   float x = clamp(t, 0.0, 1.0) * {last}.0;\n\
         \x20   float3 a; float3 b; float f;\n\
         {segs}\
         \x20   f = f * f * (3.0 - 2.0 * f);\n\
         \x20   return linearToSrgb(oklabToLinear(mix(a, b, f)));\n\
         }}\n\
         float spread(float raw) {{ return clamp((raw - 0.5) * 3.0 + 0.5, 0.0, 1.0); }}\n\
         \n\
         half4 main(float2 xy) {{\n\
         \x20   float calm = u_tc.y;\n\
         \x20   float aspect = u_res.x / u_res.y;\n\
         \x20   float2 uv = xy / u_res;\n\
         \x20   // The pixel's ray, in the camera's frame: origin, looking down -z at a sphere\n\
         \x20   // three units away. Focal length and scale are the cover-zoom's.\n\
         \x20   float2 ndc = float2(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);\n\
         \x20   float3 rd = normalize(float3(ndc.x * aspect / u_cam.x, ndc.y / u_cam.x, -1.0));\n\
         \x20   float3 sc = float3(0.0, 0.0, -3.0);\n\
         \x20   float scale = u_cam.y;\n\
         \x20   float disp = 0.30 * INTENSITY;\n\
         \x20   float3 twistAxis = normalize(float3(-0.68, 0.54, 0.49));\n\
         \x20   float radius = scale;\n\
         \x20   float h = 0.0; bool seen = false;\n\
         \x20   // Two passes: the plain sphere's hit names a height, the sphere re-sized by it\n\
         \x20   // is hit again — the displaced surface, near enough, without the mesh.\n\
         \x20   for (int i = 0; i < 2; i++) {{\n\
         \x20       if (float(i) >= u_cam.z) {{ break; }}\n\
         \x20       float mid = dot(rd, sc);\n\
         \x20       float disc = mid * mid - dot(sc, sc) + radius * radius;\n\
         \x20       if (disc < 0.0) {{ break; }}\n\
         \x20       float3 obj = (rd * (mid - sqrt(disc)) - sc) / scale;\n\
         \x20       float3 base = float3(dot(u_rot0.xyz, obj), dot(u_rot1.xyz, obj), dot(u_rot2.xyz, obj));\n\
         \x20       float3 dir = normalize(base);\n\
         \x20       dir = normalize(rotateAxis(base, twistAxis, -torsion(dir)));\n\
         \x20       h = heightField(dir);\n\
         \x20       seen = true;\n\
         \x20       radius = scale * max(1.0 + h * disp, 0.72);\n\
         \x20   }}\n\
         \x20   float3 col;\n\
         \x20   if (seen) {{\n\
         \x20       col = gradientAt(spread(h * 0.5 + 0.5));\n\
         \x20   }} else {{\n\
         \x20       // Past the sphere: the same gradient along a fixed diagonal.\n\
         \x20       float2 axis = normalize(float2(0.62 * aspect, 0.78));\n\
         \x20       float2 centered = float2((uv.x - 0.5) * aspect, uv.y - 0.5);\n\
         \x20       float extent = abs(axis.x) * aspect * 0.5 + abs(axis.y) * 0.5;\n\
         \x20       col = gradientAt(dot(centered, axis) / max(extent * 2.0, 0.0001) + 0.5);\n\
         \x20   }}\n\
         \n\
         \x20   // Calm: flatten the field toward its own ground — the pools dim and the\n\
         \x20   // corners lift, so a form screen keeps real colour under its glass rows while\n\
         \x20   // losing the launcher's contrast. Motion is untouched (see the doc comment).\n\
         \x20   col = mix(col, col * 0.60 + u_lift.rgb, calm);\n\
         \n\
         \x20   // Elliptical vignette: clear at r=0.25 → black·0.42 at r=1.15 (aspect-fit ellipse).\n\
         \x20   // Halved under calm: a launcher's cards sit in the pooled centre, but a form\n\
         \x20   // screen's rows run out toward the edges, where crushing to black just eats them.\n\
         \x20   float2 e = (uv - 0.5) * 2.0;\n\
         \x20   float vig = clamp((length(e) - 0.25) / 0.90, 0.0, 1.0)\n\
         \x20             * mix(0.42, 0.21, calm) * u_scrim.a;\n\
         \x20   col = mix(col, u_scrim.rgb, vig);\n\
         \n\
         \x20   // Vertical legibility scrim: black 0.38/0.06/0.08/0.40 at 0/0.32/0.68/1.\n\
         \x20   float v = uv.y;\n\
         \x20   float s = v < 0.32 ? mix(0.38, 0.06, v / 0.32)\n\
         \x20           : v < 0.68 ? mix(0.06, 0.08, (v - 0.32) / 0.36)\n\
         \x20           : mix(0.08, 0.40, (v - 0.68) / 0.32);\n\
         \x20   col = mix(col, u_scrim.rgb, s * u_scrim.a);\n\
         \n\
         \x20   return half4(half3(col), 1.0);\n\
         }}\n",
        last = n - 1,
    )
}
// --- The shared binary↔overlay model ------------------------------------------------------

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LibraryPhase {
    Loading,
    Error {
        title: String,
        body: String,
        can_retry: bool,
    },
    Empty,
    Ready,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LibraryGame {
    pub id: String,
    pub title: String,
    pub store: String,
    /// Opens the launcher itself, not a title. Host `role`, reduced by [`pf_client_core::library::GameEntry::is_launcher`].
    pub launcher: bool,
    /// Brand mark token, already validated by [`pf_client_core::library::GameEntry::icon_token`]. Empty or unknown draws nothing.
    pub icon: String,
    /// Host free-form display string (`"PC"`, `"PS2"`, …). `None` until [`crate::collate`] assigns a bucket.
    pub platform: Option<String>,
    /// What the launch hold says about a title beyond its name. The shelf draws none of it —
    /// a tile is a poster — so all three default, and a host or a bridge that says nothing
    /// leaves the hold with the title and the store, which is what it showed before.
    #[serde(default)]
    pub developer: Option<String>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Host play stats, for the Recent and Most played sorts.
    #[serde(default)]
    pub stats: Option<pf_client_core::library::GameStats>,
    /// Already up on the host — pick resumes. From `/api/v1/status` via [`LibraryShared::set_running`].
    ///
    /// Host state, not catalog state: not on `GameEntry`, not persisted. A disk shelf cannot
    /// claim a title is running because it was last time. `false` on older hosts and while
    /// `/status` is in flight; the badge may appear a frame late rather than hold the catalog.
    pub running: bool,
}

impl LibraryGame {
    /// In the shelf's leading band: the desktop tile, then the launchers — the tiles that
    /// open something rather than play a title. [`GridShape`]'s split is this run's length,
    /// so cursor math, the section heading and the renderer must all ask here.
    pub fn leads(&self) -> bool {
        self.launcher || self.id == DESKTOP_ID
    }
}

/// The console sorts its own reduced model; the policy is `pf_client_core::collate`.
impl pf_client_core::collate::Collatable for LibraryGame {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn store(&self) -> &str {
        &self.store
    }
    fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }
    fn is_launcher(&self) -> bool {
        self.launcher
    }
    fn last_played_ms(&self) -> u64 {
        self.stats.map_or(0, |s| s.last_played_unix_ms)
    }
    fn play_time_ms(&self) -> u64 {
        self.stats.map_or(0, |s| s.play_time_ms)
    }
}

/// Observation vs memory, and whether a memory is still being fetched.
///
/// Three states because Waking and Offline need different shelf copy. A boolean would
/// say "waking" while nothing is happening.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Stale {
    No,
    /// Served from the disk cache while the host is being woken and re-asked.
    Waking,
    /// Disk cache; the host never answered. Not an error: these are still the titles to pick from.
    Offline,
}

impl Stale {
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Stale::No => None,
            Stale::Waking => Some("Last known library \u{2014} waking the host\u{2026}"),
            Stale::Offline => Some("Last known library \u{2014} the host didn't answer"),
        }
    }
}

struct Shared {
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Disk cache vs live host. Live [`LibraryShared::set_games`] resets this to [`Stale::No`].
    stale: Stale,
    /// Every poster fetched for this epoch, encoded, by title id. Kept for the fetch, not
    /// queued: whichever screen is up decodes what it lacks, and a cover a screen evicted
    /// or never drew comes back from here. Cleared by [`LibraryShared::begin_fetch`].
    art: HashMap<String, Arc<[u8]>>,
    /// Posters a host decoded on its own thread, waiting to be adopted. Separate from
    /// [`Shared::art`] because taking one costs nothing: the work is already done.
    decoded_in: VecDeque<(String, DecodedPoster)>,
    /// The scale the shelf caches art at, published for hosts that decode off-thread so they
    /// size it the way this crate would. `None` until a shelf has drawn once.
    art_scale: Option<f64>,
    /// Bumped on phase/games changes so the renderer re-syncs its snapshot.
    generation: u64,
    /// Bumped once per fetch, by [`LibraryShared::begin_fetch`].
    ///
    /// Distinguishes "this shelf's list" from "the previous host's, still here" without
    /// catching `Loading`. A warm cache publishes `Ready` inside one 60 Hz frame, so a
    /// phase edge can be missed; a counter cannot.
    fetch_epoch: u64,
    /// Each launched title's host-side state string (`launching`, `running`, …) and whether a
    /// `window` is still to come, by library id — the launch hold's answer. Replaced whole on
    /// every `/status` read.
    states: std::collections::HashMap<String, (String, bool)>,
    /// Bumped on every `/status` read, changed or not: the launch hold paces its next poll
    /// on an answer landing, not on the answer being different.
    status_gen: u64,
}

pub(crate) struct LibrarySnapshot {
    pub phase: LibraryPhase,
    pub games: Vec<LibraryGame>,
    pub stale: Stale,
    pub generation: u64,
}

/// One poster, decoded and ready to draw, on its way from a worker thread to the shell.
///
/// Skia's handles are only conditionally `Send` — safe to move while nothing else holds a
/// reference, which a freshly decoded image satisfies. [`crate::decode_poster_off_thread`] is
/// the only way to make one, so that condition is checked once, there, rather than trusted.
pub struct DecodedPoster(skia_safe::Sendable<Image>);

/// Decode a poster on a thread that is not drawing — see
/// [`crate::screens::library::decode_poster_off_thread`], which this forwards to so the sizing
/// policy stays with the screen that owns the cache.
pub fn decode_poster_off_thread(bytes: &[u8], k: f64) -> Option<DecodedPoster> {
    crate::screens::library::decode_poster_off_thread(bytes, k)
}

impl DecodedPoster {
    pub(crate) fn new(image: Image) -> Option<Self> {
        image.wrap_send().ok().map(DecodedPoster)
    }

    pub(crate) fn into_image(self) -> Image {
        self.0.into_inner()
    }
}

impl std::fmt::Debug for DecodedPoster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecodedPoster")
    }
}

/// Binary write handle / overlay read handle. Fetch threads push; the renderer drains per frame.
#[derive(Clone)]
pub struct LibraryShared(Arc<Mutex<Shared>>);

impl Default for LibraryShared {
    fn default() -> Self {
        LibraryShared(Arc::new(Mutex::new(Shared {
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            stale: Stale::No,
            art: HashMap::new(),
            decoded_in: VecDeque::new(),
            art_scale: None,
            generation: 0,
            fetch_epoch: 0,
            states: std::collections::HashMap::new(),
            status_gen: 0,
        })))
    }
}

impl LibraryShared {
    /// A fetch is starting: `Loading`, and the epoch advances.
    ///
    /// Must go through here, not `set_phase(Loading)`. A cache that answers before the next
    /// frame would otherwise leave the epoch unchanged and the shelf on the previous host.
    pub fn begin_fetch(&self) {
        let mut s = self.0.lock().unwrap();
        s.phase = LibraryPhase::Loading;
        // Previous host's stale note is not this fetch's; a cached render re-declares it.
        s.stale = Stale::No;
        // The previous host's posters are not this fetch's.
        s.art.clear();
        s.decoded_in.clear();
        s.fetch_epoch += 1;
        s.generation += 1;
    }

    /// Fetch the model is on. A shelf records this at push; a later difference means a new fetch
    /// owns the list. `pub` because the worker that does the fetching lives in the shell crate.
    pub fn fetch_epoch(&self) -> u64 {
        self.0.lock().unwrap().fetch_epoch
    }

    pub fn set_phase(&self, phase: LibraryPhase) {
        let mut s = self.0.lock().unwrap();
        s.phase = phase;
        s.generation += 1;
    }

    /// Host titles → carousel (empty = empty scene). Clears cached staleness.
    ///
    /// Launchers move to the front, host title order kept within each group. Grouping here
    /// so cursor math, the art pump, and [`GridShape`] all see the same prefix.
    pub fn set_games(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::No);
    }

    /// Disk-cache catalog while the host is still being asked. Live fetch stays in flight.
    pub fn set_games_cached(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::Waking);
    }

    /// Shelf copy about a cached catalog, catalog unchanged. No-op on a live shelf, so a late
    /// abandoned-fetch give-up cannot mark a fresh library stale.
    pub fn set_stale(&self, stale: Stale) {
        let mut s = self.0.lock().unwrap();
        if s.stale == stale || s.stale == Stale::No {
            return;
        }
        s.stale = stale;
        s.generation += 1;
    }

    fn put_games(&self, mut games: Vec<LibraryGame>, stale: Stale) {
        // Empty is the CATALOG's verdict, taken before the desktop tile joins: a host with
        // no plugins keeps its empty copy, and gets the tile beside it.
        let empty = games.is_empty();
        games.insert(0, desktop_tile());
        order(&mut games);
        let mut s = self.0.lock().unwrap();
        s.phase = if empty {
            LibraryPhase::Empty
        } else {
            LibraryPhase::Ready
        };
        s.games = games;
        s.stale = stale;
        s.generation += 1;
    }

    /// The host's `/status` `games[]`, read after the catalog: which titles are up (the
    /// Resume badge, re-ordered) and each one's state (the launch hold). An empty list
    /// clears every badge (older or unreachable host).
    ///
    /// The badge side is a no-op — no generation bump — when nothing changed. This is polled.
    pub fn set_running(&self, games: &[pf_client_core::library::RunningGame]) {
        let mut s = self.0.lock().unwrap();
        s.status_gen += 1;
        s.states = games
            .iter()
            .filter_map(|g| Some((g.app_id.clone()?, (g.state.clone(), g.awaiting_window))))
            .collect();
        let mut changed = false;
        for g in &mut s.games {
            let now = games
                .iter()
                .any(|r| r.is_up() && r.app_id.as_deref() == Some(g.id.as_str()));
            if g.running != now {
                g.running = now;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let mut games = std::mem::take(&mut s.games);
        order(&mut games);
        s.games = games;
        s.generation += 1;
    }

    /// The host's state string for one launched title, and whether it will report `window`
    /// next, from the last `/status` read; `None` when the host lists nothing for it (no lease
    /// yet, or the launch never resolved).
    pub(crate) fn launch_state(&self, id: &str) -> Option<(String, bool)> {
        self.0.lock().unwrap().states.get(id).cloned()
    }

    /// How many `/status` reads have landed — see `status_gen`.
    pub(crate) fn status_gen(&self) -> u64 {
        self.0.lock().unwrap().status_gen
    }

    pub fn push_art(&self, id: String, bytes: Vec<u8>) {
        self.0.lock().unwrap().art.insert(id, bytes.into());
    }

    /// A poster a host already decoded, off the thread that draws.
    ///
    /// The reason this exists: on a 2020 TV one full-size PNG cover costs ~90 ms, which is five
    /// frames. Decoded here it is a move and a hash insert, so a shelf fills without the frame
    /// loop stopping for each cover. [`Self::push_art`] stays for hosts with nowhere else to
    /// decode — they get the old behaviour, budgeted per frame.
    pub fn push_decoded(&self, id: String, poster: DecodedPoster) {
        self.0.lock().unwrap().decoded_in.push_back((id, poster));
    }

    /// The scale a host should decode at, once a shelf has published one. `None` before that —
    /// a host with nothing to go on should push encoded bytes and let the shelf size them.
    #[must_use]
    pub fn art_scale(&self) -> Option<f64> {
        self.0.lock().unwrap().art_scale
    }

    pub(crate) fn set_art_scale(&self, k: f64) {
        self.0.lock().unwrap().art_scale = Some(k);
    }

    /// Every poster decoded since the last call. Unbounded on purpose: adopting one is a move,
    /// so there is no frame budget to spend and holding them back only delays the picture.
    pub(crate) fn drain_decoded(&self) -> Vec<(String, DecodedPoster)> {
        self.0.lock().unwrap().decoded_in.drain(..).collect()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub(crate) fn snapshot(&self) -> LibrarySnapshot {
        let s = self.0.lock().unwrap();
        LibrarySnapshot {
            phase: s.phase.clone(),
            games: s.games.clone(),
            stale: s.stale,
            generation: s.generation,
        }
    }

    /// The first `max` of `ids` that have bytes, in that order. Nothing is consumed: a screen
    /// asks for what it lacks, on-screen titles first, and decodes at its own pace.
    pub(crate) fn art_for<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
        max: usize,
    ) -> Vec<(String, Arc<[u8]>)> {
        let s = self.0.lock().unwrap();
        ids.into_iter()
            .filter_map(|id| s.art.get(id).map(|b| (id.to_string(), b.clone())))
            .take(max)
            .collect()
    }
}

/// Shelf display order, the one path every writer uses.
///
/// Launchers first (the [`GridShape`] prefix). Running titles lead within their group.
/// `sort_by_key` is stable, so host title order survives inside each band.
fn order(games: &mut [LibraryGame]) {
    games.sort_by_key(|g| (g.id != DESKTOP_ID, !g.launcher, !g.running));
}

/// The desktop tile's id and mark, and the store→label table. All live in `pf-client-core`
/// now, so the GTK and WinUI dialogs read the same ones; re-exported because the screens name
/// them through this module.
pub use pf_client_core::library::{store_label, DESKTOP_ICON, DESKTOP_ID};

/// Every shelf leads with the host's own desktop, so Library is never a dead end for the
/// desktop-only user and a plugin-less host is still one press from streaming. Model state
/// only: the disk cache stores wire `GameEntry`s and never sees this.
fn desktop_tile() -> LibraryGame {
    LibraryGame {
        id: DESKTOP_ID.into(),
        title: "Desktop".into(),
        store: String::new(),
        launcher: false,
        icon: DESKTOP_ICON.into(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
    }
}

pub fn initials(title: &str) -> String {
    title
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

/// One band of the Games tab, by the id `Settings::library_sections` stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Desktops,
    Recent,
    Favorites,
    Launchers,
    /// One tile per platform or store group.
    Collections,
    Games,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Desktops,
        Section::Recent,
        Section::Favorites,
        Section::Launchers,
        Section::Collections,
        Section::Games,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Section::Desktops => "desktops",
            Section::Recent => "recent",
            Section::Favorites => "favorites",
            Section::Launchers => "launchers",
            Section::Collections => "collections",
            Section::Games => "games",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Desktops => "Desktops",
            Section::Recent => "Recently played",
            Section::Favorites => "Favorites",
            Section::Launchers => "Launchers",
            Section::Collections => "Collections",
            Section::Games => "Games",
        }
    }
}

/// `library_sections` as the Apple app parses it: ids in order, `-` before one switched
/// off, an unknown id dropped, a repeat keeping its first place, a known section the value
/// lacks appended switched on.
pub fn sections(stored: &str) -> Vec<(Section, bool)> {
    let mut out: Vec<(Section, bool)> = Vec::new();
    for token in stored.split(',').map(str::trim) {
        let (on, id) = match token.strip_prefix('-') {
            Some(id) => (false, id),
            None => (true, token),
        };
        let Some(s) = Section::ALL.into_iter().find(|s| s.id() == id) else {
            continue;
        };
        if !out.iter().any(|(seen, _)| *seen == s) {
            out.push((s, on));
        }
    }
    for s in Section::ALL {
        if !out.iter().any(|(seen, _)| *seen == s) {
            out.push((s, true));
        }
    }
    out
}

/// The stored form of `sections`.
pub fn stored_sections(sections: &[(Section, bool)]) -> String {
    sections
        .iter()
        .map(|(s, on)| format!("{}{}", if *on { "" } else { "-" }, s.id()))
        .collect::<Vec<_>>()
        .join(",")
}

/// `ms` ago, as a person says it: `just now`, `12 min ago`, `2 h ago`, `3 d ago`, `5 mo ago`.
pub fn ago(ms: u64) -> String {
    let min = ms / 60_000;
    match min {
        0 => "just now".into(),
        1..60 => format!("{min} min ago"),
        60..1_440 => format!("{} h ago", min / 60),
        1_440..43_200 => format!("{} d ago", min / 1_440),
        _ => format!("{} mo ago", min / 43_200),
    }
}

/// The Details card's play line: `Last played 2 h ago · 14 h total · 12 launches`. `None`
/// for a title never played here.
pub fn stats_line(s: &pf_client_core::library::GameStats, now_ms: u64) -> Option<String> {
    if s.last_played_unix_ms == 0 {
        return None;
    }
    let mut parts = vec![format!(
        "Last played {}",
        ago(now_ms.saturating_sub(s.last_played_unix_ms))
    )];
    let hours = s.play_time_ms / 3_600_000;
    if hours > 0 {
        parts.push(format!("{hours} h total"));
    }
    if s.launch_count > 0 {
        parts.push(format!(
            "{} launch{}",
            s.launch_count,
            if s.launch_count == 1 { "" } else { "es" }
        ));
    }
    Some(parts.join(" \u{b7} "))
}

/// Where a host's favorites live in the settings document's `extra` map.
fn favorites_key(fp_hex: &str) -> String {
    format!("favorites.{fp_hex}")
}

/// The titles marked favorite on host `fp_hex` from this device, in marking order.
pub fn favorites(settings: &pf_client_core::trust::Settings, fp_hex: &str) -> Vec<String> {
    settings
        .extra
        .get(&favorites_key(fp_hex))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Mark or unmark `id` on host `fp_hex`; `true` when it is now a favorite.
pub fn toggle_favorite(
    settings: &mut pf_client_core::trust::Settings,
    fp_hex: &str,
    id: &str,
) -> bool {
    let mut list = favorites(settings, fp_hex);
    let on = match list.iter().position(|f| f == id) {
        Some(i) => {
            list.remove(i);
            false
        }
        None => {
            list.push(id.to_string());
            true
        }
    };
    let key = favorites_key(fp_hex);
    if list.is_empty() {
        settings.extra.remove(&key);
    } else {
        settings.extra.insert(key, list.into());
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_parse_as_the_apple_app_does() {
        use Section::*;
        assert_eq!(
            sections(""),
            Section::ALL.map(|s| (s, true)).to_vec(),
            "empty is every section, on"
        );
        assert_eq!(
            sections("games,-recent,bogus,games,desktops"),
            vec![
                (Games, true),
                (Recent, false),
                (Desktops, true),
                (Favorites, true),
                (Launchers, true),
                (Collections, true),
            ]
        );
        let s = sections("desktops,recent,-favorites,launchers,-collections,games");
        assert_eq!(
            stored_sections(&s),
            "desktops,recent,-favorites,launchers,-collections,games"
        );
    }

    #[test]
    fn the_play_line_reads_like_a_person_says_it() {
        let s = pf_client_core::library::GameStats {
            last_played_unix_ms: 1_000,
            play_time_ms: 14 * 3_600_000,
            last_run_ms: 0,
            launch_count: 12,
        };
        assert_eq!(
            stats_line(&s, 1_000 + 2 * 3_600_000).as_deref(),
            Some("Last played 2 h ago \u{b7} 14 h total \u{b7} 12 launches")
        );
        assert_eq!(ago(30_000), "just now");
        assert_eq!(ago(3 * 86_400_000), "3 d ago");
        let never = pf_client_core::library::GameStats {
            last_played_unix_ms: 0,
            ..s
        };
        assert_eq!(stats_line(&never, 5), None);
    }

    #[test]
    fn favorites_toggle_per_host_and_drop_an_empty_list() {
        let mut settings = pf_client_core::trust::Settings::default();
        assert!(toggle_favorite(&mut settings, "aa", "steam:1"));
        assert!(toggle_favorite(&mut settings, "aa", "steam:2"));
        assert!(favorites(&settings, "bb").is_empty(), "per host");
        assert_eq!(favorites(&settings, "aa"), ["steam:1", "steam:2"]);
        assert!(!toggle_favorite(&mut settings, "aa", "steam:1"));
        assert!(!toggle_favorite(&mut settings, "aa", "steam:2"));
        assert!(settings.extra.is_empty(), "no empty list left behind");
    }

    /// Poster bytes stay for the fetch: any screen takes what it lacks, in the order it asks,
    /// as often as it asks; the next fetch starts clean.
    #[test]
    fn art_stays_for_the_fetch_and_serves_in_asked_order() {
        let shared = LibraryShared::default();
        for i in 0..6 {
            shared.push_art(format!("g{i}"), vec![i as u8]);
        }
        let ids =
            |got: Vec<(String, Arc<[u8]>)>| got.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            ids(shared.art_for(["g4", "g1", "g9"], 8)),
            ["g4", "g1"],
            "an id that never arrived is not an error"
        );
        assert_eq!(
            ids(shared.art_for(["g4", "g1"], 8)),
            ["g4", "g1"],
            "not consumed"
        );
        assert_eq!(shared.art_for(["g0", "g1", "g2"], 2).len(), 2, "bounded");
        shared.begin_fetch();
        assert!(
            shared.art_for(["g0", "g1"], 9).is_empty(),
            "a new fetch starts clean"
        );
    }

    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 5, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(4, 5, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(2, 5, 1, false), StepResult::Moved(3));
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
    }

    /// Launcher-less, prefix, partial launcher row, degenerate two-cell, and single-column fields.
    const SHAPES: [(usize, usize, usize); 9] = [
        (11, 4, 0),
        (40, 5, 0),
        (30, 7, 2),
        (20, 4, 6),
        (4, 4, 2),
        (9, 3, 3),
        (7, 3, 7),
        (1, 3, 1),
        (13, 1, 2),
    ];

    /// Rows tile `0..len` once: `cell_of` and `row_start` agree; no index in two rows or in none.
    #[test]
    fn grid_rows_tile_the_field_exactly_once() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            let mut next = 0usize;
            for row in 0..s.rows() {
                let n = s.row_len(row);
                assert!((1..=cols).contains(&n), "{s:?} row {row} holds {n} cells");
                for col in 0..n {
                    let i = s.row_start(row) + col;
                    assert_eq!(i, next, "{s:?} row {row} does not follow the one above");
                    assert_eq!(s.cell_of(i), (row, col), "{s:?} disagrees about index {i}");
                    next += 1;
                }
            }
            assert_eq!(next, len, "{s:?} left cells in no row at all");
        }
    }

    /// Horizontal step refuses at the row's true end, not at `cols` (partial rows, launcher prefix).
    #[test]
    fn grid_horizontal_moves_walk_the_row_and_refuse_its_true_ends() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, col) = s.cell_of(i);
                let want = |first: bool, to: i32| {
                    if first {
                        StepResult::Boundary
                    } else {
                        StepResult::Moved(to)
                    }
                };
                let i = i as i32;
                // Hint is the column you would return to, not the one a horizontal step walks out of.
                for hint in 0..cols {
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Left),
                        want(col == 0, i - 1),
                        "{s:?} Left from {i}"
                    );
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Right),
                        want(col + 1 == s.row_len(row), i + 1),
                        "{s:?} Right from {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn grid_vertical_moves_change_row_and_carry_the_column() {
        const VERTICAL: [(GridDir, i32); 4] = [
            (GridDir::Up, -1),
            (GridDir::Down, 1),
            (GridDir::PageBack, -GRID_PAGE_ROWS),
            (GridDir::PageForward, GRID_PAGE_ROWS),
        ];
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, _) = s.cell_of(i);
                for hint in 0..cols {
                    for (dir, d) in VERTICAL {
                        let want_row = (row as i32 + d).clamp(0, s.rows() as i32 - 1) as usize;
                        let what = format!("{s:?} {dir:?} from {i} with hint {hint}");
                        match grid_step(i as i32, s, hint, dir) {
                            StepResult::Moved(to) => {
                                let (r, c) = s.cell_of(to as usize);
                                assert_eq!(r, want_row, "{what} landed in row {r}");
                                if r != row {
                                    assert_eq!(c, hint.min(s.row_len(r) - 1), "{what} column");
                                }
                            }
                            // Field edges refuse. A page already at the edge travels the current row.
                            StepResult::Boundary => assert_eq!(want_row, row, "{what} refused"),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_row_is_reachable_by_stepping() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                for (dir, end) in [(GridDir::Up, 0), (GridDir::Down, s.rows() - 1)] {
                    let mut cursor = i as i32;
                    for _ in 0..=s.rows() {
                        match grid_step(cursor, s, 0, dir) {
                            StepResult::Moved(to) => cursor = to,
                            StepResult::Boundary => break,
                        }
                    }
                    let (row, _) = s.cell_of(cursor as usize);
                    assert_eq!(row, end, "{s:?} {dir:?} from {i} stalled in row {row}");
                }
            }
        }
    }

    /// Two launchers on seven columns: alone on row 0, games restart at column 0.
    #[test]
    fn the_launcher_row_sits_squarely_above_the_games() {
        let s = GridShape::new(30, 7, 2);
        assert_eq!(s.rows(), 5);
        assert_eq!((s.row_len(0), s.row_len(1)), (2, 7));
        // Down from a launcher lands on the cover under it, not five columns right.
        assert_eq!(grid_step(0, s, 0, GridDir::Down), StepResult::Moved(2));
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(3));
        // Up out of the games band leaves it, rather than sliding along it.
        assert_eq!(grid_step(2, s, 0, GridDir::Up), StepResult::Moved(0));
        assert_eq!(grid_step(3, s, 1, GridDir::Up), StepResult::Moved(1));
        assert_eq!(grid_step(6, s, 4, GridDir::Up), StepResult::Moved(1));
        // Games row true ends are 2 and 8 — 6 and 7 are mid-row.
        assert_eq!(grid_step(6, s, 4, GridDir::Right), StepResult::Moved(7));
        assert_eq!(grid_step(7, s, 5, GridDir::Left), StepResult::Moved(6));
        assert_eq!(grid_step(8, s, 6, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 0, GridDir::Left), StepResult::Boundary);
    }

    #[test]
    fn a_crossing_returns_to_the_column_it_started_from() {
        use GridDir::{Down, Right, Up};
        let s = GridShape::new(30, 7, 2);
        // Mirrors `LibraryScreen::grid_move`: step, then `grid_col_hint`.
        let walk = |start: i32, dirs: &[GridDir]| {
            let (mut cursor, mut hint) = (start, s.cell_of(start.max(0) as usize).1);
            for &dir in dirs {
                if let StepResult::Moved(to) = grid_step(cursor, s, hint, dir) {
                    hint = grid_col_hint(s, hint, dir, to);
                    cursor = to;
                }
            }
            cursor
        };
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right]), 6);
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up]), 1);
        // A vertical move never spends the hint.
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up, Down]), 6);
        assert_eq!(
            walk(0, &[Down, Right, Right, Right, Right, Up, Down, Up, Down]),
            6
        );
    }

    #[test]
    fn grid_pages_by_rows_and_lands_on_the_ends() {
        let s = GridShape::new(40, 5, 0);
        assert_eq!(
            grid_step(0, s, 0, GridDir::PageForward),
            StepResult::Moved(15)
        );
        // Page past the end lands on the end (clamped `step_cursor`), it does not refuse.
        assert_eq!(
            grid_step(35, s, 0, GridDir::PageForward),
            StepResult::Moved(39)
        );
        assert_eq!(
            grid_step(39, s, 4, GridDir::PageForward),
            StepResult::Boundary
        );
        assert_eq!(grid_step(3, s, 3, GridDir::PageBack), StepResult::Moved(0));
        assert_eq!(grid_step(0, s, 0, GridDir::PageBack), StepResult::Boundary);
    }

    #[test]
    fn grid_rows_refuse_at_their_ends_but_the_tail_row_clamps() {
        // 11 items, 4 columns: rows of 4, 4, 3.
        let s = GridShape::new(11, 4, 0);
        assert_eq!(grid_step(1, s, 1, GridDir::Right), StepResult::Moved(2));
        assert_eq!(grid_step(2, s, 2, GridDir::Left), StepResult::Moved(1));
        // At a row's ends: refused, not wrapped onto the neighbouring row.
        assert_eq!(grid_step(3, s, 3, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(4, s, 0, GridDir::Left), StepResult::Boundary);
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(5));
        // Down into the short tail row clamps to the last item — column 3 does not exist there.
        assert_eq!(grid_step(7, s, 3, GridDir::Down), StepResult::Moved(10));
        assert_eq!(grid_step(10, s, 3, GridDir::Down), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 2, GridDir::Up), StepResult::Boundary);
        assert_eq!(grid_step(6, s, 2, GridDir::Up), StepResult::Moved(2));
    }

    /// Persisted view name is a file format. Unknown → shelf.
    #[test]
    fn library_view_parses_leniently() {
        assert_eq!(LibraryView::parse("grid"), LibraryView::Grid);
        assert_eq!(LibraryView::parse("shelf"), LibraryView::Shelf);
        assert_eq!(LibraryView::parse("coverwall"), LibraryView::Grid);
        assert_eq!(LibraryView::parse(""), LibraryView::Grid);
        assert_eq!(LibraryView::default(), LibraryView::Grid);
        for v in LibraryView::ALL {
            assert_eq!(LibraryView::parse(v.id()), v, "{} round-trips", v.label());
        }
    }

    #[test]
    fn grid_step_is_safe_on_a_degenerate_grid() {
        let empty = GridShape::new(0, 4, 0);
        assert_eq!(grid_step(0, empty, 0, GridDir::Right), StepResult::Boundary);
        let colless = GridShape::new(5, 0, 0);
        assert_eq!(
            grid_step(0, colless, 0, GridDir::Right),
            StepResult::Boundary
        );
        // One column: left/right are always refused, up/down still walk.
        let thin = GridShape::new(5, 1, 0);
        assert_eq!(grid_step(1, thin, 0, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(1, thin, 0, GridDir::Down), StepResult::Moved(2));
        // A cursor the library outgrew reads as the nearest real cell; the next press heals it.
        let s = GridShape::new(6, 3, 2);
        assert_eq!(grid_step(99, s, 0, GridDir::Up), StepResult::Moved(2));
        assert_eq!(grid_step(-4, s, 0, GridDir::Right), StepResult::Moved(1));
    }

    /// Launchers lead; host title order survives within each group. `launcher_count()` is prefix `0..n`.
    #[test]
    fn set_games_groups_launchers_first_and_keeps_title_order() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
        ]);
        let snap = shared.snapshot();
        assert!(matches!(snap.phase, LibraryPhase::Ready));
        assert_eq!(snap.stale, Stale::No, "a live fetch is not a memory");
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Desktop", "Big Picture", "Heroic", "Celeste", "Portal 2"]
        );
        assert_eq!(
            snap.games.iter().take_while(|g| g.leads()).count(),
            3,
            "the lead band is the desktop tile plus both launchers"
        );
    }

    /// Running titles lead within their group; the launcher prefix [`GridShape`] counts stays intact.
    #[test]
    fn running_titles_lead_without_breaking_the_launcher_prefix() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
            g("Tunic", false),
        ]);
        let up = running(&["steam:Portal 2", "steam:Heroic"], "running");
        shared.set_running(&up);
        let snap = shared.snapshot();
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Desktop",
                "Heroic",
                "Big Picture",
                "Portal 2",
                "Celeste",
                "Tunic"
            ],
            "running first inside each group; the lead band is still the prefix"
        );
        assert_eq!(
            snap.games.iter().take_while(|g| g.leads()).count(),
            3,
            "the lead band GridShape depends on is intact"
        );
        assert!(snap.games[1].running && snap.games[3].running);

        // Same set: no generation bump. This is polled.
        let gen_before = snap.generation;
        shared.set_running(&up);
        assert_eq!(shared.snapshot().generation, gen_before);

        shared.set_running(&[]);
        let after = shared.snapshot();
        assert!(after.games.iter().all(|g| !g.running));
        assert!(after.generation > gen_before, "a real change does re-sync");
    }

    fn running(ids: &[&str], state: &str) -> Vec<pf_client_core::library::RunningGame> {
        ids.iter()
            .map(|id| pf_client_core::library::RunningGame {
                app_id: Some((*id).to_string()),
                title: String::new(),
                state: state.to_string(),
                awaiting_window: false,
            })
            .collect()
    }

    /// The launch hold reads each title's state, and paces on reads landing — so a read
    /// that changes no badge still counts, and `launching` is up for the badge but not
    /// running for the hold.
    #[test]
    fn status_reads_keep_state_and_count_even_when_nothing_changed() {
        let shared = LibraryShared::default();
        shared.set_games(vec![LibraryGame {
            id: "steam:Celeste".into(),
            title: "Celeste".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        }]);
        assert_eq!(shared.status_gen(), 0);
        shared.set_running(&running(&["steam:Celeste"], "launching"));
        assert_eq!(shared.status_gen(), 1);
        assert_eq!(
            shared
                .launch_state("steam:Celeste")
                .map(|(s, _)| s)
                .as_deref(),
            Some("launching")
        );
        assert!(
            shared.snapshot().games[1].running,
            "launching is up on the shelf, behind the desktop tile"
        );
        let badges = shared.snapshot().generation;
        shared.set_running(&running(&["steam:Celeste"], "running"));
        assert_eq!(shared.status_gen(), 2);
        assert_eq!(shared.snapshot().generation, badges, "no badge moved");
        assert_eq!(
            shared
                .launch_state("steam:Celeste")
                .map(|(s, _)| s)
                .as_deref(),
            Some("running")
        );
        shared.set_running(&[]);
        assert_eq!(shared.launch_state("steam:Celeste"), None);
    }

    /// Cached catalog is `Ready` + stale, never an error. Live `set_games` clears the flag.
    #[test]
    fn a_cached_catalog_is_ready_and_stale_until_the_host_answers() {
        let g = |t: &str| LibraryGame {
            id: format!("steam:{t}"),
            title: t.to_string(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games_cached(vec![g("Celeste"), g("Tunic")]);
        let cached = shared.snapshot();
        assert!(matches!(cached.phase, LibraryPhase::Ready));
        assert_eq!(cached.stale, Stale::Waking);
        assert!(cached.stale.note().is_some());
        // The retry window closed with no answer: same titles, different words.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::Offline);
        shared.set_games(vec![g("Celeste"), g("Tunic"), g("Hades")]);
        let live = shared.snapshot();
        assert_eq!(
            live.stale,
            Stale::No,
            "the host answered — these are observed now"
        );
        assert!(live.stale.note().is_none());
        assert_eq!(live.games.len(), 4, "three titles behind the desktop tile");
        // A late give-up from an abandoned fetch must not mark a fresh library stale.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::No);
    }

    /// A launcher-less library keeps the host's order behind the one tile we add.
    #[test]
    fn set_games_leaves_a_launcher_less_library_alone() {
        let shared = LibraryShared::default();
        shared.set_games(
            ["Celeste", "Portal 2", "Tunic"]
                .iter()
                .map(|t| LibraryGame {
                    id: format!("steam:{t}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: None,
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: false,
                })
                .collect(),
        );
        let titles: Vec<String> = shared
            .snapshot()
            .games
            .iter()
            .map(|g| g.title.clone())
            .collect();
        assert_eq!(titles, ["Desktop", "Celeste", "Portal 2", "Tunic"]);
    }

    /// The tile is model state, so it leads whatever the catalog says — including a
    /// catalog with nothing in it, which keeps its Empty verdict for the shelf's copy.
    #[test]
    fn the_desktop_tile_leads_every_shelf_including_an_empty_one() {
        let shared = LibraryShared::default();
        shared.set_games(Vec::new());
        let snap = shared.snapshot();
        assert!(matches!(snap.phase, LibraryPhase::Empty));
        assert_eq!(snap.games.len(), 1);
        assert_eq!(snap.games[0].id, DESKTOP_ID);
        assert!(snap.games[0].leads());

        // A running title re-orders the games; the tile is not one of them.
        shared.set_games(vec![LibraryGame {
            id: "steam:Celeste".into(),
            title: "Celeste".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: true,
        }]);
        assert_eq!(shared.snapshot().games[0].id, DESKTOP_ID);
    }

    /// The tile has no cover and never gets one, so its mark is the whole card. A name the set
    /// does not carry falls through to the monogram, silently and on every shell at once.
    #[test]
    fn the_desktop_tile_names_a_mark_the_icon_set_ships() {
        assert!(crate::icons::by_name(&desktop_tile().icon).is_some());
    }

    /// Collections must never offer a "Desktop" group, and a one-store library must not
    /// look browsable because of it — but the unfiltered shelf still leads with the tile.
    #[test]
    fn collections_never_group_the_desktop_tile() {
        use crate::collate::{collate, filtered, worth_browsing, GroupBy, SortKey};

        let shared = LibraryShared::default();
        shared.set_games(
            ["Celeste", "Tunic"]
                .iter()
                .map(|t| LibraryGame {
                    id: format!("steam:{t}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: Some("PC".into()),
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: false,
                })
                .collect(),
        );
        let games = shared.snapshot().games;
        assert!(
            !worth_browsing(&games),
            "one platform plus the tile is still one platform"
        );
        for group in collate(&games, SortKey::Title, Some(GroupBy::Platform)) {
            assert!(
                !group.games.iter().any(|&i| games[i].id == DESKTOP_ID),
                "the tile reached a collection"
            );
        }
        assert_eq!(
            filtered(&games, SortKey::Title, None).first(),
            Some(&0),
            "the unfiltered shelf still leads with the tile"
        );
    }

    #[test]
    fn jump_clamps_onto_the_ends() {
        assert_eq!(step_cursor(1, 5, -JUMP, true), StepResult::Moved(0));
        assert_eq!(step_cursor(3, 5, JUMP, true), StepResult::Moved(4));
        assert_eq!(step_cursor(0, 5, -JUMP, true), StepResult::Boundary);
    }

    /// Stay finite through a stalled frame (0.05 s).
    #[test]
    fn springs_converge() {
        let (mut pos, mut vel) = (0.0, 0.0);
        for _ in 0..120 {
            (pos, vel) = spring_advance(pos, vel, 3.0, SPRING_K, SPRING_C, 1.0 / 60.0);
        }
        assert!((pos - 3.0).abs() < 0.01, "{pos}");
        let (p, v) = spring_advance(0.0, 0.0, 1.0, BUMP_K, BUMP_C, 0.05);
        assert!(
            p.is_finite() && v.is_finite() && p > 0.0 && p < 2.0,
            "{p}/{v}"
        );
    }

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

    #[test]
    fn initials_take_two_words() {
        assert_eq!(initials("Dota 2"), "D2");
        assert_eq!(initials("half-life"), "H");
    }

    /// The field's turn at t = 0 is its fixed tilt alone, and at any t a rotation: rows
    /// orthonormal, so the sphere is turned, never squashed.
    #[test]
    fn field_motion_is_a_rotation() {
        let m = field_motion(0.0);
        let (c, s) = (0.24f32.cos(), 0.24f32.sin());
        let tilt = [1.0, 0.0, 0.0, 0.0, 0.0, c, -s, 0.0, 0.0, s, c, 0.0];
        for (got, want) in m[..12].iter().zip(tilt) {
            assert!((got - want).abs() < 1e-5, "{m:?}");
        }
        let m = field_motion(1234.5);
        let row = |i: usize| [m[i * 4], m[i * 4 + 1], m[i * 4 + 2]];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        for i in 0..3 {
            for j in 0..3 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((dot(row(i), row(j)) - want).abs() < 1e-5);
            }
        }
    }

    /// Generated SkSL compiles for every palette (and a two-stop ramp), with the 144-byte
    /// block the shell packs.
    #[test]
    fn field_sksl_compiles_for_every_palette() {
        for p in &PALETTES {
            let src = field_sksl(p.ground, p.stops.unwrap_or(&VIOLET_FIELD));
            assert_eq!(src.matches('{').count(), src.matches('}').count());
            let effect = skia_safe::RuntimeEffect::make_for_shader(&src, None)
                .unwrap_or_else(|e| panic!("{}: {e}", p.id));
            assert_eq!(effect.uniform_size(), 144, "{}", p.id);
        }
        let two = field_sksl((0.0, 0.0, 0.0), &[(0.0, 0.0, 0.0), (1.0, 1.0, 1.0)]);
        assert!(skia_safe::RuntimeEffect::make_for_shader(&two, None).is_ok());
        let (focal, scale) = field_camera(16.0 / 9.0);
        assert!(focal > 1.0 && scale > 0.8, "{focal} {scale}");
    }

    #[test]
    fn violet_is_the_untouched_shipped_field() {
        assert_eq!(PALETTES[0].id, "violet");
        assert!(
            PALETTES[0].stops.is_none(),
            "the default is the explicit grid"
        );
        assert_eq!(palette("violet").mesh_colors(), MESH_COLORS);
        // An unknown name is a newer client's palette, not an error.
        assert_eq!(palette("chartreuse").id, "violet");
        assert_eq!(palette("").id, "violet");
    }

    /// Hue angle in degrees, or `None` for something too grey to have one.
    fn hue(c: (f64, f64, f64)) -> Option<f64> {
        let (r, g, b) = c;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        if d < 0.04 {
            return None;
        }
        let h = if max == r {
            60.0 * (((g - b) / d) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        Some((h + 360.0) % 360.0)
    }

    /// A palette is several hues, measured as the widest gap among the 16 mesh colours' hue angles.
    #[test]
    fn every_palette_is_multi_tone() {
        for p in &PALETTES {
            // Graphite, Paper and Slate are near-neutral; Void has no hue at all.
            if p.id == "void" {
                continue;
            }
            let hues: Vec<f64> = p.mesh_colors().iter().filter_map(|c| hue(*c)).collect();
            assert!(hues.len() >= 8, "{}: too few coloured cells", p.id);
            let spread = hues
                .iter()
                .flat_map(|a| {
                    hues.iter().map(move |b| {
                        let d = (a - b).abs() % 360.0;
                        d.min(360.0 - d)
                    })
                })
                .fold(0.0f64, f64::max);
            let floor = if matches!(p.id, "graphite" | "paper" | "slate") {
                20.0
            } else {
                45.0
            };
            assert!(spread >= floor, "{} spans only {spread:.0}° of hue", p.id);
        }
    }

    /// Ids are stored `ui_palette` values, and their order is the cycle order.
    #[test]
    fn ids_and_order_hold() {
        let ids: Vec<&str> = PALETTES.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "violet",
                "oled",
                "void",
                "graphite",
                "slate",
                "midnight",
                "electric",
                "ocean",
                "aurora",
                "jade",
                "emerald",
                "crimson",
                "ruby",
                "lava",
                "copper",
                "amber",
                "dusk",
                "grape",
                "neon",
                "tropic",
                "paper",
                "sky",
                "glacier",
                "lilac",
                "iris",
                "bubblegum",
                "coral",
                "flamingo",
                "peach",
                "candy",
                "lemon",
                "sunflower",
                "sherbet",
                "sage",
                "meadow",
                "lagoon",
            ]
        );
        // Dark fields lead, pale ones follow, so stepping the row walks one direction.
        let first_light = PALETTES
            .iter()
            .position(|p| p.light)
            .expect("some are light");
        assert!(PALETTES[first_light..].iter().all(|p| p.light));
        assert_eq!(first_light, 20);
    }

    /// `oled` is genuinely black: pure-black corners, mean under every other field, ground lifts to nothing.
    #[test]
    fn oled_is_actually_black() {
        let luma = |c: (f64, f64, f64)| 0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2;
        let oled = palette("oled");
        assert_eq!(
            oled.ground,
            (0.0, 0.0, 0.0),
            "the calm lift must be nothing"
        );
        let cells = oled.mesh_colors();
        assert!(
            cells.iter().filter(|c| luma(**c) == 0.0).count() >= 3,
            "the shaded corner has to be switched off, not dimmed"
        );
        let mean = cells.iter().map(|c| luma(*c)).sum::<f64>() / 16.0;
        let darkest_other = PALETTES
            .iter()
            .filter(|p| !matches!(p.id, "oled" | "void"))
            .map(|p| p.mesh_colors().iter().map(|c| luma(*c)).sum::<f64>() / 16.0)
            .fold(f64::MAX, f64::min);
        assert!(
            mean < darkest_other / 2.0,
            "oled means {mean:.3}, only half a stop under {darkest_other:.3}"
        );
    }

    /// Every colour a palette produces stays in gamut, and a pale palette really is pale —
    /// its ink flips, so a mislabelled one would put dark text on a dark field.
    #[test]
    fn palettes_are_in_gamut_and_honest_about_lightness() {
        let luma = |c: (f64, f64, f64)| 0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2;
        // WCAG contrast of white on `c`; 3:1 is the floor for bold and large type.
        let contrast = |c: (f64, f64, f64)| {
            let lin = |v: f64| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            1.05 / (luma((lin(c.0), lin(c.1), lin(c.2))) + 0.05)
        };
        for p in &PALETTES {
            for c in p.mesh_colors().iter() {
                for v in [c.0, c.1, c.2] {
                    assert!((0.0..=1.0).contains(&v), "{} {c:?}", p.id);
                }
            }
            let mean = p.mesh_colors().iter().map(|c| luma(*c)).sum::<f64>() / 16.0;
            if p.light {
                assert!(mean > 0.5, "{} is flagged light but means {mean:.2}", p.id);
                assert!(luma(p.ground) > 0.6, "{}'s ground is dark", p.id);
            } else {
                assert!(mean < 0.45, "{} is flagged dark but means {mean:.2}", p.id);
                assert!(
                    contrast(p.ground) >= 3.0,
                    "white ink fades on {}'s ground",
                    p.id
                );
            }
            // Accent tints glass of the opposite polarity: dark on white frost, bright on dark glass.
            let a = luma(p.accent);
            if p.light {
                assert!(a < 0.45, "{}'s accent is too pale for white glass", p.id);
            } else {
                assert!(a > 0.25, "{}'s accent is too dark for dark glass", p.id);
            }
        }
    }

    /// The ramp samples linearly between stops and clamps outside them.
    #[test]
    fn ramp_interpolates_between_stops() {
        let stops = [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (1.0, 1.0, 1.0)];
        assert_eq!(ramp(&stops, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(ramp(&stops, 1.0), (1.0, 1.0, 1.0));
        assert_eq!(ramp(&stops, 0.5), (1.0, 0.0, 0.0));
        let q = ramp(&stops, 0.25);
        assert!((q.0 - 0.5).abs() < 1e-9 && q.1 == 0.0);
        // Out of range clamps rather than panicking.
        assert_eq!(ramp(&stops, -3.0), (0.0, 0.0, 0.0));
        assert_eq!(ramp(&stops, 9.0), (1.0, 1.0, 1.0));
        assert_eq!(ramp(&[], 0.5), (0.0, 0.0, 0.0));
    }
}
