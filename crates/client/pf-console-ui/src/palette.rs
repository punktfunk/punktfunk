//! Background palettes and the mesh-gradient field: [`PALETTES`] is the one palette table,
//! and native pickers read its ids and names over their bridge.

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
        // The brand default: the website's deep-violet ground, lit by its brand violets.
        id: "violet", name: "Violet", stops: None,
        ground: (0.129, 0.094, 0.431), accent: (0.525, 0.471, 0.961), light: false,
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

/// The brand default's field ramp: the website's surfaces, dark first — `--neutral-accent`
/// `#0e093a`, `--neutral` `#21186e`, `--neutral-highlight` `#302593`, `--brand` `#6c5bf3`,
/// `--brand-light` `#a79ff8` (punktfunk-website `src/styles/globals.css`).
pub const VIOLET_FIELD: [(f64, f64, f64); 5] = [
    (0.055, 0.035, 0.227),
    (0.129, 0.094, 0.431),
    (0.188, 0.145, 0.576),
    (0.424, 0.357, 0.953),
    (0.655, 0.624, 0.973),
];

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

#[cfg(test)]
mod tests {
    use super::*;

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
