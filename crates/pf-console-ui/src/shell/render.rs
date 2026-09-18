//! Per-frame screen compose and transition.

use crate::anim::approach;
use crate::glyphs::{hint_bar, GlyphStyle};
use crate::library::LibraryShared;
use crate::model::HostRow;
use crate::screens::{Bg, Ctx, Screen};
use crate::theme::{fg, Fonts, PanelStroke, EDGE_INSET, W};
use pf_client_core::menu_nav::PadInfo;
use pf_client_core::trust;
use skia_safe::{Canvas, Rect};
use std::time::Instant;

use super::{
    Motion, NavKind, Shell, BOTTOM_BAND, NAV_ENTER_SCALE, NAV_EXIT_SCALE, NAV_REVEAL_ALPHA,
    NAV_SLIDE_DP, TOP_BAND,
};

impl Shell {
    #[allow(clippy::too_many_arguments)]
    /// Test helper: no insets, default scale. Hosts call [`Self::render_in`].
    #[cfg(test)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        fonts: &Fonts,
        pad: Option<&str>,
        pad_pref: Option<punktfunk_core::config::GamepadPref>,
        pads: &[PadInfo],
    ) {
        self.render_in(
            canvas,
            &crate::console::Viewport::plain(width, height),
            fonts,
            pad,
            pad_pref,
            pads,
        );
    }

    /// One frame. Backdrop paints the whole surface; chrome and content lay out
    /// inside the insets by one canvas translate — screens never see insets.
    /// `k` is `viewport.scale`, else the couch formula on full height.
    pub(crate) fn render_in(
        &mut self,
        canvas: &Canvas,
        viewport: &crate::console::Viewport,
        fonts: &Fonts,
        pad: Option<&str>,
        pad_pref: Option<punktfunk_core::config::GamepadPref>,
        pads: &[PadInfo],
    ) {
        let now = Instant::now();
        let dt = self
            .last_frame
            .replace(now)
            .map_or(1.0 / 60.0, |t| (now - t).as_secs_f64().clamp(0.0, 0.05));
        #[cfg(test)]
        let dt = match self.fake_clock.as_mut() {
            Some((t, step)) => {
                *t += *step;
                *step
            }
            None => dt,
        };
        // Shaped-paragraph cache clock, before anything draws.
        fonts.begin_frame();
        self.sync();
        // Publish ink before any draw. Widgets read `theme::set_ink`; skipping this
        // paints the previous palette's text on the new field.
        crate::theme::set_ink(self.ink);
        // Same publish-once contract as ink. Also a local: `LayerEnv` mut-borrows
        // `settings`, so the transition arms cannot read the field.
        let reduce = self.settings.reduce_motion;
        crate::theme::set_reduce_motion(reduce);
        self.pads = pads.to_vec();
        self.glyphs = glyph_style(self.input_source, pad_pref, self.platform);
        // Rebuild the chip string only when it changes. `pads` is left alone — a
        // handful of small structs; `PadInfo` has no `PartialEq` in its crate.
        let chip = pad.unwrap_or(if self.glyphs == GlyphStyle::Remote {
            "TV remote — a controller works too"
        } else {
            "No controller — keyboard works too"
        });
        if self.chip.as_deref() != Some(chip) {
            self.chip = Some(chip.to_owned());
        }

        let (full_w, full_h) = (f64::from(viewport.width), f64::from(viewport.height));
        let ins = viewport.insets;
        // Scale from FULL height even under insets: a landscape cutout is a side
        // inset and must not shrink type.
        let k = viewport
            .scale
            .unwrap_or_else(|| (full_h / 800.0).clamp(0.75, 3.0));
        // Layout origin is the safe-area top-left. Pointers enter the same space
        // via `last_insets` in `Shell::pointer`.
        let (w, h) = (
            full_w - f64::from(ins.left) - f64::from(ins.right),
            full_h - f64::from(ins.top) - f64::from(ins.bottom),
        );
        self.last_insets = (ins.left, ins.top);
        self.last_full = (full_w as f32, full_h as f32);
        self.last_k = k;
        let t = self.t();

        // `None` is settled (`Motion::None`). A reversed push pops its screen here;
        // a completed pop drops the one it was carrying.
        let motion_p = self.advance_nav(dt);

        // One shader pass: form screens quiet the same field via a chased `calm`
        // uniform. Not a second backdrop.
        let bg_target = match self.stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
        self.bg_mix = approach(self.bg_mix, bg_target, dt, 0.12);
        if (self.bg_mix - bg_target).abs() < 0.005 {
            self.bg_mix = bg_target;
        }
        self.draw_aurora(canvas, full_w, full_h, t, self.bg_mix);
        // Translate only when inset: with none this is the desktop canvas, and
        // screenshot dumps stay byte-identical.
        let inset = ins.left != 0.0 || ins.top != 0.0;
        if inset {
            canvas.save();
            canvas.translate((ins.left, ins.top));
        }

        let content = Rect::from_ltrb(
            0.0,
            (TOP_BAND * k) as f32,
            w as f32,
            (h - BOTTOM_BAND * k) as f32,
        );
        // Heading budget left of the controller chip. 12 dp is the gap between them;
        // the 0.35 w floor stops a long chip from squeezing the title to nothing.
        let title_max_w = {
            let chip_w = self.chip.as_ref().map_or(0.0, |c| {
                chip_width(
                    fonts,
                    c,
                    self.pads.first().is_some_and(|p| p.battery.is_some()),
                    k,
                )
            });
            (w - 2.0 * EDGE_INSET * k - chip_w - 12.0 * k).max(w * 0.35)
        };
        let mut env = LayerEnv {
            canvas,
            w,
            h,
            content,
            k,
            title_max_w,
            dt,
            fonts,
            hosts: &self.hosts,
            library: &self.library,
            settings: &mut self.settings,
            store: &*self.store,
            platform: self.platform,
            screen: self.screen,
            pads: &self.pads,
            deck: self.deck,
            fallback_ui: self.fallback_ui,
            pyrowave_ok: self.pyrowave_ok,
            av1_ok: self.av1_ok,
            device_name: &self.device_name,
            t,
            glyphs: self.glyphs,
            // A modal owns B/A while up — do not also show the screen's legend.
            show_hints: self.connecting.is_none()
                && self.launching.is_none()
                && self.wake.is_none(),
        };
        // Only a settled top screen publishes hint hit-boxes. Mid-transition every
        // layer is slid inside a `save_layer`, so reported rects are not the pixels.
        self.hint_rects.clear();
        // Reduced motion keeps the crossfade (an instant swap loses the only spatial
        // cue) and drops slide/scale.
        let slide = |dy: f64| if reduce { 0.0 } else { dy };
        let zoom = |s: f64| if reduce { 1.0 } else { s };
        match (&mut self.motion, motion_p) {
            (
                Motion::Nav {
                    kind: NavKind::Push,
                    leaving,
                    ..
                },
                Some(p),
            ) => {
                let n = self.stack.len();
                let enter_scale = zoom(NAV_ENTER_SCALE + (1.0 - NAV_ENTER_SCALE) * p);
                let enter_slide = slide(NAV_SLIDE_DP * k * (1.0 - p));
                let recede = zoom(1.0 - (1.0 - NAV_EXIT_SCALE) * p);
                if let Some(replaced) = leaving.as_mut() {
                    // REPLACE paints the swapped-out screen. Painting stack n-2 recedes its
                    // parent, so "Edit…" would flash the host list under the incoming editor.
                    env.paint(replaced.as_mut(), 1.0 - p, 0.0, recede);
                    env.paint(&mut self.stack[n - 1], p, enter_slide, enter_scale);
                } else if n >= 2 {
                    let (below, top) = self.stack.split_at_mut(n - 1);
                    env.paint(&mut below[n - 2], 1.0 - p, 0.0, recede);
                    env.paint(&mut top[0], p, enter_slide, enter_scale);
                } else {
                    env.paint(&mut self.stack[0], p, enter_slide, enter_scale);
                }
            }
            (
                Motion::Nav {
                    kind: NavKind::Pop,
                    leaving: Some(leaving),
                    ..
                },
                Some(p),
            ) => {
                let n = self.stack.len();
                env.paint(
                    &mut self.stack[n - 1],
                    NAV_REVEAL_ALPHA + (1.0 - NAV_REVEAL_ALPHA) * p,
                    0.0,
                    zoom(NAV_EXIT_SCALE + (1.0 - NAV_EXIT_SCALE) * p),
                );
                env.paint(leaving.as_mut(), 1.0 - p, slide(NAV_SLIDE_DP * k * p), 1.0);
            }
            _ => {
                let n = self.stack.len();
                self.hint_rects = env.paint(&mut self.stack[n - 1], 1.0, 0.0, 1.0);
            }
        }

        if let Some(chip) = &self.chip {
            let size = 12.0 * k;
            let tw = f64::from(fonts.measure(chip, W::Medium, size));
            let (bh, pad_x, gap) = (24.0 * k, 12.0 * k, 8.0 * k);
            let mark_w = 15.0 * k;
            let battery = self.pads.first().and_then(|p| p.battery);
            let bw = chip_width(fonts, chip, battery.is_some(), k);
            let bx = w - EDGE_INSET * k - bw;
            let top = 18.0 * k;
            let rect = Rect::from_xywh(bx as f32, top as f32, bw as f32, bh as f32);
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.12),
                k as f32,
            );
            let cy = top + bh / 2.0;
            crate::glyphs::pad_mark(canvas, self.glyphs, bx + pad_x, cy, mark_w, k, fg(0.7));
            fonts.draw(
                canvas,
                chip,
                bx + pad_x + mark_w + gap,
                cy + size * 0.36,
                W::Medium,
                size,
                fg(0.7),
            );
            if let Some(b) = battery {
                crate::glyphs::battery_pip(
                    canvas,
                    bx + pad_x + mark_w + gap + tw + gap,
                    cy,
                    22.0 * k,
                    k,
                    b,
                );
            }
        }

        self.draw_overlays(canvas, w, h, k, dt, t, fonts);
        if inset {
            canvas.restore();
        }
    }
}

/// Chip width in device px. Shared with the heading, which stops short of
/// it — a second copy of this arithmetic puts the title under the chip the
/// day a field is added. The pip takes room only when a charge exists.
fn chip_width(fonts: &Fonts, chip: &str, has_battery: bool, k: f64) -> f64 {
    let tw = f64::from(fonts.measure(chip, W::Medium, 12.0 * k));
    let (pad_x, gap, mark_w) = (12.0 * k, 8.0 * k, 15.0 * k);
    let pip_w = if has_battery { 22.0 * k + gap } else { 0.0 };
    pad_x + mark_w + gap + tw + pip_w + pad_x
}

/// One screen layer's paint args, so each `paint` borrows `Shell` fields disjointly.
struct LayerEnv<'a> {
    canvas: &'a Canvas,
    w: f64,
    h: f64,
    content: Rect,
    k: f64,
    title_max_w: f64,
    dt: f64,
    fonts: &'a Fonts,
    hosts: &'a [HostRow],
    library: &'a LibraryShared,
    settings: &'a mut trust::Settings,
    store: &'a dyn crate::store::SettingsStore,
    platform: crate::platform::Platform,
    screen: Option<crate::shell::DeviceScreen>,
    pads: &'a [PadInfo],
    deck: bool,
    fallback_ui: bool,
    pyrowave_ok: bool,
    av1_ok: bool,
    device_name: &'a str,
    t: f64,
    glyphs: GlyphStyle,
    show_hints: bool,
}

impl LayerEnv<'_> {
    /// One screen as a unit: fade, vertical slide, scale about centre. Title and
    /// hint bar ride inside the layer so chrome travels with content. Hit-boxes
    /// are only worth keeping on a settled top screen (see `Shell::render`).
    fn paint(
        &mut self,
        screen: &mut Screen,
        alpha: f64,
        dy: f64,
        scale: f64,
    ) -> Vec<(crate::glyphs::HintKey, Rect)> {
        let canvas = self.canvas;
        // Raise a layer only when alpha/scale/slide actually change. Unbounded
        // `save_layer` is a full-surface offscreen; Skia does not elide alpha ≥ 1.
        // Settled SrcOver draws are pixel-identical without the isolation.
        let layered = alpha < 0.999 || (scale - 1.0).abs() > 0.001 || dy.abs() > 0.001;
        if layered {
            canvas.save_layer_alpha_f(None, alpha.clamp(0.0, 1.0) as f32);
        } else {
            // Save anyway: the transform below is undone by the same `restore`.
            canvas.save();
        }
        canvas.translate((0.0, dy as f32));
        let (cx, cy) = ((self.w / 2.0) as f32, (self.h / 2.0) as f32);
        canvas.translate((cx, cy));
        canvas.scale((scale as f32, scale as f32));
        canvas.translate((-cx, -cy));

        let mut ctx = Ctx {
            hosts: self.hosts,
            library: self.library,
            settings: self.settings,
            store: self.store,
            platform: self.platform,
            screen: self.screen,
            pads: self.pads,
            deck: self.deck,
            fallback_ui: self.fallback_ui,
            pyrowave_ok: self.pyrowave_ok,
            av1_ok: self.av1_ok,
            device_name: self.device_name,
            t: self.t,
        };
        self.fonts.heading(
            canvas,
            &screen.title(&ctx),
            W::Bold,
            30.0 * self.k,
            fg(1.0),
            EDGE_INSET * self.k,
            18.0 * self.k,
            self.title_max_w,
        );
        screen.render(canvas, self.content, self.k, self.dt, self.fonts, &mut ctx);
        let rects = if self.show_hints {
            let hints = screen.hints(&ctx);
            hint_bar(
                canvas,
                self.fonts,
                &hints,
                self.glyphs,
                18.0 * self.k,
                self.h - 18.0 * self.k,
                self.k,
            )
            .rects
        } else {
            Vec::new()
        };
        canvas.restore();
        rects
    }
}

/// Glyphs follow the last input source. Keys speak the platform's key device
/// (Android remote, desktop keyboard); a pad speaks its family. Before any
/// input, the connected pad's family if there is one, else the key device.
fn glyph_style(
    source: Option<crate::console::InputSource>,
    pad_pref: Option<punktfunk_core::config::GamepadPref>,
    platform: crate::platform::Platform,
) -> GlyphStyle {
    let keys = || match platform {
        // A TV remote either way — the legend says D-pad, not keys.
        crate::platform::Platform::Android | crate::platform::Platform::WebOS => GlyphStyle::Remote,
        crate::platform::Platform::Desktop | crate::platform::Platform::Web => GlyphStyle::Keyboard,
    };
    match (source, pad_pref) {
        (Some(crate::console::InputSource::Keys), _) => keys(),
        (_, Some(p)) => GlyphStyle::from_pref(Some(p)),
        (_, None) => keys(),
    }
}

#[cfg(test)]
mod glyph_style_tests {
    use super::*;
    use crate::console::InputSource;
    use crate::platform::Platform;
    use punktfunk_core::config::GamepadPref;

    /// Last input source wins; an unplugged pad falls back to the platform key device.
    #[test]
    fn the_legend_follows_what_drives() {
        let xbox = Some(GamepadPref::Xbox360);
        assert_eq!(
            glyph_style(None, xbox, Platform::Android),
            GlyphStyle::Letters
        );
        assert_eq!(
            glyph_style(None, None, Platform::Android),
            GlyphStyle::Remote
        );
        assert_eq!(
            glyph_style(None, None, Platform::Desktop),
            GlyphStyle::Keyboard
        );
        assert_eq!(
            glyph_style(Some(InputSource::Keys), xbox, Platform::Android),
            GlyphStyle::Remote
        );
        assert_eq!(
            glyph_style(Some(InputSource::Keys), xbox, Platform::Desktop),
            GlyphStyle::Keyboard
        );
        assert_eq!(
            glyph_style(
                Some(InputSource::Pad),
                Some(GamepadPref::SwitchPro),
                Platform::Desktop
            ),
            GlyphStyle::Nintendo
        );
        assert_eq!(
            glyph_style(
                Some(InputSource::Pad),
                Some(GamepadPref::DualSense),
                Platform::Android
            ),
            GlyphStyle::Shapes
        );
        assert_eq!(
            glyph_style(Some(InputSource::Pad), None, Platform::Android),
            GlyphStyle::Remote
        );
    }
}
