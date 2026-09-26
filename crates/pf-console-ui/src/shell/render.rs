//! Per-frame screen compose and transition.

use crate::anim::approach;
use crate::glyphs::{hint_bar, GlyphStyle};
use crate::library::LibraryShared;
use crate::model::HostRow;
use crate::screens::{Bg, Ctx, Screen};
use crate::theme::{edge, fg, Fonts, PanelStroke, EDGE_INSET, W};
use crate::widgets::{text_tab, tray, Toward};
use pf_client_core::menu_nav::PadInfo;
use pf_client_core::trust;
use skia_safe::{Canvas, Picture, PictureRecorder, Rect};
use std::time::Instant;

use super::{
    Motion, NavKind, Shell, Tab, BOTTOM_BAND, NAV_ENTER_SCALE, NAV_EXIT_SCALE, NAV_REVEAL_ALPHA,
    NAV_SLIDE_DP, TABS, TAB_SLIDE, TOP_BAND,
};
use crate::el::{El, Id, Tree};
use crate::glyphs::{Hint, HintKey, HINT_PAD};

/// The tab strip's top and height, design units.
const STRIP_TOP: f64 = 32.0;
const STRIP_H: f64 = 36.0;

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
        crate::el::begin_frame();
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
        self.tick_touch();
        self.tick_ok();
        // Publish ink before any draw. Widgets read `theme::set_ink`; skipping this
        // paints the previous palette's text on the new field.
        crate::theme::set_ink(self.ink);
        // Same publish-once contract as ink. Also a local: `LayerEnv` mut-borrows
        // `settings`, so the transition arms cannot read the field.
        let reduce = self.reduce_motion();
        crate::theme::set_reduce_motion(reduce);
        crate::theme::set_reduced_ui(crate::screens::settings::reduce_ui_res(
            &self.settings,
            self.platform,
            self.fallback_ui,
        ));
        self.pads = pads.to_vec();
        self.glyphs = glyph_style(self.input_source, pad_pref, self.platform);
        if let Some(Screen::InputTest(test)) = self.stack.last_mut() {
            test.pref = pad_pref;
        }
        // The chip names the connected pad, rebuilt only when it changes; with none there
        // is nothing to say. `PadInfo` has no `PartialEq` in its crate.
        if self.chip.as_deref() != pad {
            self.chip = pad.map(str::to_owned);
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
        crate::theme::set_side_inset(f64::from(ins.left));
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

        // The hint bar's band only where a hint bar can show.
        let bottom = if self.glyphs == GlyphStyle::Remote {
            EDGE_INSET
        } else {
            BOTTOM_BAND
        };
        let content = Rect::from_ltrb(
            0.0,
            (TOP_BAND * k) as f32,
            w as f32,
            (h - bottom * k) as f32,
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
            (w - 2.0 * edge(k) - chip_w - 12.0 * k).max(w * 0.35)
        };
        let games_ok = self.games_host().is_some();
        let (tab, strip_focus) = (self.tab, self.strip_focus);
        let mut env = LayerEnv {
            strip: &mut self.strip,
            tab,
            strip_focus,
            games_ok,
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
            tv: self.tv,
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
            cheap: false,
            root_targets: None,
        };
        self.hint_rects.clear();
        // Reduced motion keeps the crossfade (an instant swap loses the only spatial
        // cue) and drops slide/scale.
        let slide = |d: f64| if reduce { 0.0 } else { d };
        // A tab's root shows the strip where a pushed screen shows its title.
        let band = |i: usize| if i == 0 { Band::Strip } else { Band::Title };
        let zoom = |s: f64| if reduce { 1.0 } else { s };
        let mut chrome = Vec::with_capacity(2);
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
                    let b = band(n - 1);
                    chrome.push(env.paint(replaced.as_mut(), 1.0 - p, 0.0, 0.0, recede, b));
                    let top = &mut self.stack[n - 1];
                    chrome.push(env.paint(top, p, 0.0, enter_slide, enter_scale, b));
                } else if n >= 2 {
                    let (below, top) = self.stack.split_at_mut(n - 1);
                    let b = band(n - 2);
                    chrome.push(env.paint(&mut below[n - 2], 1.0 - p, 0.0, 0.0, recede, b));
                    let t = Band::Title;
                    chrome.push(env.paint(&mut top[0], p, 0.0, enter_slide, enter_scale, t));
                } else {
                    let root = &mut self.stack[0];
                    let b = Band::Strip;
                    chrome.push(env.paint(root, p, 0.0, enter_slide, enter_scale, b));
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
                chrome.push(env.paint(
                    &mut self.stack[n - 1],
                    NAV_REVEAL_ALPHA + (1.0 - NAV_REVEAL_ALPHA) * p,
                    0.0,
                    0.0,
                    zoom(NAV_EXIT_SCALE + (1.0 - NAV_EXIT_SCALE) * p),
                    band(n - 1),
                ));
                let dy = slide(NAV_SLIDE_DP * k * p);
                chrome.push(env.paint(leaving.as_mut(), 1.0 - p, 0.0, dy, 1.0, Band::Title));
            }
            // A tab switch: the new root slides in a quarter width from the side it sits on.
            // Both roots ask for the strip, so it holds still at full strength.
            (Motion::Tab { from, .. }, Some(p)) => {
                let dir = if from.index() < tab.index() {
                    1.0
                } else {
                    -1.0
                };
                let dx = |x: f64| slide(x * w * TAB_SLIDE);
                if let Some(old) = self.parked[from.index()].as_mut() {
                    chrome.push(env.paint(old, 1.0 - p, dx(-dir * p), 0.0, 1.0, Band::Strip));
                }
                let n = self.stack.len();
                let root = &mut self.stack[n - 1];
                chrome.push(env.paint(root, p, dx(dir * (1.0 - p)), 0.0, 1.0, Band::Strip));
            }
            _ => {
                let n = self.stack.len();
                let top = &mut self.stack[n - 1];
                chrome.push(env.paint(top, 1.0, 0.0, 0.0, 1.0, band(n - 1)));
            }
        }
        // Only a settled top screen publishes hint hit-boxes: mid-transition two legends
        // share the band, so a reported rect is not necessarily the one under the pointer.
        let settled = chrome.len() == 1 && chrome[0].alpha >= 0.999;
        let rects = env.chrome(&chrome);
        if settled {
            self.hint_rects = rects;
        }
        self.root_targets = env.root_targets;

        if let Some(chip) = &self.chip {
            let size = 12.0 * k;
            let tw = f64::from(fonts.measure(chip, W::Medium, size));
            let (bh, pad_x, gap) = (24.0 * k, 12.0 * k, 8.0 * k);
            let mark_w = 15.0 * k;
            let battery = self.pads.first().and_then(|p| p.battery);
            let bw = chip_width(fonts, chip, battery.is_some(), k);
            let bx = w - edge(k) - bw;
            // Centred on the tabs' line.
            let top = (STRIP_TOP + STRIP_H / 2.0) * k - bh / 2.0;
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
            // The chip names a pad, so it draws that pad's family whatever drove last.
            let mark =
                crate::glyphs::device_icon(Some(pad_pref.unwrap_or_default()), self.platform);
            crate::glyphs::pad_mark(canvas, mark, bx + pad_x, cy, mark_w, k, fg(0.7));
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

/// What a layer asks the band above its content to show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Band {
    Title,
    Strip,
}

/// One layer's request of the fixed chrome, at the layer's alpha.
struct Chrome {
    alpha: f64,
    band: Band,
    title: Option<String>,
    hints: Vec<Hint>,
    /// How far into the content the top and bottom trays reach for this screen, and the
    /// screen's own chrome to draw over them, recorded while the layer painted.
    pinned: (f32, f32),
    pinned_pic: Option<Picture>,
}

/// How far the top and bottom trays reach into the content: the furthest any screen asks,
/// scaled by how far that screen has faded in, so a tray recedes with the screen it serves.
fn tray_reach(layers: &[Chrome]) -> (f32, f32) {
    let reach = |pick: fn(&Chrome) -> f32| {
        layers
            .iter()
            .map(|c| pick(c) * c.alpha.clamp(0.0, 1.0) as f32)
            .fold(0.0, f32::max)
    };
    (reach(|c| c.pinned.0), reach(|c| c.pinned.1))
}

pub(super) fn pill_id(tab: Tab) -> Id {
    Id::new(tab.id(), 0)
}

/// The legend keeps the shortcuts the device in hand has: Y, X, the shoulders (tabs, on a
/// root) and named keys. OK and the directions are what the focus plate already says; a
/// pad's Back is on the pad. A remote has none of these, so it shows no bar. A keyboard
/// keeps Back: for a mouse the legend is the only exit.
fn shortcuts(hints: Vec<Hint>, glyphs: GlyphStyle, root: bool) -> Vec<Hint> {
    if glyphs == GlyphStyle::Remote {
        return Vec::new();
    }
    let mut kept: Vec<Hint> = hints
        .into_iter()
        .filter(|h| match h.key {
            HintKey::Secondary | HintKey::Tertiary | HintKey::Key(_) => true,
            HintKey::Back => glyphs == GlyphStyle::Keyboard,
            _ => false,
        })
        .collect();
    if root {
        kept.insert(0, Hint::new(HintKey::Shoulders, "Tabs"));
    }
    kept
}

/// One screen layer's paint args, so each `paint` borrows `Shell` fields disjointly.
struct LayerEnv<'a> {
    strip: &'a mut Tree,
    tab: Tab,
    strip_focus: bool,
    /// Games has a paired host to show.
    games_ok: bool,
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
    tv: bool,
    fallback_ui: bool,
    pyrowave_ok: bool,
    av1_ok: bool,
    device_name: &'a str,
    t: f64,
    glyphs: GlyphStyle,
    show_hints: bool,
    /// Reduced UI resolution, as the last painted screen's settings say.
    cheap: bool,
    /// Focus targets the tab root placed, once its layer paints.
    root_targets: Option<usize>,
}

/// Opens a draw at `alpha`: a layer when faded, a plain save when whole. False, and
/// nothing opened, when there is nothing to see; the caller restores only on true.
fn open_at(canvas: &Canvas, alpha: f64) -> bool {
    if alpha < 0.001 {
        return false;
    }
    if alpha < 0.999 {
        crate::theme::save_layer_alpha(canvas, None, alpha as f32);
    } else {
        canvas.save();
    }
    true
}

impl LayerEnv<'_> {
    /// One screen's content as a unit: fade, slide, scale about centre. The band and
    /// legend stay out of the layer; the returned [`Chrome`] draws them in place, so a
    /// tab switch or a push never moves the strip. A tab root records the focus targets
    /// it placed.
    #[allow(clippy::too_many_arguments)]
    fn paint(
        &mut self,
        screen: &mut Screen,
        alpha: f64,
        dx: f64,
        dy: f64,
        scale: f64,
        band: Band,
    ) -> Chrome {
        let canvas = self.canvas;
        // Raise a layer only when alpha/scale/slide actually change. Unbounded
        // `save_layer` is a full-surface offscreen; Skia does not elide alpha ≥ 1.
        // Settled SrcOver draws are pixel-identical without the isolation.
        let layered = alpha < 0.999 || (scale - 1.0).abs() > 0.001 || dy.abs() > 0.001;
        if layered {
            crate::theme::save_layer_alpha(canvas, None, alpha.clamp(0.0, 1.0) as f32);
        } else {
            // Save anyway: the transform below is undone by the same `restore`.
            canvas.save();
        }
        // A list's own soft edges copy the surface, which has none of this layer: they
        // would blur the field behind it over the rows. Off for the transition.
        crate::blur::set_in_layer(layered);
        // Pinned chrome records over the whole canvas, past the safe area, so a band's
        // scrim reaches the glass. Read before the slide moves the clip.
        let edges = canvas
            .local_clip_bounds()
            .unwrap_or_else(|| Rect::from_wh(self.w as f32, self.h as f32));
        // It records in device space, as this canvas stands before the slide: a focus plate
        // handing off to or from it, and a blur, then read the rects every layer reads.
        let base = canvas.local_to_device();
        let device_edges = canvas.local_to_device_as_3x3().map_rect(edges).0;
        canvas.translate((dx as f32, dy as f32));
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
            tv: self.tv,
            fallback_ui: self.fallback_ui,
            pyrowave_ok: self.pyrowave_ok,
            av1_ok: self.av1_ok,
            device_name: self.device_name,
            t: self.t,
        };
        // With focus on the tabs, a root's plate fades out. A root's target count says if
        // focus can enter.
        crate::el::set_dormant(self.strip_focus && band == Band::Strip);
        let mut pinned_pic = None;
        let targets = crate::el::census(|| {
            screen.render(canvas, self.content, self.k, self.dt, self.fonts, &mut ctx);
            // Pinned chrome is recorded, not drawn: it goes over the trays, in place, so
            // a slide or a zoom never carries it. Its targets still count here.
            if screen.pinned(self.k) != (0.0, 0.0) {
                let mut rec = PictureRecorder::new();
                let rc = rec.begin_recording(device_edges, false);
                rc.set_matrix(&base);
                screen.render_pinned(rc, self.content, self.k, self.dt, self.fonts, &ctx);
                pinned_pic = rec.finish_recording_as_picture(None);
            }
        });
        crate::el::set_dormant(false);
        if band == Band::Strip {
            self.root_targets = Some(targets);
        }
        self.cheap =
            crate::screens::settings::reduce_ui_res(ctx.settings, ctx.platform, ctx.fallback_ui);
        let title = (band == Band::Title).then(|| screen.title(&ctx));
        let hints = if self.show_hints {
            shortcuts(screen.hints(&ctx), self.glyphs, band == Band::Strip)
        } else {
            Vec::new()
        };
        canvas.restore();
        crate::blur::set_in_layer(false);
        Chrome {
            alpha,
            band,
            title,
            hints,
            pinned: screen.pinned(self.k),
            pinned_pic,
        }
    }

    /// The band and legend over every layer, drawn after content so a list scrolls up
    /// under them. The strip's strength is the sum of the layers asking for it, so two
    /// roots crossing hold it at full. Returns the last legend's hit-boxes.
    fn chrome(&mut self, layers: &[Chrome]) -> Vec<(HintKey, Rect)> {
        let canvas = self.canvas;
        // Trays out to the screen's edges, past the safe area, behind the band and legend,
        // reaching in as far as the screens' own pinned chrome: one ramp, never two stacked.
        let (k, content) = (self.k, self.content);
        let edges = canvas.local_clip_bounds().unwrap_or(content);
        let (into_top, into_bottom) = tray_reach(layers);
        let top = Rect::from_ltrb(edges.left, edges.top, edges.right, content.top + into_top);
        tray(canvas, top, Toward::Top, k);
        let foot = content.bottom - into_bottom;
        let bottom = Rect::from_ltrb(edges.left, foot, edges.right, edges.bottom);
        tray(canvas, bottom, Toward::Bottom, k);
        for c in layers {
            let Some(pic) = &c.pinned_pic else {
                continue;
            };
            if open_at(canvas, c.alpha) {
                // Recorded in device space (`paint`).
                canvas.reset_matrix();
                canvas.draw_picture(pic, None, None);
                canvas.restore();
            }
        }
        let strip: f64 = layers
            .iter()
            .filter(|c| c.band == Band::Strip)
            .map(|c| c.alpha)
            .sum();
        if open_at(canvas, strip.min(1.0)) {
            self.draw_strip(canvas);
            canvas.restore();
        }
        for c in layers {
            let Some(title) = c.title.as_deref() else {
                continue;
            };
            if open_at(canvas, c.alpha) {
                let k = self.k;
                let (x, top) = (edge(k), 18.0 * k);
                let size = 30.0 * k;
                self.fonts.heading(
                    canvas,
                    title,
                    W::Bold,
                    size,
                    fg(1.0),
                    x,
                    top,
                    self.title_max_w,
                );
                canvas.restore();
            }
        }
        let mut rects = Vec::new();
        for c in layers.iter().filter(|c| !c.hints.is_empty()) {
            if open_at(canvas, c.alpha) {
                let (k, glyphs) = (self.k, self.glyphs);
                let (x, bottom) = (edge(k) - HINT_PAD * k, self.h - 18.0 * k);
                rects = hint_bar(canvas, self.fonts, &c.hints, glyphs, x, bottom, k).rects;
                canvas.restore();
            }
        }
        rects
    }

    /// The text tabs where a root's title would be, text on the title's margin. The plate
    /// sits behind the current tab while the strip has focus.
    fn draw_strip(&mut self, canvas: &Canvas) {
        let k = self.k;
        let (size, pad, gap) = (20.0 * k, 10.0 * k, 4.0 * k);
        let (h, top) = (STRIP_H * k, STRIP_TOP * k);
        let mut x = edge(k) - pad;
        let mut row = El::column();
        for tab in TABS {
            let w = f64::from(self.fonts.measure(tab.name(), W::Bold, size)) + 2.0 * pad;
            let r = Rect::from_xywh(x as f32, top as f32, w as f32, h as f32);
            let ink = match (tab != Tab::Games || self.games_ok, tab == self.tab) {
                (false, _) => fg(0.28),
                (true, true) => fg(1.0),
                (true, false) => fg(0.6),
            };
            let fonts = self.fonts;
            row = row.child(
                El::paint(move |canvas, r| text_tab(canvas, fonts, tab.name(), r, size, ink))
                    .id(pill_id(tab))
                    .focusable((10.0 * k) as f32)
                    .place(r),
            );
            x += w + gap;
        }
        let frame = self
            .strip
            .layout(row, Rect::from_xywh(0.0, 0.0, self.w as f32, self.h as f32));
        // Unfocused, the strip's plate fades like any tree's, so focus arriving here glides
        // in from the content rather than showing on the pill at once.
        self.strip
            .set_focus(self.strip_focus.then(|| pill_id(self.tab)));
        self.strip
            .paint_focus(canvas, frame, k as f32, self.dt, self.cheap);
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
    match (source, pad_pref) {
        (Some(crate::console::InputSource::Keys), _) => GlyphStyle::keys(platform),
        (_, Some(p)) => GlyphStyle::from_pref(Some(p)),
        (_, None) => GlyphStyle::keys(platform),
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

#[cfg(test)]
mod tray_tests {
    use super::*;

    /// Leaving a screen with a tall footer, the bottom tray shrinks with its fade instead of
    /// holding at full height and snapping back when the slide ends.
    #[test]
    fn a_tray_recedes_with_the_screen_it_serves() {
        let chrome = |alpha: f64, bottom: f32| Chrome {
            alpha,
            band: Band::Strip,
            title: None,
            hints: Vec::new(),
            pinned: (0.0, bottom),
            pinned_pic: None,
        };
        let at = |p: f64| tray_reach(&[chrome(1.0 - p, 80.0), chrome(p, 0.0)]).1;
        assert_eq!(at(0.0), 80.0);
        assert!((at(0.5) - 40.0).abs() < 1e-3);
        assert_eq!(at(1.0), 0.0);
    }
}
