//! Shared look for the console shell: palette ink, embedded Geist, glass
//! panels, and the paint constructors every fill in this crate goes through.
//!
//! Skia's `Paint` defaults anti-alias off. [`fill`], [`stroke`], [`shaded`],
//! and [`layer`] are the only constructors; the shell test fails a stray
//! `Paint::new` outside this file. [`Ink`] is per-frame palette state, carried
//! with the frame's other state in the [`Frame`] the shell builds and hands down.
//! Form screens share the aurora mesh at `calm = 1`. No static backdrop here.
//!
//! Recede evidence: `recede_matrix_is_identity_at_the_focus` and neighbours.

use anyhow::{anyhow, Result};
use skia_safe::textlayout::{
    FontCollection, Paragraph, ParagraphBuilder, ParagraphStyle, TextAlign, TextStyle,
    TypefaceFontProvider,
};
use skia_safe::{
    gradient, Canvas, Color4f, Data, Font, FontMgr, FontStyle, MaskFilter, Paint, PathEffect,
    Point, RRect, Rect, TileMode, Typeface, Unichar,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Anti-aliased fill. Skia's `Paint::new` defaults `fAntiAlias` off, so a
/// bare constructor hard-steps every round-rect and glyph.
pub fn fill(color: Color4f) -> Paint {
    let mut p = Paint::new(color, None);
    p.set_anti_alias(true);
    p
}

/// Stroke of `width` device pixels. Callers that scale by `k` pass `width * k`.
pub fn stroke(color: Color4f, width: f32) -> Paint {
    let mut p = fill(color);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// Shader paint, opaque by construction. Skia multiplies shader output by
/// the paint's alpha, so an alpha-0 placeholder draws nothing.
pub fn shaded() -> Paint {
    fill(Color4f::new(0.0, 0.0, 0.0, 1.0))
}

/// [`shaded`] as a stroke. Opaque, or Skia scales the gradient's alpha away.
pub fn shaded_stroke(width: f32) -> Paint {
    let mut p = shaded();
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p
}

/// Radial shade under the in-stream ring so the discs lift off the picture.
/// `alpha` tracks the ring's opening.
pub fn ring_scrim(cx: f32, cy: f32, r: f32, alpha: f32) -> Paint {
    let mut p = shaded();
    let colors = [
        Color4f::new(0.0, 0.0, 0.0, 0.36 * alpha),
        Color4f::new(0.0, 0.0, 0.0, 0.14 * alpha),
    ];
    p.set_shader(gradient::shaders::radial_gradient(
        (Point::new(cx, cy), r),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    p
}

/// Blurred black under a disc or card. Lifts translucent glass off a moving picture.
pub fn soft_shadow(alpha: f32, sigma: f32) -> Paint {
    let mut p = fill(Color4f::new(0.0, 0.0, 0.0, alpha));
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// Blurred white stroke. Drawn under the crisp ring so the glow bleeds past the edge.
pub fn glow_ring(alpha: f32, width: f32, sigma: f32) -> Paint {
    let mut p = stroke(Color4f::new(1.0, 1.0, 1.0, alpha), width);
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, sigma, None));
    p
}

/// Top-edge hairline. The stream cannot backdrop-blur, so this is the glass edge.
pub fn rim_light(top: f32, bottom: f32, alpha: f32, width: f32) -> Paint {
    let mut p = shaded_stroke(width);
    let colors = [
        Color4f::new(1.0, 1.0, 1.0, alpha),
        Color4f::new(1.0, 1.0, 1.0, 0.0),
    ];
    p.set_shader(gradient::shaders::linear_gradient(
        (Point::new(0.0, top), Point::new(0.0, bottom)),
        &gradient::Gradient::new(
            gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            gradient::Interpolation::default(),
        ),
        None,
    ));
    p
}

/// `save_layer` paint: filters only, no geometry, so AA is a no-op. Own constructor
/// so the AA guard can tell a compositing paint from a drawing one.
pub fn layer() -> Paint {
    Paint::default()
}

/// A 10/10/10/2 canvas has two bits of alpha, so a fade or a mask layered on it steps in
/// quarters: the layer goes F16 there. Every `save_layer` in the crate carries this.
pub fn layer_flags(canvas: &Canvas) -> skia_safe::canvas::SaveLayerFlags {
    use skia_safe::ColorType::{BGRA1010102, RGBA1010102};
    if matches!(canvas.image_info().color_type(), RGBA1010102 | BGRA1010102) {
        skia_safe::canvas::SaveLayerFlags::F16_COLOR_TYPE
    } else {
        skia_safe::canvas::SaveLayerFlags::empty()
    }
}

/// `Canvas::save_layer_alpha_f` with [`layer_flags`].
pub fn save_layer_alpha(canvas: &Canvas, bounds: impl Into<Option<Rect>>, alpha: f32) -> usize {
    let mut paint = layer();
    paint.set_alpha_f(alpha);
    let bounds = bounds.into();
    let mut rec = skia_safe::canvas::SaveLayerRec::default()
        .paint(&paint)
        .flags(layer_flags(canvas));
    if let Some(b) = bounds.as_ref() {
        rec = rec.bounds(b);
    }
    canvas.save_layer(&rec)
}

/// Linear + linear mipmap. `draw_image_rect` defaults to nearest with no mipmaps;
/// a poster shrunk into a cell then drops whole source rows.
pub fn art_sampling() -> skia_safe::SamplingOptions {
    skia_safe::SamplingOptions::new(skia_safe::FilterMode::Linear, skia_safe::MipmapMode::Linear)
}

/// Status red (GTK `#ff938a`). A warning must not follow the wallpaper.
pub const ERROR: Color4f = Color4f::new(1.0, 0.576, 0.541, 1.0);
pub const ONLINE_GREEN: Color4f = Color4f::new(0.20, 0.84, 0.29, 1.0);

/// Glyph stroke, dp: every Lucide mark and device silhouette, whatever its box, so a 44 dp
/// mark and a 20 dp chevron on one screen share a weight. `icons::draw_icon` and
/// `glyphs::pad_mark` apply it; nothing else picks a stroke for a glyph.
pub const GLYPH_STROKE: f64 = 1.5;
/// Hairline, dp: the edge of a badge, a keycap or a battery cell — a container, not a glyph.
/// Unscaled it reads thinner than every other edge on HiDPI.
pub const HAIRLINE: f64 = 1.2;

/// Palette-derived fg, accent, glass, and scrim. Pale fields need dark text;
/// a brand-violet wash on a copper field clashes.
#[derive(Clone, Copy)]
pub struct Ink {
    fg: Color4f,
    accent: Color4f,
    /// Glass at a panel's top and at its foot. The shell draws no glass since the panel became
    /// a line; the theme lab's data (`dump_theme_lab`) still carries both for its preview.
    #[allow(dead_code)]
    glass: Color4f,
    #[allow(dead_code)]
    glass_low: Color4f,
    /// A coverless card's face at tint 0 and at tint 1 ([`Frame::card_face`]).
    face: [Color4f; 2],
    /// Drop shadow strength against a dark field's.
    shadow: f32,
    /// Ground the vignette leans toward (black on dark, white on pale) and how
    /// hard (`a`). Mixing toward white at dark-field strength bleaches chroma.
    pub scrim: Color4f,
}

const BLACK: Color4f = Color4f::new(0.0, 0.0, 0.0, 1.0);
const WHITE: Color4f = Color4f::new(1.0, 1.0, 1.0, 1.0);

/// Dark look, and the fallback before any palette is applied.
const DARK_INK: Ink = Ink {
    fg: WHITE,
    // Brand violet, dark-appearance `#8678F5`.
    accent: Color4f::new(0.525, 0.471, 0.961, 1.0),
    glass: Color4f::new(0.086, 0.086, 0.125, 0.62),
    glass_low: Color4f::new(0.086, 0.086, 0.125, 0.62),
    face: [BLACK, Color4f::new(0.525, 0.471, 0.961, 1.0)],
    shadow: 1.0,
    scrim: BLACK,
};

/// A mid-bright field (the brand default): white ink, violet glass clearing toward white at
/// its foot, violet faces deep enough for white titles, an indigo accent (white would hide
/// a switch's knob), and a whisper of vignette.
const VIVID_INK: Ink = Ink {
    fg: WHITE,
    accent: Color4f::new(0.28, 0.20, 0.70, 1.0),
    glass: Color4f::new(0.32, 0.24, 0.42, 0.30),
    glass_low: Color4f::new(1.0, 1.0, 1.0, 0.16),
    face: [
        Color4f::new(0.56, 0.42, 0.93, 1.0),
        Color4f::new(0.40, 0.26, 0.82, 1.0),
    ],
    shadow: 0.55,
    scrim: Color4f::new(0.0, 0.0, 0.0, 0.12),
};

/// A pale field: white frost (0.66 vs dark's 0.62, a bright gradient has less to separate
/// it), a softer shadow, and scrims that lean white. `fg` and `accent` are the palette's.
const PALE_INK: Ink = Ink {
    fg: BLACK,
    accent: BLACK,
    glass: Color4f::new(1.0, 1.0, 1.0, 0.66),
    glass_low: Color4f::new(1.0, 1.0, 1.0, 0.66),
    face: [WHITE, BLACK],
    shadow: 0.40,
    scrim: Color4f::new(1.0, 1.0, 1.0, 0.45),
};

/// A white-ink field whose ground is brighter than this takes [`VIVID_INK`].
const VIVID_GROUND: f64 = 0.3;

/// Relative luminance of an sRGB colour, for picking ink against a ground.
pub(crate) fn luma((r, g, b): (f64, f64, f64)) -> f64 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

impl Ink {
    /// Palette ink. Pale fields get near-black fg tinted toward the ground (a
    /// foreign grey reads as a second palette) and white-frost glass.
    pub fn of(p: &crate::palette::Palette) -> Ink {
        let accent = Color4f::new(p.accent.0 as f32, p.accent.1 as f32, p.accent.2 as f32, 1.0);
        if !p.light && luma(p.ground) > VIVID_GROUND {
            return VIVID_INK;
        }
        if !p.light {
            return Ink {
                accent,
                face: [BLACK, accent],
                ..DARK_INK
            };
        }
        let g = p.ground;
        Ink {
            fg: Color4f::new(
                (g.0 * 0.16) as f32,
                (g.1 * 0.14) as f32,
                (g.2 * 0.20) as f32,
                1.0,
            ),
            accent,
            face: [WHITE, accent],
            ..PALE_INK
        }
    }

    /// OS-theme ink. The accent is already lifted by [`crate::os_theme::readable_accent`];
    /// an arbitrary OS colour is not contrast-safe as focus. Fg is the theme's own.
    pub fn of_os(t: &crate::os_theme::OsTheme) -> Ink {
        let c =
            |crate::os_theme::Rgb(r, g, b), a: f32| Color4f::new(r as f32, g as f32, b as f32, a);
        let accent = c(crate::os_theme::readable_accent(t), 1.0);
        if !t.light {
            // Theme field, not brand violet-grey: a panel sits a shade above the ground it covers.
            let glass = c(t.background.mix(t.foreground, 0.10), 0.62);
            return Ink {
                fg: c(t.foreground, 1.0),
                accent,
                glass,
                glass_low: glass,
                face: [BLACK, accent],
                ..DARK_INK
            };
        }
        Ink {
            fg: c(t.foreground, 1.0),
            accent,
            face: [WHITE, accent],
            ..PALE_INK
        }
    }
}

/// What one frame draws with: the palette's ink, whether motion and the interface are reduced,
/// the safe area's left inset, the element layer's focus ledger, and the text an input method
/// is composing. The shell builds one a frame with [`Frame::next`] and hands it down in
/// [`crate::screens::Ctx`]; whatever draws takes it as `ui`. A host drawing the kit on its own
/// makes one with [`Frame::new`].
pub struct Frame {
    pub ink: Ink,
    /// Travel suppressed this frame. Callers keep the state change and drop the glide.
    /// Never used to skip a haptic: the pulse replaces the motion.
    pub reduce_motion: bool,
    /// The reduced interface: a TV-class GPU. Passes that read back what is already drawn
    /// (a backdrop blur) break a tiled GPU's render pass and are skipped.
    pub reduced_ui: bool,
    /// The safe area's left inset, px ([`Frame::edge`]).
    pub side_inset: f64,
    /// The shell is painting a screen into a transition layer. A copy of the surface cannot
    /// see that layer, so a band drawn inside it would blur the field behind.
    pub(crate) in_layer: Cell<bool>,
    pub el: crate::el::Ledger,
    /// An input method's text in progress. The field being typed into draws it at its caret,
    /// underlined; it is not in the field's value until the input method commits it.
    pub(crate) preedit: String,
}

impl Default for Frame {
    /// The dark look, at full motion, with no inset.
    fn default() -> Frame {
        Frame::new(DARK_INK, false, false, 0.0)
    }
}

impl Frame {
    pub fn new(ink: Ink, reduce_motion: bool, reduced_ui: bool, side_inset: f64) -> Frame {
        Frame {
            ink,
            reduce_motion,
            reduced_ui,
            side_inset,
            in_layer: Cell::new(false),
            el: crate::el::Ledger::default(),
            preedit: String::new(),
        }
    }

    /// The frame after this one. Only the focus a plate gave up here carries over.
    pub fn next(&self, ink: Ink, reduce_motion: bool, reduced_ui: bool, side_inset: f64) -> Frame {
        Frame {
            el: self.el.next(),
            ..Frame::new(ink, reduce_motion, reduced_ui, side_inset)
        }
    }

    pub fn fg(&self, alpha: f32) -> Color4f {
        let c = self.ink.fg;
        Color4f::new(c.r, c.g, c.b, alpha)
    }

    pub fn accent(&self, alpha: f32) -> Color4f {
        let c = self.ink.accent;
        Color4f::new(c.r, c.g, c.b, alpha)
    }

    /// Text wash. `alpha` is dark-field strength; multiplied by [`Ink::scrim`].a so a
    /// pale field does not bleach.
    pub fn shade(&self, alpha: f32) -> Color4f {
        let s = self.ink.scrim;
        Color4f::new(s.r, s.g, s.b, alpha * s.a)
    }

    /// Opaque coverless-card face, `tint` of the way along the ink's face ramp. Coverflow
    /// sides overlap, so glass would show the neighbour. Face and [`Frame::fg`] move opposite
    /// ways with the palette; a fixed near-black face under pale `fg` fails contrast.
    pub fn card_face(&self, tint: f32) -> Color4f {
        let [a, b] = self.ink.face;
        let mix = |x: f32, y: f32| x + (y - x) * tint;
        Color4f::new(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b), 1.0)
    }

    /// A drop shadow's alpha: `alpha` is dark-field strength, softened on brighter fields.
    pub fn shadow(&self, alpha: f32) -> f32 {
        alpha * self.ink.shadow
    }

    /// Black or white on the accent, by luminance not `light`: an accent is picked
    /// against the glass, not the field.
    pub fn on_accent(&self) -> Color4f {
        let a = self.ink.accent;
        let luma = 0.2126 * a.r + 0.7152 * a.g + 0.0722 * a.b;
        if luma > 0.55 {
            Color4f::new(0.0, 0.0, 0.0, 1.0)
        } else {
            Color4f::new(1.0, 1.0, 1.0, 1.0)
        }
    }

    /// Green text for what is live (online, playing) on glass: pastel under white ink, deep
    /// under dark ink.
    pub fn live(&self) -> Color4f {
        if self.ink.scrim.r > 0.5 {
            Color4f::new(0.06, 0.50, 0.18, 1.0)
        } else {
            Color4f::new(0.64, 0.95, 0.60, 1.0)
        }
    }

    /// The margin every screen shares, px from the layout's edge: [`EDGE_INSET`] from the
    /// screen's edge, of which a safe-area inset gives its share, and never under
    /// [`EDGE_AIR`] past the safe area.
    pub fn edge(&self, k: f64) -> f64 {
        (EDGE_INSET * k - self.side_inset).max(EDGE_AIR * k)
    }
}

pub enum PanelStroke {
    /// Hairline at this alpha (rows, pills).
    Plain(f32),
    /// Hairline at 0.22: the edge every card shares.
    Edge,
    /// [`Edge`] dashed `[6, 5]` design units.
    EdgeDashed,
    /// Accent hairline (the field being edited).
    Brand(f32),
}

/// A panel: a wash where the caller asks for one, and a hairline round it. The console's
/// material is the field showing through a line; nothing fills a card. `corner`, the dash
/// pattern and the line are design units; the caller's `k` scales them. The line keeps a
/// one-pixel floor.
pub fn panel(
    canvas: &Canvas,
    ui: &Frame,
    rect: Rect,
    corner: f32,
    tint: Option<Color4f>,
    stroke: PanelStroke,
    k: f32,
) {
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    if let Some(tint) = tint {
        canvas.draw_rrect(rr, &fill(tint));
    }
    // Opaque: Plain/Brand overwrite colour; the Edge is the one line every card shares.
    let mut sp = shaded_stroke(k.max(1.0));
    match stroke {
        PanelStroke::Plain(alpha) => {
            sp.set_color4f(ui.fg(alpha), None);
        }
        PanelStroke::Brand(alpha) => {
            sp.set_color4f(ui.accent(alpha), None);
        }
        PanelStroke::Edge | PanelStroke::EdgeDashed => {
            sp.set_color4f(ui.fg(0.22), None);
            if matches!(stroke, PanelStroke::EdgeDashed) {
                sp.set_path_effect(PathEffect::dash(&[6.0 * k, 5.0 * k], 0.0));
            }
        }
    }
    canvas.draw_rrect(rr, &sp);
}

/// 4×5 row-major colour matrix: neighbours lose saturation and brightness with
/// `d` (0 = focused, 1 = fully receded). Rec. 709 sat mix, then lerp toward the
/// ground: `out = (1 − b)·sat_mix(c) + ground·b`.
pub fn recede_matrix(ui: &Frame, d: f64) -> [f32; 20] {
    let d = d.clamp(0.0, 1.0);
    let sat = (1.0 - RECEDE_SATURATION * d) as f32;
    // Toward the ground, not darker. A darkened tile on a pale field gains contrast.
    let toward_light = ui.ink.scrim.r > 0.5;
    // Fraction of the way to the ground, not a level offset. An additive
    // darken clips a dark placeholder to #000; a lerp cannot clip at either pole.
    let b = (RECEDE_BRIGHTNESS * d) as f32;
    let ground = if toward_light { 1.0f32 } else { 0.0 };
    let keep = 1.0 - b;
    let offset = ground * b;
    const LR: f32 = 0.2126;
    const LG: f32 = 0.7152;
    const LB: f32 = 0.0722;
    let (ir, ig, ib) = (LR * (1.0 - sat), LG * (1.0 - sat), LB * (1.0 - sat));
    [
        keep * (ir + sat),
        keep * ig,
        keep * ib,
        0.0,
        offset,
        keep * ir,
        keep * (ig + sat),
        keep * ib,
        0.0,
        offset,
        keep * ir,
        keep * ig,
        keep * (ib + sat),
        0.0,
        offset,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// Saturation drain at full recede. Floor is 0.125 while brightness is 0.20:
/// channel spread must drop 30 %, and spread scales as `(1 − b)·sat`.
const RECEDE_SATURATION: f64 = 0.34;
/// Fraction of the way to the ground. An additive term clips; a dark card clips to nothing.
const RECEDE_BRIGHTNESS: f64 = 0.20;

/// The accent hairline round a focused card, with no glass: a poster under it keeps its
/// brightness. `corner` and the line are design units, the line one pixel at least.
pub fn focus_ring(canvas: &Canvas, ui: &Frame, rect: Rect, corner: f32, k: f32) {
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.draw_rrect(rr, &stroke(ui.accent(0.9), k.max(1.0)));
}

/// [`focus_halo`] growth past the card, design units. Applied to both rect and corner radius.
const HALO_OUTSET: f32 = 4.0;

/// Accent glow under the focused card. Drawn behind [`drop_shadow`].
pub fn focus_halo(canvas: &Canvas, ui: &Frame, rect: Rect, corner: f32, k: f32, f: f32) {
    if f <= 0.01 {
        return;
    }
    // Pale accents are dark, so a blurred accent on a pale field is a smudge.
    // Mix halfway to the scrim (white) to keep the hue; 0.24 vs 0.20 so the
    // lightened mark still registers.
    let (a, s) = (ui.ink.accent, ui.ink.scrim);
    let (c, alpha) = if s.r > 0.5 {
        let mix = |x: f32, y: f32| x + (y - x) * 0.5;
        (
            Color4f::new(mix(a.r, s.r), mix(a.g, s.g), mix(a.b, s.b), 1.0),
            0.24 * f,
        )
    } else {
        (a, 0.20 * f)
    };
    let mut p = fill(Color4f::new(c.r, c.g, c.b, alpha));
    // Outer, not Normal: Normal keeps the interior, so the halo fills the card
    // at full accent and shows through translucent glass (α 0.62 / 0.66).
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Outer,
        10.0 * k,
        None,
    ));
    // Outset, not offset: a halo spills every side; the shadow falls one way.
    // Reach is outset + 3σ and must stay inside the neighbour gap.
    let spread = rect.with_outset((HALO_OUTSET * k, HALO_OUTSET * k));
    // Radius grows by the same `d` as the rect or the arcs do not share a centre
    // and the halo reads squarer than the card at the corners.
    let r = (corner + HALO_OUTSET) * k;
    // The blur's nine-patch can seam across the card's middle; glass would show it.
    canvas.save();
    let card = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.clip_rrect(card, skia_safe::ClipOp::Difference, true);
    canvas.draw_rrect(RRect::new_rect_xy(spread, r, r), &p);
    canvas.restore();
}

/// A soft shadow falling below `rect`, drawn only outside it: glass shows what lies under.
pub fn drop_shadow(canvas: &Canvas, ui: &Frame, rect: Rect, corner: f32, k: f32, alpha: f32) {
    let mut p = fill(Color4f::new(0.0, 0.0, 0.0, ui.shadow(alpha)));
    p.set_mask_filter(MaskFilter::blur(
        skia_safe::BlurStyle::Normal,
        10.0 * k,
        None,
    ));
    let shifted = rect.with_offset((0.0, 10.0 * k));
    canvas.save();
    let card = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.clip_rrect(card, skia_safe::ClipOp::Difference, true);
    canvas.draw_rrect(RRect::new_rect_xy(shifted, corner * k, corner * k), &p);
    canvas.restore();
}

/// Loading spinner. `t` is the shell clock.
pub fn spinner(canvas: &Canvas, ui: &Frame, cx: f64, cy: f64, r: f64, t: f64) {
    let start = (t * 300.0) % 360.0;
    let mut paint = stroke(ui.fg(0.85), (r / 5.0) as f32);
    paint.set_stroke_cap(skia_safe::PaintCap::Round);
    canvas.draw_arc(
        Rect::from_xywh(
            (cx - r) as f32,
            (cy - r) as f32,
            2.0 * r as f32,
            2.0 * r as f32,
        ),
        start as f32,
        270.0,
        false,
        &paint,
    );
}

/// Content inset from the screen edge, design units: a ten-foot column, read from a sofa.
pub const EDGE_INSET: f64 = 40.0;
/// Least air between the safe area and content, design units. A notch or a rounded
/// corner is not a margin.
const EDGE_AIR: f64 = 20.0;

/// Geist weights, matching the Apple client's `.geist(size, weight)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum W {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

/// Embedded Geist, the faces that draw what it lacks, and a paragraph collection over both.
/// Geist has Latin and Cyrillic only; a title or a host name can be Han, kana or Hangul.
pub struct Fonts {
    regular: Typeface,
    medium: Typeface,
    semibold: Typeface,
    bold: Typeface,
    collection: FontCollection,
    /// The host's own faces ([`build_fonts_with`]), tried before the system's.
    extra: Vec<Typeface>,
    /// A paragraph's families: Geist, then [`Self::extra`] under their aliases.
    families: Vec<String>,
    fallback: RefCell<Fallback>,
    /// Shaped paragraphs keyed by [`ParaKey`]. Position is not in the key (`paint`
    /// takes it) so a scrolling shelf reuses the same layout. `RefCell`: every draw
    /// path is `&self` and the shell is one render thread.
    paragraphs: RefCell<HashMap<ParaKey, Cached>>,
    /// Liveness clock, bumped by [`Fonts::begin_frame`].
    frame: Cell<u64>,
}

/// Faces found for chars Geist lacks, under one UI language: the language picks among the
/// Han faces, so a change empties it.
#[derive(Default)]
struct Fallback {
    lang: &'static str,
    /// By char and weight; `None` when no face has the char.
    chars: HashMap<(char, W), Option<Typeface>>,
    /// The last system face found per 256-codepoint page and weight, tried before asking the
    /// system again. A page is one script's neighbourhood: kana never borrows a Hangul face.
    pages: HashMap<(u32, W), Typeface>,
}

/// The language hint a font manager reads. Android's fallback list tags Chinese by script,
/// fontconfig by region; each spelling picks the other's wrong Han face.
fn font_lang(lang: &'static str) -> &'static str {
    const CHINESE: [(&str, &str); 3] = [
        ("zh-Hans", "zh-CN"),
        ("zh-Hant", "zh-TW"),
        ("zh-Hant", "zh-HK"),
    ];
    let android = cfg!(target_os = "android");
    for (script, region) in CHINESE {
        let (from, to) = if android {
            (region, script)
        } else {
            (script, region)
        };
        if lang.eq_ignore_ascii_case(from) {
            return to;
        }
    }
    lang
}

/// Whether `face` has a glyph for `c`.
fn has(face: &Typeface, c: char) -> bool {
    face.unichar_to_glyph(c as Unichar) != 0
}

/// Whether `face` has every char of `text`: one lookup for a line of up to 64 chars. A call
/// per char costs more than the `measure_str` it guards.
fn covers(face: &Typeface, text: &str) -> bool {
    let mut glyphs = [0; 64];
    let n = face.str_to_glyphs(text, &mut glyphs);
    match glyphs.get(..n) {
        Some(found) => found.iter().all(|&g| g != 0),
        None => text.chars().all(|c| has(face, c)),
    }
}

/// `face` at `size`, positioned to the subpixel like every run the kit draws.
fn sized(face: &Typeface, size: f64) -> Font {
    let mut f = Font::new(face.clone(), size as f32);
    f.set_subpixel(true);
    f
}

fn font_style(w: W) -> FontStyle {
    use skia_safe::font_style::{Slant, Weight, Width};
    match w {
        W::Regular => FontStyle::normal(),
        W::Medium => FontStyle::new(Weight::MEDIUM, Width::NORMAL, Slant::Upright),
        W::SemiBold => FontStyle::new(Weight::SEMI_BOLD, Width::NORMAL, Slant::Upright),
        W::Bold => FontStyle::bold(),
    }
}

/// Paragraph shape. A tag, not a loose `(TextAlign, Option<usize>)` pair: those two
/// are not independent, and this is half of a hash key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Para {
    Centered,
    Leading,
    /// Left-aligned, one ellipsized line.
    Heading,
    /// Left-aligned, three lines then an ellipsis.
    Title,
}

impl Para {
    fn style(self) -> (TextAlign, Option<usize>) {
        match self {
            Para::Centered => (TextAlign::Center, None),
            Para::Leading => (TextAlign::Left, None),
            Para::Heading => (TextAlign::Left, Some(1)),
            Para::Title => (TextAlign::Left, Some(3)),
        }
    }
}

/// Everything that changes the shaped result. Floats ride as bits: sizes are
/// `k`-scaled so never whole, and `f32` is not `Hash`. Same `k` → same bits.
#[derive(PartialEq, Eq, Hash)]
struct ParaKey {
    text: String,
    kind: Para,
    weight: W,
    size: u64,
    max_w: u32,
    /// ARGB, as `[a, r, g, b]`.
    color: [u8; 4],
    /// The UI language: it picks the Han face.
    lang: &'static str,
}

struct Cached {
    para: Paragraph,
    used: u64,
}

/// Cap before cold entries drop. A screen is well under this; the library can page
/// thousands of titles and would otherwise keep every one.
const PARA_CACHE_MAX: usize = 512;

/// Shape one paragraph. Free function: the cache hands a `&ParaKey` borrowed from
/// the map being inserted into, which rules out `&self` across the call.
fn shape(collection: &FontCollection, families: &[String], key: &ParaKey) -> Paragraph {
    let (align, clamp) = key.kind.style();
    let mut style = ParagraphStyle::new();
    style.set_text_align(align);
    if let Some(lines) = clamp {
        style.set_max_lines(lines);
        style.set_ellipsis("\u{2026}");
    }
    let mut ts = TextStyle::new();
    ts.set_font_families(families);
    ts.set_locale(font_lang(key.lang));
    ts.set_font_size(f64::from_bits(key.size) as f32);
    let [a, r, g, b] = key.color;
    ts.set_color(skia_safe::Color::from_argb(a, r, g, b));
    ts.set_font_style(font_style(key.weight));
    style.set_text_style(&ts);
    let mut builder = ParagraphBuilder::new(&style, collection.clone());
    builder.add_text(&key.text);
    let mut p = builder.build();
    p.layout(f32::from_bits(key.max_w));
    p
}

/// Embedded so a bare gamescope session with no font packages still looks right.
/// fontconfig still serves CJK fallback where it exists.
const GEIST_REGULAR: &[u8] = include_bytes!("../assets/fonts/Geist-Regular.otf");
const GEIST_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Geist-Medium.otf");
const GEIST_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.otf");
const GEIST_BOLD: &[u8] = include_bytes!("../assets/fonts/Geist-Bold.otf");

/// [`build_fonts_with`] and no faces of the host's own.
pub fn build_fonts() -> Result<Fonts> {
    build_fonts_with(&[])
}

/// Geist, then `fallback`'s faces for the chars it lacks, then the system's. A host with no
/// system fonts (a console, the browser) hands a CJK face here, or every Han, kana and Hangul
/// char draws as a box. Each blob is one face; of a collection, its first.
pub fn build_fonts_with(fallback: &[&[u8]]) -> Result<Fonts> {
    build(fallback, FontMgr::new())
}

/// Fonts with no system faces: what a test draws does not hang on the machine's fonts.
#[cfg(test)]
pub(crate) fn test_fonts(fallback: &[&[u8]]) -> Fonts {
    build(fallback, FontMgr::empty()).expect("faces load")
}

/// A face for chars Geist lacks: Lucide's icons, at private-use codepoints from U+E000.
#[cfg(test)]
pub(crate) const TEST_FALLBACK: &[u8] = include_bytes!("../../../../assets/lucide/font/lucide.ttf");

fn build(fallback: &[&[u8]], system: FontMgr) -> Result<Fonts> {
    let mgr = FontMgr::new();
    let load = |bytes: &[u8]| mgr.new_from_data(Data::new_copy(bytes), None);
    let geist = |bytes, which: &str| {
        load(bytes).ok_or_else(|| anyhow!("embedded Geist face rejected: {which}"))
    };
    let regular = geist(GEIST_REGULAR, "Regular")?;
    let medium = geist(GEIST_MEDIUM, "Medium")?;
    let semibold = geist(GEIST_SEMIBOLD, "SemiBold")?;
    let bold = geist(GEIST_BOLD, "Bold")?;
    let extra = (fallback.iter().enumerate())
        .map(|(i, bytes)| load(bytes).ok_or_else(|| anyhow!("fallback face rejected: {i}")))
        .collect::<Result<Vec<_>>>()?;

    // Asset provider first: all four weights under one "Geist" alias so style matching
    // picks the face, the host's faces each under their own; system manager is fallback.
    let mut provider = TypefaceFontProvider::new();
    for tf in [&regular, &medium, &semibold, &bold] {
        provider.register_typeface(tf.clone(), Some("Geist"));
    }
    let mut families = vec!["Geist".to_owned()];
    for (i, tf) in extra.iter().enumerate() {
        families.push(format!("pf-fallback-{i}"));
        provider.register_typeface(tf.clone(), Some(families[i + 1].as_str()));
    }
    let mut collection = FontCollection::new();
    collection.set_asset_font_manager(Some(provider.into()));
    collection.set_default_font_manager(system, None);
    Ok(Fonts {
        regular,
        medium,
        semibold,
        bold,
        collection,
        extra,
        families,
        fallback: RefCell::default(),
        paragraphs: RefCell::new(HashMap::new()),
        frame: Cell::new(0),
    })
}

impl Fonts {
    fn face(&self, w: W) -> &Typeface {
        match w {
            W::Regular => &self.regular,
            W::Medium => &self.medium,
            W::SemiBold => &self.semibold,
            W::Bold => &self.bold,
        }
    }

    /// Geist alone. Text goes through [`measure`](Self::measure) and [`draw`](Self::draw),
    /// which reach past Geist for a char it lacks.
    pub fn font(&self, w: W, size: f64) -> Font {
        sized(self.face(w), size)
    }

    /// The face that draws `c`: Geist when it has the glyph, else [`Self::fallback`], else
    /// Geist's missing-glyph box. ASCII is always Geist, as [`Self::runs`] takes it.
    fn face_for(&self, c: char, w: W) -> Typeface {
        let geist = self.face(w);
        if c.is_ascii() || has(geist, c) {
            return geist.clone();
        }
        self.fallback(c, w).unwrap_or_else(|| geist.clone())
    }

    /// The face for `c`, which Geist lacks: the host's, else the system's for the UI language.
    /// `None` when no face has it.
    fn fallback(&self, c: char, w: W) -> Option<Typeface> {
        let lang = pf_l10n::language();
        let mut fb = self.fallback.borrow_mut();
        if fb.lang != lang {
            *fb = Fallback {
                lang,
                ..Fallback::default()
            };
        }
        if let Some(face) = fb.chars.get(&(c, w)) {
            return face.clone();
        }
        let page = (u32::from(c) >> 8, w);
        let mut face = self.extra.iter().find(|f| has(f, c)).cloned();
        if face.is_none() {
            face = fb.pages.get(&page).filter(|f| has(f, c)).cloned();
        }
        if face.is_none() {
            // The paragraph path's own lookup, so a title and its card label share a face.
            face = (self.collection.clone())
                .default_fallback_char_and_families(
                    c as Unichar,
                    &["Geist"],
                    font_style(w),
                    font_lang(lang),
                    None,
                )
                .filter(|f| has(f, c));
            if let Some(f) = &face {
                fb.pages.insert(page, f.clone());
            }
        }
        fb.chars.insert((c, w), face.clone());
        face
    }

    /// `text` cut where its face changes. `None` when Geist draws all of it, so the common
    /// line stays one `draw_str` and pure ASCII costs one scan.
    fn runs<'t>(&self, text: &'t str, w: W) -> Option<Vec<(&'t str, Typeface)>> {
        let geist = self.face(w);
        if text.is_ascii() || covers(geist, text) {
            return None;
        }
        let mut runs = Vec::new();
        let (mut start, mut face) = (0, geist.clone());
        for (i, c) in text.char_indices() {
            let next = self.face_for(c, w);
            if next.unique_id() != face.unique_id() {
                if i > start {
                    runs.push((&text[start..i], face));
                }
                (start, face) = (i, next);
            }
        }
        runs.push((&text[start..], face));
        Some(runs)
    }

    /// The advance [`draw`](Self::draw) gives `text`.
    pub fn measure(&self, text: &str, w: W, size: f64) -> f32 {
        match self.runs(text, w) {
            None => self.font(w, size).measure_str(text, None).0,
            Some(runs) => (runs.iter())
                .map(|(t, face)| sized(face, size).measure_str(t, None).0)
                .sum(),
        }
    }

    /// `draw_str` at a baseline, not a top edge. Returns the advance so callers can chain runs.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        color: Color4f,
    ) -> f32 {
        self.put(canvas, text, x, baseline, w, size, color, None)
    }

    /// [`draw`](Self::draw) for a run its caller gave `boxed` width. Each face's run sits on
    /// the one baseline. The text probe checks the run against `boxed`, else the canvas clip.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(not(test), allow(unused_variables))]
    fn put(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        color: Color4f,
        boxed: Option<f64>,
    ) -> f32 {
        let paint = fill(color);
        let runs = self.runs(text, w);
        let advance = match &runs {
            None => {
                let font = self.font(w, size);
                canvas.draw_str(text, Point::new(x as f32, baseline as f32), &font, &paint);
                font.measure_str(text, None).0
            }
            Some(runs) => {
                let mut pen = x as f32;
                for (t, face) in runs {
                    let font = sized(face, size);
                    canvas.draw_str(t, Point::new(pen, baseline as f32), &font, &paint);
                    pen += font.measure_str(t, None).0;
                }
                pen - x as f32
            }
        };
        #[cfg(test)]
        {
            let whole = [(text, self.face(w).clone())];
            let missing = (runs.as_deref().unwrap_or(&whole).iter())
                .any(|(t, face)| t.chars().any(|c| !has(face, c)));
            crate::textprobe::run(
                canvas,
                text,
                (x as f32, advance),
                (
                    (baseline - size * 0.8) as f32,
                    (baseline + size * 0.25) as f32,
                ),
                boxed.map(|b| b as f32),
                missing,
            );
        }
        advance
    }

    /// Bump the paragraph-cache clock. Entries not drawn this frame or the last
    /// are eviction candidates. The shell calls this once per `render_in`.
    pub fn begin_frame(&self) {
        self.frame.set(self.frame.get().wrapping_add(1));
    }

    /// Draw a cached paragraph and return its laid-out height. `at` is top-left and
    /// is not part of the key. With no canvas it only shapes, for a caller placing
    /// a block by a title's height before drawing it.
    #[allow(clippy::too_many_arguments)]
    fn draw_paragraph(
        &self,
        canvas: Option<&Canvas>,
        text: &str,
        kind: Para,
        w: W,
        size: f64,
        color: Color4f,
        max_w: f64,
        at: Point,
    ) -> f64 {
        let frame = self.frame.get();
        // Owned key: a hit still allocates a `String`. Cheap next to a reshape.
        let key = ParaKey {
            text: text.to_owned(),
            kind,
            weight: w,
            size: size.to_bits(),
            max_w: (max_w as f32).to_bits(),
            color: {
                // 8-bit ARGB the paragraph bakes, not the `Color4f`: two floats that
                // round to the same pixel share an entry.
                let c = color.to_color();
                [c.a(), c.r(), c.g(), c.b()]
            },
            lang: pf_l10n::language(),
        };
        let mut cache = self.paragraphs.borrow_mut();
        let entry = cache.entry(key).or_insert_with_key(|k| Cached {
            para: shape(&self.collection, &self.families, k),
            used: frame,
        });
        entry.used = frame;
        if let Some(canvas) = canvas {
            entry.para.paint(canvas, at);
            #[cfg(test)]
            {
                let line = entry.para.longest_line();
                let lead = if kind == Para::Centered {
                    (max_w as f32 - line) / 2.0
                } else {
                    0.0
                };
                let rows = (at.y, at.y + entry.para.height());
                let missing = entry.para.unresolved_glyphs().is_some_and(|n| n > 0);
                let run = (at.x + lead, line);
                crate::textprobe::run(canvas, text, run, rows, Some(max_w as f32), missing);
            }
        }
        let height = f64::from(entry.para.height());
        // Reap entries not drawn this frame or the last (`used + 1 >= frame`).
        if cache.len() > PARA_CACHE_MAX {
            cache.retain(|_, c| c.used + 1 >= frame);
        }
        height
    }

    /// Centered wrapping paragraph; `y` is the top edge (CJK fallback).
    #[allow(clippy::too_many_arguments)]
    pub fn centered(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        cx: f64,
        y: f64,
        max_w: f64,
    ) {
        let at = Point::new((cx - max_w / 2.0) as f32, y as f32);
        self.draw_paragraph(
            Some(canvas),
            text,
            Para::Centered,
            w,
            size,
            color,
            max_w,
            at,
        );
    }

    /// Left-aligned twin of [`centered`](Self::centered). Same paragraph path (CJK
    /// fallback); chrome cannot use `draw`/`draw_clipped` instead. Returns the laid-out
    /// height, so what follows can sit under the wrap.
    #[allow(clippy::too_many_arguments)]
    pub fn leading(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        x: f64,
        y: f64,
        max_w: f64,
    ) -> f64 {
        let at = Point::new(x as f32, y as f32);
        self.draw_paragraph(Some(canvas), text, Para::Leading, w, size, color, max_w, at)
    }

    /// Left-aligned title at `(x, y)` top edge, at most three lines then an ellipsis.
    /// Returns the laid-out height so the lines under it follow the wrap.
    #[allow(clippy::too_many_arguments)]
    pub fn title(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        x: f64,
        y: f64,
        max_w: f64,
    ) -> f64 {
        let at = Point::new(x as f32, y as f32);
        self.draw_paragraph(Some(canvas), text, Para::Title, w, size, color, max_w, at)
    }

    /// The height [`title`](Self::title) draws at `max_w`, without drawing it. Same
    /// cache entry, so measuring before drawing shapes once.
    pub fn title_height(&self, text: &str, w: W, size: f64, color: Color4f, max_w: f64) -> f64 {
        let at = Point::new(0.0, 0.0);
        self.draw_paragraph(None, text, Para::Title, w, size, color, max_w, at)
    }

    /// Left-aligned heading at `(x, y)`, one ellipsized line at `max_w`. A wrap would
    /// run under the controller chip and push a second line into the content.
    #[allow(clippy::too_many_arguments)]
    pub fn heading(
        &self,
        canvas: &Canvas,
        text: &str,
        w: W,
        size: f64,
        color: Color4f,
        x: f64,
        y: f64,
        max_w: f64,
    ) {
        let at = Point::new(x as f32, y as f32);
        self.draw_paragraph(Some(canvas), text, Para::Heading, w, size, color, max_w, at);
    }

    /// One line, ellipsized to `max_w`, at a baseline. For titles that exceed their tile.
    /// Each char is measured in the face that draws it, so the cut fits what is drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_clipped(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f64,
        baseline: f64,
        w: W,
        size: f64,
        color: Color4f,
        max_w: f64,
    ) {
        if self.measure(text, w, size) <= max_w as f32 {
            self.put(canvas, text, x, baseline, w, size, color, Some(max_w));
            return;
        }
        let ell = "…";
        let ell_w = self.measure(ell, w, size);
        let runs = (self.runs(text, w)).unwrap_or_else(|| vec![(text, self.face(w).clone())]);
        let (mut used, mut end) = (0.0f32, 0);
        // Measure from a stack buffer, not a `String` per character: this runs
        // every over-long title, every frame.
        let mut buf = [0u8; 4];
        'fit: for (t, face) in &runs {
            let font = sized(face, size);
            for ch in t.chars() {
                let cw = font.measure_str(&*ch.encode_utf8(&mut buf), None).0;
                if used + cw + ell_w > max_w as f32 {
                    break 'fit;
                }
                used += cw;
                end += ch.len_utf8();
            }
        }
        let fitted = format!("{}{ell}", &text[..end]);
        self.put(canvas, &fitted, x, baseline, w, size, color, Some(max_w));
    }
}

/// `src` composited over an opaque `dst`: the opaque colour a stacked paint shows.
pub(crate) fn over(src: Color4f, dst: Color4f) -> Color4f {
    let m = |s: f32, d: f32| s * src.a + d * (1.0 - src.a);
    Color4f::new(m(src.r, dst.r), m(src.g, dst.g), m(src.b, dst.b), 1.0)
}

/// WCAG contrast of two opaque colours ([`pf_client_core::rgb::contrast`]).
#[cfg(test)]
pub(crate) fn contrast(a: Color4f, b: Color4f) -> f64 {
    use pf_client_core::rgb::Rgb;
    let rgb = |c: Color4f| Rgb(f64::from(c.r), f64::from(c.g), f64::from(c.b));
    pf_client_core::rgb::contrast(rgb(a), rgb(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cargo test -p pf-console-ui --lib dump_theme_lab -- --ignored` refreshes
    /// `tools/theme-lab/lab-data.json`: the palette table, the inks [`Ink::of`] picks from, and
    /// the field shader around its gradient, which the lab rebuilds from edited stops.
    #[test]
    #[ignore]
    fn dump_theme_lab() {
        use crate::palette::{field_sksl, CELL_RAMP, MESH_COLORS, PALETTES, VIOLET_FIELD};
        use serde_json::json;
        let rgb = |c: (f64, f64, f64)| json!([c.0, c.1, c.2]);
        let c4 = |c: Color4f| json!([c.r, c.g, c.b, c.a]);
        let ink = |i: &Ink| {
            json!({
                "fg": c4(i.fg), "accent": c4(i.accent), "glass": c4(i.glass),
                "glass_low": c4(i.glass_low), "face": [c4(i.face[0]), c4(i.face[1])],
                "shadow": i.shadow, "scrim": c4(i.scrim),
            })
        };
        let palettes: Vec<_> = (PALETTES.iter())
            .map(|p| {
                json!({
                    "id": p.id, "name": (p.name)(), "light": p.light,
                    "stops": p.stops.map(|s| s.iter().map(|c| rgb(*c)).collect::<Vec<_>>()),
                    "ground": rgb(p.ground), "accent": rgb(p.accent),
                })
            })
            .collect();
        let p = &PALETTES[1];
        let stops = p.stops.expect("the second palette has its own ramp");
        let sksl = field_sksl(p.ground, stops);
        let (head, rest) = (sksl.split_once("    float x = clamp(t, 0.0, 1.0) * "))
            .expect("the gradient's first line");
        let tail = &rest[rest
            .find("    f = f * f * (3.0")
            .expect("the gradient's blend")..];
        let data = json!({
            "palettes": palettes,
            "violet_field": VIOLET_FIELD.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
            "vivid_ground": VIVID_GROUND,
            // The palette gate's inputs (`palette.rs` tests): it samples this mesh, not the field.
            "cell_ramp": CELL_RAMP,
            "mesh_colors": MESH_COLORS.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
            "inks": { "dark": ink(&DARK_INK), "vivid": ink(&VIVID_INK), "pale": ink(&PALE_INK) },
            "sksl": { "head": head, "tail": tail },
            "check": {
                "ground": rgb(p.ground),
                "stops": stops.iter().map(|c| rgb(*c)).collect::<Vec<_>>(),
                "sksl": sksl,
            },
        });
        let out = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tools/theme-lab/lab-data.json"
        );
        std::fs::write(out, serde_json::to_string_pretty(&data).unwrap() + "\n").unwrap();
    }

    /// A bad embedded face takes out every console screen at init.
    #[test]
    fn embedded_fonts_load() {
        let fonts = build_fonts().expect("Geist faces load");
        for w in [W::Regular, W::Medium, W::SemiBold, W::Bold] {
            assert!(fonts.measure("Punktfunk", w, 16.0) > 0.0);
        }
        // Heavier faces are wider at the same size: four distinct faces, not one reused.
        assert!(
            fonts.measure("Punktfunk", W::Bold, 16.0)
                > fonts.measure("Punktfunk", W::Regular, 16.0)
        );
    }

    /// Latin and Cyrillic stay one Geist `draw_str`, at the width Geist alone gives.
    #[test]
    fn geist_text_takes_the_one_run_path() {
        let fonts = test_fonts(&[TEST_FALLBACK]);
        let ascii: String = (' '..='~').collect();
        for text in [ascii.as_str(), "Einstellungen für Ŵéŕķ", "Настройки"] {
            assert!(fonts.runs(text, W::Medium).is_none(), "{text:?} split");
            let geist = fonts.font(W::Medium, 15.0).measure_str(text, None).0;
            assert_eq!(fonts.measure(text, W::Medium, 15.0), geist, "{text:?}");
        }
        // The ASCII shortcut skips the glyph check: Geist has every printable one.
        assert!(ascii.chars().all(|c| has(&fonts.medium, c)));
    }

    /// A char Geist lacks takes the fallback face's advance, and draw and measure agree.
    #[test]
    fn a_char_geist_lacks_draws_in_the_fallback_face() {
        let (text, icon) = ("Play \u{e58a}", "\u{e58a}");
        let fonts = test_fonts(&[TEST_FALLBACK]);
        let runs = fonts
            .runs(text, W::Regular)
            .expect("the icon splits the line");
        assert_eq!(
            runs.iter().map(|r| r.0).collect::<Vec<_>>(),
            ["Play ", icon]
        );
        let lucide = sized(&fonts.extra[0], 16.0).measure_str(icon, None).0;
        assert!(lucide > 0.0);
        let latin = fonts.measure("Play ", W::Regular, 16.0);
        assert_eq!(fonts.measure(text, W::Regular, 16.0), latin + lucide);
        // Without a fallback the box takes Geist's missing-glyph advance instead.
        let bare = test_fonts(&[]).measure(text, W::Regular, 16.0);
        assert_ne!(bare, latin + lucide);
        let mut surface = skia_safe::surfaces::raster_n32_premul((200, 40)).unwrap();
        let ink = Color4f::new(1.0, 1.0, 1.0, 1.0);
        let drawn = fonts.draw(surface.canvas(), text, 0.0, 30.0, W::Regular, 16.0, ink);
        assert_eq!(drawn, latin + lucide);
    }

    /// A long line of chars Geist lacks ellipsizes inside its box: Lucide icons always, and
    /// Japanese where this machine has a CJK face.
    #[test]
    fn a_fallback_line_ellipsizes_inside_its_box() {
        let icons: String = ('\u{e58a}'..='\u{e5a0}').collect();
        let cases = [
            (test_fonts(&[TEST_FALLBACK]), icons),
            (
                build_fonts().unwrap(),
                "日本語のとても長いゲームのタイトル".repeat(3),
            ),
        ];
        let mut surface = skia_safe::surfaces::raster_n32_premul((400, 60)).unwrap();
        let ink = Color4f::new(1.0, 1.0, 1.0, 1.0);
        for (fonts, text) in cases {
            let first = text.chars().next().unwrap();
            if !has(&fonts.face_for(first, W::Regular), first) {
                eprintln!("skipped {first:?}: no face on this machine has it");
                continue;
            }
            let max_w = 120.0;
            assert!(fonts.measure(&text, W::Regular, 16.0) > max_w as f32);
            crate::textprobe::begin();
            let c = surface.canvas();
            fonts.draw_clipped(c, &text, 10.0, 30.0, W::Regular, 16.0, ink, max_w);
            let run = crate::textprobe::end().remove(0);
            assert!(
                run.text.ends_with('…') && run.text.chars().count() > 2,
                "{run:?}"
            );
            assert!(run.x1 - run.x0 <= max_w as f32, "{run:?}");
            assert!(!run.missing, "{run:?}");
        }
    }

    /// Chinese reaches each font manager in the spelling its fallback list is tagged with.
    #[test]
    fn the_han_hint_matches_the_platform() {
        assert_eq!(font_lang("ja"), "ja");
        let (hans, hant) = (font_lang("zh-Hans"), font_lang("zh-TW"));
        if cfg!(target_os = "android") {
            assert_eq!((hans, hant), ("zh-Hans", "zh-Hant"));
        } else {
            assert_eq!((hans, hant), ("zh-CN", "zh-TW"));
        }
    }

    /// Skia's 4×5 row-major apply, without the clamp, so a channel below zero is visible.
    fn apply(m: &[f32; 20], c: [f32; 4]) -> [f32; 4] {
        core::array::from_fn(|row| {
            let o = row * 5;
            m[o] * c[0] + m[o + 1] * c[1] + m[o + 2] * c[2] + m[o + 3] * c[3] + m[o + 4]
        })
    }

    /// Focused card is identity, so every card can share one code path. A half-percent
    /// tint at d=0 is invisible on glass and wrong in every screenshot.
    #[test]
    fn recede_matrix_is_identity_at_the_focus() {
        let m = recede_matrix(&Frame::default(), 0.0);
        for c in [
            [1.0, 0.2, 0.4, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.3, 0.9, 0.1, 0.5],
        ] {
            let out = apply(&m, c);
            for i in 0..4 {
                assert!((out[i] - c[i]).abs() < 1e-5, "{c:?} became {out:?}");
            }
        }
    }

    #[test]
    fn recede_matrix_drains_colour_and_light_but_never_alpha() {
        let m = recede_matrix(&Frame::default(), 1.0);
        let c = [0.9f32, 0.2, 0.15, 1.0]; // high chroma: spread drain is the assertion
        let out = apply(&m, c);
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(
            spread(out) < spread(c) * 0.7,
            "colour did not drain: {out:?}"
        );
        let lum = |v: [f32; 4]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        assert!(lum(out) < lum(c), "it did not darken: {out:?}");
        // Alpha is untouched: coverflow sides overlap, so a recede that reached alpha
        // would show neighbours through each other.
        assert_eq!(out[3], 1.0);
    }

    /// Recede moves toward the ground, the opposite direction on a pale palette.
    /// Dark-palette screenshots miss this: a darkened neighbour gains contrast.
    #[test]
    fn recede_moves_toward_the_ground_on_a_pale_palette() {
        let lum = |v: [f32; 4]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        let card = [0.55f32, 0.55, 0.6, 1.0];

        let ui = |id| Frame::new(Ink::of(crate::palette::palette(id)), false, false, 0.0);
        let violet = ui("violet");
        assert!(violet.ink.scrim.r < 0.5, "violet is a dark field");
        let dark_side = apply(&recede_matrix(&violet, 1.0), card);

        let sky = ui("sky");
        assert!(sky.ink.scrim.r > 0.5, "mint is a pale field");
        let pale_side = apply(&recede_matrix(&sky, 1.0), card);

        assert!(
            lum(dark_side) < lum(card),
            "on a dark field a receded card sinks"
        );
        assert!(
            lum(pale_side) > lum(card),
            "on a pale field it must LIFT, not sink: {pale_side:?}"
        );
        // Saturation has no handedness: both poles drain colour.
        let spread = |v: [f32; 4]| v[0].max(v[1]).max(v[2]) - v[0].min(v[1]).min(v[2]);
        assert!(spread(dark_side) < spread(card));
        assert!(spread(pale_side) < spread(card));
    }

    /// Recede must not clamp. The other tests are relative and never feed a dark
    /// input. `apply` omits Skia's clamp, so a channel below zero is the clip.
    #[test]
    fn a_dark_card_face_survives_a_full_recede() {
        // Coverless card at the quieter placeholder tint: the darkest face the shelf has.
        for p in crate::palette::PALETTES.iter().filter(|p| !p.light) {
            let ui = Frame::new(Ink::of(p), false, false, 0.0);
            let f = ui.card_face(0.20);
            let out = apply(&recede_matrix(&ui, 1.0), [f.r, f.g, f.b, 1.0]);
            for c in &out[..3] {
                assert!(
                    *c > 0.05,
                    "the recede crushed {}'s card face to black: {out:?}",
                    p.id
                );
            }
        }
    }

    /// A panel is a hairline round a bare field: nothing fills it, the edge draws.
    #[test]
    fn a_panel_is_a_line_round_the_field() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((300, 200)).unwrap();
        let rect = Rect::from_xywh(20.0, 20.0, 260.0, 160.0);
        surface.canvas().clear(skia_safe::Color::BLACK);
        let ui = Frame::default();
        panel(
            surface.canvas(),
            &ui,
            rect,
            22.0,
            None,
            PanelStroke::Edge,
            1.5,
        );
        let px = surface.peek_pixels().unwrap();
        assert_eq!(
            px.get_color((150, 100)),
            skia_safe::Color::BLACK,
            "the field shows through"
        );
        assert_ne!(
            px.get_color((150, 20)),
            skia_safe::Color::BLACK,
            "the edge draws"
        );
    }
}
