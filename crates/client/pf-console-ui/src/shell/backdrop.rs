//! The field behind every screen: the palette's mesh shader, the calm mix that follows the
//! screen, and the retained offscreen it renders into.

use crate::palette::{field_camera, field_motion, field_sksl, palette, VIOLET_FIELD};
use anyhow::{anyhow, Result};
use pf_client_core::trust;
use skia_safe::{Canvas, Color4f, Data, Paint, Rect, RuntimeEffect, Surface};
use std::cell::RefCell;

/// Long edge of the backdrop's offscreen, px. The field is a pure function of `xy/u_res`
/// and soft, so a small buffer blitted up holds the same picture at any glass size — its
/// per-pixel noise never scales with a 4K surface. The reduced interface takes a quarter of
/// 384's pixels: on a 2025 LG TV the noise costs ~29 ms of GPU at 384 and ~8 ms at 192.
const FIELD_EDGE: f64 = 512.0;
const FIELD_EDGE_REDUCED: f64 = 192.0;
/// Seconds between backdrop re-renders (~25 Hz). The field morphs slowly, so the step is
/// invisible; a frozen (reduce-motion) field renders once.
pub(super) const FIELD_STEP: f64 = 0.04;

pub(super) struct Backdrop {
    mesh: RuntimeEffect,
    /// Palette id baked into `mesh`. [`Self::sync`] recompiles when
    /// `settings.ui_palette` moves.
    mesh_palette: String,
    /// OS-theme revision baked into `mesh` while follow-system is on.
    /// `None` for a curated palette. Pair with `mesh_palette` so `sync`
    /// rebuilds only on the row step or a real theme change.
    mesh_os: Option<u64>,
    /// Palette ground × 0.4. `col*0.6 + lift` leaves the ground unchanged
    /// and pulls the bright pools down: form screens lose contrast, not colour.
    mesh_lift: [f32; 3],
    /// Backdrop scrim: rgb = vignette target (black on dark, white on pale),
    /// a = strength. Kept with the ink.
    mesh_scrim: [f32; 4],
    /// Text/accent/glass for this palette, published once per frame
    /// (see [`crate::theme::set_ink`]).
    pub(super) ink: crate::theme::Ink,
    /// 0 = launcher aurora, 1 = form field. Chased so the backdrop settles
    /// with the screen transition.
    pub(super) bg_mix: f64,
    /// The retained offscreen [`Self::draw`] renders into. A cell because the takeover chain
    /// borrows overlay state while it draws, so `&mut self` never reaches here.
    pub(super) field: RefCell<Option<FieldCache>>,
}

impl Backdrop {
    /// The backdrop for a palette, its calm mix settled at `bg_mix`.
    pub(super) fn new(palette_id: &str, bg_mix: f64) -> Result<Backdrop> {
        let (mesh, mesh_lift, mesh_scrim, ink) = build_mesh(palette_id)?;
        Ok(Backdrop {
            mesh,
            mesh_palette: palette_id.to_string(),
            mesh_os: None,
            mesh_lift,
            mesh_scrim,
            ink,
            bg_mix,
            field: RefCell::new(None),
        })
    }

    /// Settings write palette/follow-OS; recompile here so the backdrop re-colours live. A
    /// rejected compile keeps the field that is drawing (never black) and still advances
    /// bookkeeping so a broken build warns once, not once per frame.
    pub(super) fn sync(&mut self, settings: &trust::Settings) {
        let (os_rev, os) = crate::os_theme::os_theme();
        let want_os = if settings.follow_os_theme { os } else { None };
        if let Some(t) = want_os {
            if self.mesh_os != Some(os_rev) {
                match build_mesh_os(&t) {
                    Ok(look) => self.apply_look(look),
                    Err(e) => tracing::warn!("console: OS theme rejected: {e}"),
                }
                self.mesh_os = Some(os_rev);
            }
        } else if self.mesh_os.is_some() || settings.ui_palette != self.mesh_palette {
            match build_mesh(&settings.ui_palette) {
                Ok(look) => self.apply_look(look),
                Err(e) => {
                    tracing::warn!("console: {} palette rejected: {e}", settings.ui_palette);
                }
            }
            self.mesh_os = None;
            self.mesh_palette = settings.ui_palette.clone();
        }
    }

    fn apply_look(&mut self, (mesh, lift, scrim, ink): MeshLook) {
        (self.mesh, self.mesh_lift, self.mesh_scrim, self.ink) = (mesh, lift, scrim, ink);
    }

    /// The field as a paint for an `w`×`h` target — `u_res` is the TARGET's pixels,
    /// the shader's `xy/u_res` normalises everything, so the reduced pass's small
    /// offscreen renders the same picture the full surface would.
    /// `passes` is the shader's work per pixel: 2 hits the displaced surface, 1 the plain
    /// sphere — the reduced path's saving on a TV, where the offscreen hides the difference.
    fn paint(&self, w: f64, h: f64, t: f64, calm: f64, passes: f32) -> Option<Paint> {
        // Matches the SkSL block: u_res, u_tc, u_lift, u_scrim, u_cam, then `field_motion`'s
        // u_rot0..2, u_mot, u_wmot.
        let (focal, scale) = field_camera(w / h.max(1.0));
        let head: [f32; 16] = [
            w as f32,
            h as f32,
            t as f32,
            calm as f32,
            self.mesh_lift[0],
            self.mesh_lift[1],
            self.mesh_lift[2],
            0.0,
            self.mesh_scrim[0],
            self.mesh_scrim[1],
            self.mesh_scrim[2],
            self.mesh_scrim[3],
            focal as f32,
            scale as f32,
            passes,
            0.0,
        ];
        let mut uniforms = [0.0f32; 36];
        uniforms[..16].copy_from_slice(&head);
        uniforms[16..].copy_from_slice(&field_motion(t));
        let words = uniforms.map(f32::to_ne_bytes);
        let bytes = words.as_flattened();
        self.mesh
            .make_shader(Data::new_copy(bytes), &[], None)
            .map(|shader| {
                let mut paint = crate::theme::shaded();
                paint.set_shader(shader);
                paint
            })
    }

    /// The field over `w`×`h` at clock `t`. The reduced interface takes a smaller buffer and
    /// one pass over the sphere.
    pub(super) fn draw(&self, canvas: &Canvas, w: f64, h: f64, t: f64, calm: f64, reduced: bool) {
        let mut cache = self.field.borrow_mut();
        let (edge, passes) = if reduced {
            (FIELD_EDGE_REDUCED, 1.0)
        } else {
            (FIELD_EDGE, 2.0)
        };
        self.draw_field(canvas, &mut cache, w, h, t, calm, edge, passes);
    }

    /// The field into a ≤`edge`-px offscreen, blitted up with bilinear sampling.
    /// Re-rendered only when an input moved — size, palette, calm, or the clock past
    /// [`FIELD_STEP`]. The takeover's `calm = 0` and the base field's share one slot: when
    /// both differ each gets a small re-render a frame, still a fraction of a surface pass.
    #[allow(clippy::too_many_arguments)]
    fn draw_field(
        &self,
        canvas: &Canvas,
        cache: &mut Option<FieldCache>,
        w: f64,
        h: f64,
        t: f64,
        calm: f64,
        edge: f64,
        passes: f32,
    ) {
        let scale = (edge / w.max(h)).min(1.0);
        let size = ((w * scale).ceil() as i32, (h * scale).ceil() as i32);
        // `t < c.t` is the test clock rewinding, not a direction the field moves.
        let stale = cache.as_ref().is_none_or(|c| {
            c.size != size
                || c.calm != calm
                || c.mesh.0 != self.mesh_palette
                || c.mesh.1 != self.mesh_os
                || t - c.t >= FIELD_STEP
                || t < c.t
        });
        if stale {
            if let Some(mut surface) = field_surface(canvas, size) {
                // u_res is the offscreen's own pixels — `paint` is resolution-free.
                if let Some(paint) = self.paint(size.0 as f64, size.1 as f64, t, calm, passes) {
                    surface
                        .canvas()
                        .draw_rect(Rect::from_wh(size.0 as f32, size.1 as f32), &paint);
                    *cache = Some(FieldCache {
                        surface,
                        size,
                        t,
                        calm,
                        mesh: (self.mesh_palette.clone(), self.mesh_os),
                    });
                }
                // A rejected shader keeps whatever the cache held: a stale field beats black.
            } else {
                // No offscreen (context teardown): a full-surface draw is the fallback,
                // never a black frame.
                match self.paint(w, h, t, calm, passes) {
                    Some(paint) => {
                        canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
                    }
                    None => {
                        canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
                    }
                }
                return;
            }
        }
        match cache {
            Some(c) => {
                canvas.draw_image_rect_with_sampling_options(
                    c.surface.image_snapshot(),
                    None,
                    Rect::from_wh(w as f32, h as f32),
                    skia_safe::SamplingOptions::new(
                        skia_safe::FilterMode::Linear,
                        skia_safe::MipmapMode::None,
                    ),
                    &crate::theme::shaded(),
                );
            }
            // Stale with nothing cached means the shader rejected — the direct path's
            // own answer.
            None => {
                canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
            }
        }
    }
}

/// The reduced backdrop's retained pass: the offscreen and the inputs it was rendered
/// from — anything that moves one of them is what a re-render keys on.
pub(super) struct FieldCache {
    surface: Surface,
    /// The offscreen's pixel size (`FIELD_EDGE`-scaled from the surface it blits to).
    pub(super) size: (i32, i32),
    /// Clock and calm mix baked into the current contents.
    pub(super) t: f64,
    calm: f64,
    /// `mesh`'s provenance (palette id, OS-theme revision) — a palette change must
    /// re-render even with the clock frozen.
    mesh: (String, Option<u64>),
}

/// The reduced backdrop's offscreen, on `canvas`'s own backend ([`crate::blur::offscreen`]).
/// A raster offscreen under a GPU canvas runs the field's SkSL on the CPU, several frames'
/// worth on a TV.
fn field_surface(canvas: &Canvas, size: (i32, i32)) -> Option<Surface> {
    crate::blur::offscreen(canvas, size.0, size.1)
}

/// Compile the mesh for a palette and the lift, scrim, and ink it decides.
/// `uniform_size` is checked: [`Backdrop::paint`] hand-packs the buffer
/// and a silent layout change would feed the field garbage.
type MeshLook = (RuntimeEffect, [f32; 3], [f32; 4], crate::theme::Ink);

fn build_mesh(palette_id: &str) -> Result<MeshLook> {
    let p = palette(palette_id);
    compile_mesh(
        p.stops.unwrap_or(&VIOLET_FIELD),
        crate::theme::Ink::of(p),
        p.ground,
    )
}

/// Follow-system field: a quiet ramp from the theme's own colours, not the
/// curated hue arcs. The desk colour is the point.
pub(super) fn build_mesh_os(t: &crate::os_theme::OsTheme) -> Result<MeshLook> {
    use crate::os_theme::Rgb;
    let (bg, fg, ac) = (t.background, t.foreground, t.accent);
    // A pale field shades toward its text colour, not black: darkening a pastel strands
    // dark ink on it (see `theme::Ink` scrim).
    let stops = if t.light {
        [bg.mix(fg, 0.10), bg.mix(ac, 0.18), bg]
    } else {
        [bg.mix(Rgb(0.0, 0.0, 0.0), 0.35), bg.mix(ac, 0.30), bg]
    };
    let rgb = |Rgb(r, g, b)| (r, g, b);
    compile_mesh(&stops.map(rgb), crate::theme::Ink::of_os(t), rgb(bg))
}

fn compile_mesh(
    stops: &[(f64, f64, f64)],
    ink: crate::theme::Ink,
    ground: (f64, f64, f64),
) -> Result<MeshLook> {
    let effect = RuntimeEffect::make_for_shader(field_sksl(ground, stops), None)
        .map_err(|e| anyhow!("backdrop SkSL: {e}"))?;
    anyhow::ensure!(
        effect.uniform_size() == 144,
        "mesh uniform block is {} bytes, expected 144 (u_res … u_cam, u_rot0..2, u_mot, u_wmot)",
        effect.uniform_size()
    );
    let g = ground;
    Ok((
        effect,
        [(g.0 * 0.4) as f32, (g.1 * 0.4) as f32, (g.2 * 0.4) as f32],
        [ink.scrim.r, ink.scrim.g, ink.scrim.b, ink.scrim.a],
        ink,
    ))
}
