//! The stream's HUD over the video: the stats panel, the corner pills and the bottom pill.
//! Each draw returns the pixels it can touch, so the presenter blends only those
//! (`OverlayFrame::rects`). Platform-free so the Mac tests prove that reach on a raster.

use crate::theme::{fill, Fonts, W};
use punktfunk_core::hud::{HudCorner, HudLine, Role};
use skia_safe::{Canvas, Color4f, IRect, Point, RRect, Rect, RoundOut};

/// Chrome metrics in px at 100 % (96 dpi). Multiply by the display scale: the overlay
/// is 1:1 in physical pixels, so an unscaled 14 px OSD is half-size on a 200 % panel.
pub(crate) mod base {
    /// OSD/hint size; scale is a multiplier on this.
    pub const FONT_PX: f32 = 14.0;
    pub const OSD_MARGIN: f32 = 12.0;
    pub const OSD_PAD_X: f32 = 10.0;
    pub const OSD_PAD_Y: f32 = 8.0;
    pub const OSD_RADIUS: f32 = 8.0;
    pub const PILL_PAD_X: f32 = 14.0;
    pub const PILL_PAD_Y: f32 = 8.0;
    pub const PILL_BOTTOM: f32 = 24.0;
}

/// The HUD of one render: everything the stream chrome draws but the resize scrim, the exit
/// banner and the ring.
pub(crate) struct Hud<'a> {
    pub stats: Option<&'a [HudLine]>,
    pub stats_corner: HudCorner,
    /// The stats panel's size on top of the display scale.
    pub stats_scale: f32,
    pub mic_notice: Option<bool>,
    /// The speakers' mute sentence; empty while none shows.
    pub audio_mute: &'a str,
    pub access: Option<&'a str>,
    /// The bottom pill: the access toast, else the capture hint.
    pub pill: Option<&'a str>,
}

/// Draw `hud` on a `width`×`height` canvas at display `scale`. Returns one rect per element
/// drawn, each holding every pixel that element touched; the rest of the canvas is untouched.
pub(crate) fn draw_hud(
    canvas: &Canvas,
    fonts: &Fonts,
    hud: &Hud,
    (width, height): (u32, u32),
    scale: f32,
) -> Vec<IRect> {
    let mut drawn = Vec::new();
    if let Some(stats) = hud.stats {
        let osd_scale = scale * hud.stats_scale.clamp(0.5, 4.0);
        let size = (width, height);
        let corner = hud.stats_corner;
        drawn.push(draw_osd_panel(
            canvas, fonts, stats, size, osd_scale, corner,
        ));
    }
    // Top-right, stacked in a fixed order, and top-left while the stats panel holds the
    // top right: never under the panel or the bottom pill, even at stats Off. A host
    // mute stands; the player's own switches say so for a moment and go.
    let left = hud.stats.is_some() && hud.stats_corner == HudCorner::TopRight;
    let mut pill = |text: &str, dot: bool, row: usize| {
        drawn.push(corner_pill(
            canvas,
            fonts,
            text,
            dot,
            (width, row, left),
            scale,
        ));
    };
    let mut row = 0;
    if let Some(muted) = hud.mic_notice {
        pill(punktfunk_core::audio::mic_mute_label(muted), muted, row);
        row += 1;
    }
    if !hud.audio_mute.is_empty() {
        pill(hud.audio_mute, true, row);
        row += 1;
    }
    // The access chip (preset label + countdown) stacks under them. A full-control
    // permanent session has none.
    if let Some(access) = hud.access {
        pill(access, false, row);
    }
    if let Some(text) = hud.pill {
        let at = draw_hint_pill(canvas, fonts, text, width, height, 1.0, scale);
        drawn.push(at);
    }
    drawn
}

/// The pixels an anti-aliased fill of `r` can touch: `r` rounded out, plus 2 px. Skia's GPU
/// analytic AA reaches half a pixel past the edge; the second pixel is slack.
fn reach(r: Rect) -> IRect {
    RoundOut::<IRect>::round_out(&r).with_outset((2, 2))
}

/// Chrome text size at `scale`, px. Chrome draws in Medium; a line is laid out on Geist's
/// metrics, and a fallback run sits on Geist's baseline.
pub(crate) fn chrome_px(scale: f32) -> f64 {
    f64::from(base::FONT_PX * scale)
}

/// [`Fonts::measure`] for chrome text at `scale`.
pub(crate) fn chrome_w(fonts: &Fonts, text: &str, scale: f32) -> f32 {
    fonts.measure(text, W::Medium, chrome_px(scale))
}

/// Shrink `scale` until `width_at_scale` (linear in scale) fits `budget`. DPI-up
/// is only an improvement while the result still fits: the capture hint is ~150
/// chars and already fills a 1280 px window at 100 %.
fn fit_scale(scale: f32, width_at_scale: f32, budget: f32) -> f32 {
    if width_at_scale > budget && width_at_scale > 0.0 {
        (scale * budget / width_at_scale).max(0.1)
    } else {
        scale
    }
}

/// Stats OSD: translucent rounded panel in `corner`, one line each, painted by role. Returns
/// the panel's [`reach`]; the lines sit inside its padding.
fn draw_osd_panel(
    canvas: &Canvas,
    fonts: &Fonts,
    lines: &[HudLine],
    (width, height): (u32, u32),
    scale: f32,
    corner: HudCorner,
) -> IRect {
    // Width is linear in scale; measure once, then fit so Detailed-tier lines stay in-window.
    let widest_at = |s: f32| {
        (lines.iter())
            .map(|l| chrome_w(fonts, &l.text, s))
            .fold(0.0f32, f32::max)
    };
    let width_at = |s: f32| widest_at(s) + 2.0 * (base::OSD_PAD_X + base::OSD_MARGIN) * s;
    let scale = fit_scale(scale, width_at(scale), width as f32);

    let (_, metrics) = fonts.font(W::Medium, chrome_px(scale)).metrics();
    let line_h = metrics.descent - metrics.ascent + metrics.leading;
    let widest = widest_at(scale);
    let (pad_x, pad_y) = (base::OSD_PAD_X * scale, base::OSD_PAD_Y * scale);
    let (w, h) = (
        widest + 2.0 * pad_x,
        line_h * lines.len() as f32 + 2.0 * pad_y,
    );
    let margin = base::OSD_MARGIN * scale;
    let x = if corner.right() {
        width as f32 - w - margin
    } else {
        margin
    };
    let y = if corner.bottom() {
        height as f32 - h - margin
    } else {
        margin
    };
    let panel = Rect::from_xywh(x, y, w, h);
    let radius = base::OSD_RADIUS * scale;
    canvas.draw_rrect(
        RRect::new_rect_xy(panel, radius, radius),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
    );
    for (i, line) in lines.iter().enumerate() {
        fonts.draw(
            canvas,
            &line.text,
            f64::from(x + pad_x),
            f64::from(y + pad_y - metrics.ascent + line_h * i as f32),
            W::Medium,
            chrome_px(scale),
            role_color(line.role),
        );
    }
    reach(panel)
}

/// Headline bright, breakdowns softer, asides dimmer, warnings in status red.
fn role_color(role: Role) -> Color4f {
    match role {
        Role::Primary => Color4f::new(1.0, 1.0, 1.0, 0.92),
        Role::Detail => Color4f::new(1.0, 1.0, 1.0, 0.78),
        Role::Muted => Color4f::new(1.0, 1.0, 1.0, 0.6),
        Role::Warn => crate::theme::ERROR,
    }
}

/// A standing pill in a top corner, `row` pills down: words, led by an error-colour dot when
/// `dot`. `at` is the window width, the row and whether it sits top-left. Drawn from state,
/// not from the stats text, so it survives stats Off. Words, not a mute glyph Geist lacks.
/// Every pill is one line tall, so rows stack on one pitch. Returns the pill's [`reach`].
fn corner_pill(
    canvas: &Canvas,
    fonts: &Fonts,
    text: &str,
    dot: bool,
    (width, row, left): (u32, usize, bool),
    scale: f32,
) -> IRect {
    // Short; it fits any stream window, so take the display scale as-is.
    let (_, metrics) = fonts.font(W::Medium, chrome_px(scale)).metrics();
    let line_h = metrics.descent - metrics.ascent;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let dot_r = 4.0 * scale;
    let lead = if dot { 2.0 * dot_r + 8.0 * scale } else { 0.0 };
    let w = chrome_w(fonts, text, scale) + lead + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let margin = base::OSD_MARGIN * scale;
    let x = if left {
        margin
    } else {
        width as f32 - w - margin
    };
    let y = margin + row as f32 * (h + 8.0 * scale);
    let pill = Rect::from_xywh(x, y, w, h);
    canvas.draw_rrect(
        RRect::new_rect_xy(pill, h / 2.0, h / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
    );
    if dot {
        canvas.draw_circle(
            Point::new(x + pad_x + dot_r, y + h / 2.0),
            dot_r,
            &fill(crate::theme::ERROR),
        );
    }
    fonts.draw(
        canvas,
        text,
        f64::from(x + pad_x + lead),
        f64::from(y + pad_y - metrics.ascent),
        W::Medium,
        chrome_px(scale),
        Color4f::new(1.0, 1.0, 1.0, 0.92),
    );
    reach(pill)
}

/// Capture hint / start banner: centered pill near the bottom. `scale` is the
/// display UI scale, a multiplier on [`base::FONT_PX`]. Returns the pill's [`reach`].
pub(crate) fn draw_hint_pill(
    canvas: &Canvas,
    fonts: &Fonts,
    text: &str,
    width: u32,
    height: u32,
    alpha: f32,
    scale: f32,
) -> IRect {
    // Capture hint already fills most of a 1280 px window at 100 %; fit to the
    // window (4 % gutter) so 2× scale does not overrun both edges.
    let pill_w = |s: f32| chrome_w(fonts, text, s) + 2.0 * base::PILL_PAD_X * s;
    let scale = fit_scale(scale, pill_w(scale), width as f32 * 0.96);

    let (_, metrics) = fonts.font(W::Medium, chrome_px(scale)).metrics();
    let line_h = metrics.descent - metrics.ascent;
    let text_w = chrome_w(fonts, text, scale);
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let w = text_w + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let x = (width as f32 - w) / 2.0;
    let y = height as f32 - h - base::PILL_BOTTOM * scale;
    let pill = Rect::from_xywh(x, y, w, h);
    canvas.draw_rrect(
        RRect::new_rect_xy(pill, h / 2.0, h / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62 * alpha)),
    );
    fonts.draw(
        canvas,
        text,
        f64::from(x + pad_x),
        f64::from(y + pad_y - metrics.ascent),
        W::Medium,
        chrome_px(scale),
        Color4f::new(1.0, 1.0, 1.0, 0.92 * alpha),
    );
    reach(pill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{test_fonts, TEST_FALLBACK};
    use punktfunk_core::hud::{format, Extra, Hdr, StatsSnapshot, StatsVerbosity, Summary};
    use skia_safe::{AlphaType, ColorType, ImageInfo};

    /// A wide window, and a narrow one where the panel and the hint fit by shrinking.
    const SIZES: [(u32, u32); 2] = [(1280, 720), (480, 270)];
    /// Display scale and stats size: the smallest offered, plain, and a 200 % panel.
    const SCALES: [(f32, f32); 3] = [(0.5, 0.75), (1.0, 1.0), (2.0, 1.5)];

    fn sum(p50_us: u32, p95_us: u32) -> Summary {
        Summary {
            n: 120,
            mean_us: p50_us,
            min_us: p50_us,
            max_us: p95_us,
            p50_us,
            p95_us,
            p99_us: p95_us,
        }
    }

    /// A desktop window with loss, every stage measured, and an extra line of accents,
    /// descenders and a fallback face's glyph.
    fn snapshot() -> StatsSnapshot {
        StatsSnapshot {
            window_ms: 1000,
            width: 3840,
            height: 2160,
            refresh_hz: 120,
            codec: "HEVC".into(),
            bit_depth: 10,
            hdr: Hdr::ToneMapped,
            decoder: "native-vulkan".into(),
            received: 120,
            bytes: 9_037_500,
            decoded: Some(120),
            presented: Some(119),
            e2e: sum(6400, 9100),
            e2e_decoded: sum(4000, 6000),
            host_net: sum(2100, 3000),
            host: sum(1200, 1800),
            net: sum(900, 1200),
            decode: sum(1800, 2500),
            display: sum(1100, 1500),
            pace: sum(300, 900),
            latch: sum(700, 1400),
            host_queue: sum(300, 400),
            host_encode: sum(500, 700),
            host_xfer: sum(100, 200),
            host_pace: sum(300, 400),
            on_glass: true,
            lost: 3,
            rtt_us: Some(2400),
            extras: vec![Extra::detail("Ǻccéntéd ŶÿĴ ǅ gjpqy \u{e58a}")],
            ..StatsSnapshot::default()
        }
    }

    /// Draws `hud` on an F16 canvas, as over a 10-bit swapchain. Fails on a pixel with any
    /// alpha outside every returned rect, and on a rect whose on-canvas part is more than
    /// 3 px wider than the ink in it.
    fn assert_covered(fonts: &Fonts, hud: &Hud, (w, h): (u32, u32), scale: f32, what: &str) {
        let (wi, hi) = (w as i32, h as i32);
        let f16 = ImageInfo::new((wi, hi), ColorType::RGBAF16, AlphaType::Premul, None);
        let mut surface = skia_safe::surfaces::raster(&f16, None, None).unwrap();
        surface.canvas().clear(Color4f::new(0.0, 0.0, 0.0, 0.0));
        let rects = draw_hud(surface.canvas(), fonts, hud, (w, h), scale);
        let f32_info = ImageInfo::new((wi, hi), ColorType::RGBAF32, AlphaType::Premul, None);
        let row = w as usize * 16;
        let mut px = vec![0u8; row * h as usize];
        assert!(surface.read_pixels(&f32_info, &mut px, row, (0, 0)));
        let inside =
            |r: &IRect, x: i32, y: i32| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        let mut ink: Vec<Option<IRect>> = vec![None; rects.len()];
        for (i, texel) in px.chunks_exact(16).enumerate() {
            let alpha = f32::from_ne_bytes(texel[12..16].try_into().unwrap());
            if alpha <= 0.0 {
                continue;
            }
            let (x, y) = ((i % w as usize) as i32, (i / w as usize) as i32);
            let at = IRect::new(x, y, x + 1, y + 1);
            let mut hit = false;
            for (r, ink) in rects.iter().zip(&mut ink) {
                if inside(r, x, y) {
                    hit = true;
                    *ink = Some(ink.map_or(at, |b| IRect::join(&b, &at)));
                }
            }
            assert!(hit, "{what}: ink at ({x}, {y}) outside {rects:?}");
        }
        for (r, ink) in rects.iter().zip(ink) {
            // A pill wider than a narrow window runs off it; the presenter clamps the same.
            let r = IRect::intersect(r, &IRect::from_wh(wi, hi)).expect("on the canvas");
            let ink = ink.unwrap_or_else(|| panic!("{what}: {r:?} holds no ink"));
            let slack = ink.with_outset((3, 3));
            assert!(
                r.left >= slack.left
                    && r.top >= slack.top
                    && r.right <= slack.right
                    && r.bottom <= slack.bottom,
                "{what}: {r:?} is wider than its ink {ink:?}"
            );
        }
    }

    /// Every stats tier in every corner, at every size and scale, in the pseudo-language.
    #[test]
    fn the_stats_panel_reports_every_pixel_it_draws() {
        let fonts = test_fonts(&[TEST_FALLBACK]);
        pf_l10n::with_language("en-XA", || {
            let snap = snapshot();
            let tiers = [
                (StatsVerbosity::Compact, false),
                (StatsVerbosity::Normal, false),
                (StatsVerbosity::Detailed, true),
            ];
            for (tier, advanced) in tiers {
                let lines = format(&snap, tier, advanced);
                for corner in HudCorner::ALL {
                    for size in SIZES {
                        for (scale, stats_scale) in SCALES {
                            let hud = Hud {
                                stats: Some(&lines),
                                stats_corner: corner,
                                stats_scale,
                                mic_notice: None,
                                audio_mute: "",
                                access: None,
                                pill: None,
                            };
                            let what = format!("{tier:?} {corner:?} {size:?} ×{scale}");
                            assert_covered(&fonts, &hud, size, scale, &what);
                        }
                    }
                }
            }
        });
    }

    /// The pills alone, with a dot and without, and beside a panel that pushes them left
    /// or shares the bottom edge with the hint.
    #[test]
    fn the_pills_report_every_pixel_they_draw() {
        let fonts = test_fonts(&[TEST_FALLBACK]);
        pf_l10n::with_language("en-XA", || {
            use pf_l10n::client as c;
            let lines = format(&snapshot(), StatsVerbosity::Normal, false);
            let leave = c::notice_exit_keys("Ctrl+Alt+Shift+Q");
            let hint = format!("{} · {leave}", c::notice_capture_click());
            let panels = [
                None,
                Some(HudCorner::TopRight),
                Some(HudCorner::BottomRight),
            ];
            for (muted, panel) in [(true, panels[0]), (false, panels[1]), (true, panels[2])] {
                for size in SIZES {
                    for (scale, _) in SCALES {
                        let hud = Hud {
                            stats: panel.map(|_| lines.as_slice()),
                            stats_corner: panel.unwrap_or(HudCorner::TopLeft),
                            stats_scale: 1.0,
                            mic_notice: Some(muted),
                            audio_mute: c::notice_audio_muted_host_and_device(),
                            access: Some(c::grant_ends_soon()),
                            pill: Some(&hint),
                        };
                        let what = format!("pills {muted} {panel:?} {size:?} ×{scale}");
                        assert_covered(&fonts, &hud, size, scale, &what);
                    }
                }
            }
        });
    }
}
