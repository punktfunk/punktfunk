//! Progressive backdrop treatment behind pinned text: what lies under a chrome band
//! softens with depth into the band, as the Apple client's Glur tray does — σ grows
//! linearly from nothing at the content edge toward full strength 60 % into the band
//! plus its overhang past the glass (Glur's `interpolation: 0.6` over an 80 pt bleed).
//!
//! Explicit surfaces, not an image-filter graph: the band is copied out once, halved
//! twice (a true box filter down to quarter size), blurred there by two separable
//! variable-σ Gaussian passes, and drawn back scaled up. Each shader samples an image
//! whose pixels are its own coordinates, so every backend places the band identically —
//! a filter graph resolved its spaces differently on GL than on raster and smeared the
//! band. Pinned by `a_band_blurs_more_toward_its_edge`.

use skia_safe::{
    runtime_effect::ChildPtr, Canvas, Color4f, Data, FilterMode, IRect, Image, Matrix, Paint, Rect,
    RuntimeEffect, SamplingOptions, Shader, Surface, TileMode,
};
use std::cell::OnceCell;

/// Downsample factor: two halvings.
const DOWN: f32 = 4.0;
/// σ under this, in quarter-size px, adds nothing over the upscale's own softness:
/// the copy fades out there and the sharp backdrop shows through.
const FADE: f32 = 0.8;
/// Full-strength block of the pixel styles, design units.
const PIXEL_BLOCK: f32 = 12.0;

/// One separable pass. `p` is a pixel of the quarter-size copy; the child's pixels are
/// the same coordinates, so a tap is exactly `p + dir·i`, clamped by the child's tiling.
const BLUR_SKSL: &str = r#"
uniform shader src;
uniform float dirx;
uniform float diry;
uniform float edge;
uniform float clear;
uniform float sigma;
uniform float fade;
uniform float full;

half4 main(float2 p) {
    float t = clamp((clear - p.y) / (clear - edge), 0.0, 1.0);
    float s = sigma * min(t / full, 1.0);
    half4 acc = src.eval(p);
    if (s >= 0.35) {
        float reach = min(ceil(3.0 * s), 15.0);
        acc = half4(0.0);
        float wsum = 0.0;
        for (int i = -15; i <= 15; i++) {
            float fi = float(i);
            if (abs(fi) <= reach) {
                float w = exp(-fi * fi / (2.0 * s * s));
                acc += src.eval(p + float2(dirx, diry) * fi) * half(w);
                wsum += w;
            }
        }
        acc /= half(wsum);
    }
    return acc * half(fade > 0.0 ? smoothstep(0.1, fade, s) : 1.0);
}
"#;

/// The pixel styles: the band snaps to blocks instead of blurring — one read a pixel,
/// straight onto the canvas. Stepped, blocks grow in power-of-two tiers toward the edge;
/// flat, one block size covers the band.
const PIXEL_SKSL: &str = r#"
uniform shader src;
uniform float edge;
uniform float clear;
uniform float block;
uniform float stepped;
uniform float full;

half4 main(float2 p) {
    float t = clamp((clear - p.y) / (clear - edge), 0.0, 1.0);
    float b = stepped > 0.5 ? min(t / full, 1.0) * block : (t <= 0.0 ? 0.0 : block);
    if (b <= 1.0) {
        return src.eval(p);
    }
    if (stepped > 0.5) {
        b = exp2(ceil(log2(b)));
    }
    return src.eval((floor(p / b) + 0.5) * b);
}
"#;

thread_local! {
    static BLUR: OnceCell<Option<RuntimeEffect>> = const { OnceCell::new() };
    static PIXEL: OnceCell<Option<RuntimeEffect>> = const { OnceCell::new() };
    static OVERRIDE: std::cell::Cell<Option<Style>> = const { std::cell::Cell::new(None) };
    /// The shell is painting a screen into a transition layer. A copy of the surface
    /// cannot see that layer, so a band drawn inside it would blur the field behind.
    static IN_LAYER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn blur_effect() -> Option<RuntimeEffect> {
    BLUR.with(|e| {
        e.get_or_init(|| RuntimeEffect::make_for_shader(BLUR_SKSL, None).ok())
            .clone()
    })
}

fn pixel_effect() -> Option<RuntimeEffect> {
    PIXEL.with(|e| {
        e.get_or_init(|| RuntimeEffect::make_for_shader(PIXEL_SKSL, None).ok())
            .clone()
    })
}

/// How a backdrop softens what lies under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Blur,
    /// Blocks growing toward the edge in power-of-two tiers.
    Pixel,
    /// One block size across the band.
    PixelFlat,
    Off,
}

/// A/B while the TV's look is chosen: a style for every backdrop this thread draws,
/// `None` for the default. EXPERIMENT, not for commit.
pub fn set_style_override(style: Option<Style>) {
    OVERRIDE.with(|o| o.set(style));
}

/// Where the band is and how hard it blurs, device px.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    /// The screen-side edge: full strength here.
    pub edge: f32,
    /// The content-side edge: untouched here.
    pub clear: f32,
    /// Full-strength σ.
    pub sigma: f32,
    /// How far past `edge` the band notionally runs on. Glur's tray bleeds 80 pt past
    /// the screen, so its ramp reaches full strength 60 % into that longer span and a
    /// visible band never quite gets there: a gentler rise than a ramp cut at the glass.
    pub over: f32,
}

impl Band {
    /// The depth, as a fraction of the visible band, where σ peaks.
    fn full(self) -> f32 {
        let depth = (self.clear - self.edge).abs().max(1.0);
        0.6 * (depth + self.over) / depth
    }
}

/// The style this frame draws: the blur everywhere, the reduced interface included — the
/// pixel treatment stays behind the A/B override until it is wanted.
fn style() -> Style {
    OVERRIDE.with(std::cell::Cell::get).unwrap_or(Style::Blur)
}

/// Whether a backdrop draws anything this frame: content may run on under the chrome
/// only when a band will treat it.
pub fn active() -> bool {
    style() != Style::Off
}

/// Set while the shell paints a screen into a layer; bands inside it are skipped.
pub fn set_in_layer(on: bool) {
    IN_LAYER.with(|c| c.set(on));
}

/// Soften what `canvas` already holds under `rect` by `band`. Inside a transition layer
/// the surface holds none of the screen, so the band is left off for those frames.
pub fn backdrop(canvas: &Canvas, rect: Rect, band: Band) {
    if IN_LAYER.with(std::cell::Cell::get) {
        return;
    }
    match style() {
        Style::Off => {}
        Style::Blur => blur(canvas, rect, band),
        Style::Pixel => pixel(canvas, rect, band, true),
        Style::PixelFlat => pixel(canvas, rect, band, false),
    }
}

/// The band copied out of the canvas's surface: the image (device px), the device rect
/// it covers, the local rect to draw the treated copy back over, and the device scale.
fn snap(canvas: &Canvas, rect: Rect) -> Option<(Image, IRect, Rect, f32)> {
    let m = canvas.local_to_device_as_3x3();
    // The UI never rotates a tray; a transition only scales about the centre.
    if !m.is_scale_translate() {
        return None;
    }
    let (dev, _) = m.map_rect(rect);
    let dims = canvas.base_layer_size();
    let outer: IRect = skia_safe::RoundOut::round_out(&dev);
    let dev = IRect::intersect(&outer, &IRect::from_size((dims.width, dims.height)))?;
    // SAFETY: the surface handle is used within this frame only, while the canvas —
    // owned by that surface — is borrowed by every caller up the stack.
    let mut surface = unsafe { canvas.surface() }?;
    let image = surface.image_snapshot_with_bounds(dev)?;
    let inv = m.invert()?;
    let (dst, _) = inv.map_rect(Rect::from_irect(dev));
    Some((image, dev, dst, m.scale_y()))
}

/// An offscreen on `canvas`'s own backend and colour depth, so a 10-bit canvas keeps its
/// gradients through the pass. 10/10/10/2 becomes F16: the band's alpha ramp needs more
/// than two bits. A GPU target under a GPU canvas, raster under a raster one.
pub(crate) fn offscreen(canvas: &Canvas, w: i32, h: i32) -> Option<Surface> {
    use skia_safe::ColorType::{BGRA1010102, RGBA1010102, RGBAF16};
    let color_type = match canvas.image_info().color_type() {
        RGBA1010102 | BGRA1010102 => RGBAF16,
        other => other,
    };
    let info = skia_safe::ImageInfo::new(
        (w.max(1), h.max(1)),
        color_type,
        skia_safe::AlphaType::Premul,
        None,
    );
    canvas
        .new_surface(&info, None)
        .or_else(|| skia_safe::surfaces::raster(&info, None, None))
}

/// `image` drawn into a fresh `w`×`h` offscreen with linear sampling.
fn scaled(canvas: &Canvas, image: &Image, w: i32, h: i32) -> Option<Image> {
    let mut s = offscreen(canvas, w, h)?;
    s.canvas().draw_image_rect_with_sampling_options(
        image,
        None,
        Rect::from_wh(w as f32, h as f32),
        SamplingOptions::from(FilterMode::Linear),
        &carrier(),
    );
    Some(s.image_snapshot())
}

/// The one paint every draw here uses: an opaque carrier for an image or a shader.
fn carrier() -> Paint {
    crate::theme::fill(Color4f::new(1.0, 1.0, 1.0, 1.0))
}

fn image_shader(image: &Image, filter: FilterMode) -> Option<Shader> {
    image.to_shader(
        (TileMode::Clamp, TileMode::Clamp),
        SamplingOptions::from(filter),
        None,
    )
}

fn uniforms(values: &[f32]) -> Data {
    let words: Vec<[u8; 4]> = values.iter().map(|v| v.to_ne_bytes()).collect();
    Data::new_copy(words.as_flattened())
}

/// One blur pass over a quarter-size copy, back as a fresh image of the same size.
#[allow(clippy::too_many_arguments)]
fn blur_pass(
    canvas: &Canvas,
    src: &Image,
    (w, h): (i32, i32),
    dir: (f32, f32),
    (edge, clear): (f32, f32),
    sigma: f32,
    fade: f32,
    full: f32,
) -> Option<Image> {
    let effect = blur_effect()?;
    let child = ChildPtr::Shader(image_shader(src, FilterMode::Nearest)?);
    let data = uniforms(&[dir.0, dir.1, edge, clear, sigma, fade, full]);
    let shader = effect.make_shader(data, &[child], None)?;
    let mut s = offscreen(canvas, w, h)?;
    let mut paint = carrier();
    paint.set_shader(shader);
    s.canvas()
        .draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
    Some(s.image_snapshot())
}

fn blur(canvas: &Canvas, rect: Rect, band: Band) {
    let Some((image, dev, dst, scale)) = snap(canvas, rect) else {
        return;
    };
    let (w, h) = (image.width(), image.height());
    let q = (
        (w as f32 / DOWN).ceil() as i32,
        (h as f32 / DOWN).ceil() as i32,
    );
    let Some(half) = scaled(canvas, &image, (w + 1) / 2, (h + 1) / 2) else {
        return;
    };
    let Some(small) = scaled(canvas, &half, q.0, q.1) else {
        return;
    };
    // The band's lines in the copy's own pixels.
    let m = canvas.local_to_device_as_3x3();
    let to_q = |y: f32| (m.map_point((0.0, y)).y - dev.top as f32) / DOWN;
    let lines = (to_q(band.edge), to_q(band.clear));
    let sigma = band.sigma * scale / DOWN;
    let full = band.full();
    let Some(x) = blur_pass(canvas, &small, q, (1.0, 0.0), lines, sigma, 0.0, full) else {
        return;
    };
    let Some(y) = blur_pass(canvas, &x, q, (0.0, 1.0), lines, sigma, FADE, full) else {
        return;
    };
    canvas.draw_image_rect_with_sampling_options(
        &y,
        None,
        dst,
        SamplingOptions::from(FilterMode::Linear),
        &carrier(),
    );
}

/// A pixel style straight onto the canvas: local coordinates map 1:1 onto the copy's
/// pixels through the shader's matrix, so nothing depends on the backend's spaces.
fn pixel(canvas: &Canvas, rect: Rect, band: Band, stepped: bool) {
    let Some((image, dev, dst, scale)) = snap(canvas, rect) else {
        return;
    };
    let Some(effect) = pixel_effect() else {
        return;
    };
    let Some(src) = image_shader(&image, FilterMode::Nearest) else {
        return;
    };
    let m = canvas.local_to_device_as_3x3();
    let to_img = |y: f32| m.map_point((0.0, y)).y - dev.top as f32;
    let block = PIXEL_BLOCK * (band.sigma / 14.0) * scale;
    let data = uniforms(&[
        to_img(band.edge),
        to_img(band.clear),
        block.max(2.0),
        f32::from(u8::from(stepped)),
        band.full(),
    ]);
    // Shader space is the copy's pixels; the matrix lays them over `dst`.
    let mut local = Matrix::translate((dst.left, dst.top));
    local.pre_scale((1.0 / scale, 1.0 / scale), None);
    let Some(shader) = effect.make_shader(data, &[ChildPtr::Shader(src)], Some(&local)) else {
        return;
    };
    let mut paint = carrier();
    paint.set_shader(shader);
    canvas.draw_rect(dst, &paint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::Color;

    /// 8 px stripes under a top band: rows at the clear edge keep full contrast, rows at
    /// the screen edge lose nearly all of it, and contrast falls steadily in between.
    #[test]
    fn a_band_blurs_more_toward_its_edge() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((64, 100)).unwrap();
        let canvas = surface.canvas();
        canvas.clear(Color::BLACK);
        let white = crate::theme::fill(skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0));
        for x in (0..64).step_by(16) {
            canvas.draw_rect(Rect::from_xywh(x as f32, 0.0, 8.0, 100.0), &white);
        }
        let band = Band {
            edge: 0.0,
            clear: 80.0,
            sigma: 8.0,
            over: 0.0,
        };
        backdrop(canvas, Rect::from_xywh(0.0, 0.0, 64.0, 100.0), band);
        let px = surface.image_snapshot();
        let info = px.image_info();
        let mut bytes = vec![0u8; info.compute_min_byte_size()];
        assert!(px.read_pixels(
            info,
            &mut bytes,
            info.min_row_bytes(),
            (0, 0),
            skia_safe::image::CachingHint::Allow
        ));
        let contrast = |y: usize| {
            let row = &bytes[y * info.min_row_bytes()..];
            let lum = |x: usize| i32::from(row[x * 4 + 1]);
            (lum(20) - lum(28)).abs()
        };
        assert_eq!(contrast(90), 255, "below the band nothing moves");
        assert_eq!(contrast(79), 255, "the clear edge is untouched");
        assert!(
            contrast(5) < 10,
            "the screen edge is fully blurred: {}",
            contrast(5)
        );
        let ramp: Vec<i32> = [74, 68, 62, 56, 50].into_iter().map(contrast).collect();
        assert!(
            ramp.windows(2).all(|w| w[0] >= w[1]),
            "contrast falls into the band: {ramp:?}"
        );
        assert!(ramp[0] > ramp[4], "the fall is real, not flat: {ramp:?}");
    }

    /// The pixel styles snap the band to blocks without touching the clear edge, and
    /// the treatment stays inside the band.
    #[test]
    fn the_pixel_styles_stay_inside_the_band() {
        for style in [Style::Pixel, Style::PixelFlat] {
            let mut surface = skia_safe::surfaces::raster_n32_premul((64, 100)).unwrap();
            let canvas = surface.canvas();
            canvas.clear(Color::BLACK);
            let white = crate::theme::fill(skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0));
            for x in (0..64).step_by(4) {
                canvas.draw_rect(Rect::from_xywh(x as f32, 0.0, 2.0, 100.0), &white);
            }
            set_style_override(Some(style));
            let band = Band {
                edge: 0.0,
                clear: 80.0,
                sigma: 14.0,
                over: 0.0,
            };
            backdrop(canvas, Rect::from_xywh(0.0, 0.0, 64.0, 80.0), band);
            set_style_override(None);
            let px = surface.image_snapshot();
            let info = px.image_info();
            let mut bytes = vec![0u8; info.compute_min_byte_size()];
            assert!(px.read_pixels(
                info,
                &mut bytes,
                info.min_row_bytes(),
                (0, 0),
                skia_safe::image::CachingHint::Allow
            ));
            let lum = |x: usize, y: usize| i32::from(bytes[y * info.min_row_bytes() + x * 4 + 1]);
            let period = |y: usize| (0..32).any(|x| (lum(x, y) - lum(x + 2, y)).abs() > 100);
            assert!(period(90), "below the band the stripes stand");
            // Flat covers its whole band by design; only stepped clears its content edge.
            if style == Style::Pixel {
                assert!(period(79), "the clear edge is untouched");
            }
            assert!(
                !period(5),
                "at the screen edge 2 px stripes vanish into blocks ({style:?})"
            );
        }
    }
}
