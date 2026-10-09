//! Background picker: one card per palette, its field in miniature. Reached from the
//! Interface section's Background row.
//!
//! Confirm writes `ui_palette` and saves; the shell recompiles the field on the next
//! frame, so the pick shows behind the cards. The focus plate is the shell's; a card
//! draws no halo of its own.

use crate::anim::approach;
use crate::el::{Axis, El, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::icons::{by_name, draw_icon_weight};
use crate::palette::{Palette, PALETTES, VIOLET_FIELD};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::{edge, fill, stroke, Fonts, W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, ClipOp, Color4f, Point, RRect, Rect, TileMode};

const GRID: &str = "palette-grid";
const CARD_W: f64 = 236.0;
const CARD_H: f64 = 150.0;
const CARD_GAP: f64 = 22.0;
const CARD_CORNER: f64 = 20.0;

fn card_id(i: usize) -> Id {
    Id::new("palette-card", i)
}

pub(crate) struct PaletteScreen {
    cursor: usize,
    /// Cards as focus targets in a vertical scroll. Boxed: the screen is a variant of one enum.
    tree: Box<Tree>,
    geom: Vec<Rect>,
}

impl PaletteScreen {
    /// Opens on the palette in force, so Confirm without a move changes nothing.
    pub(crate) fn new(current: &str) -> PaletteScreen {
        PaletteScreen {
            cursor: PALETTES.iter().position(|p| p.id == current).unwrap_or(0),
            tree: Box::default(),
            geom: Vec::new(),
        }
    }

    fn apply(&mut self, ctx: &mut Ctx) -> Option<MenuPulse> {
        let id = PALETTES[self.cursor].id;
        let changed = ctx.write(|c| {
            let changed = c.settings.ui_palette != id;
            c.settings.ui_palette = id.to_string();
            changed
        });
        Some(if changed {
            MenuPulse::Confirm
        } else {
            MenuPulse::Boundary
        })
    }

    fn step(&mut self, dir: MenuDir) -> Option<MenuPulse> {
        let to = self.tree.move_focus(dir)?;
        match (0..PALETTES.len()).find(|&i| card_id(i) == to) {
            Some(i) => {
                self.cursor = i;
                Some(MenuPulse::Move)
            }
            None => Some(MenuPulse::Boundary),
        }
    }
}

impl ScreenView for PaletteScreen {
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            // Nothing that way: a thud, not silence.
            MenuEvent::Move(dir) => self.step(dir).or(Some(MenuPulse::Boundary)),
            MenuEvent::Confirm => self.apply(ctx),
            MenuEvent::Back => {
                fx.pop();
                None
            }
            _ => None,
        }
    }

    fn press(&mut self) {
        self.tree.press();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.tree.drag(Id::new(GRID, 0), p)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        match p.kind {
            PointerKind::Scroll { up } => {
                let step = if up { -80.0 } else { 80.0 };
                self.tree.pan(Id::new(GRID, 0), step);
                self.tree.release(Id::new(GRID, 0), 0.0);
                true
            }
            // Hover focuses, so the press that follows applies. A touchscreen sends Press
            // with no Move before it: the first press focuses, the second applies.
            PointerKind::Move => match p.pick(&self.geom) {
                Some(i) if i != self.cursor => {
                    self.cursor = i;
                    true
                }
                _ => false,
            },
            PointerKind::Press => match p.pick(&self.geom) {
                Some(i) if i == self.cursor => {
                    self.apply(ctx);
                    true
                }
                Some(i) => {
                    self.cursor = i;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        let p = &PALETTES[self.cursor];
        let tone = if p.light { "light" } else { "dark" };
        let state = if ctx.settings.ui_palette == p.id {
            ", selected"
        } else {
            ""
        };
        Some(format!("{}, {tone}{state}", p.name))
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Done"),
        ]
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let grid = Id::new(GRID, 0);
        let avail = f64::from(rect.width()) - 2.0 * edge(k);
        let gap = CARD_GAP * k;
        // At least two across, shrinking the card on a phone rather than stacking one column.
        let cols = (((avail + gap) / (CARD_W * k + gap)).floor() as usize).max(2);
        let cw = ((avail - gap * (cols - 1) as f64) / cols as f64).min(CARD_W * k);
        let ch = cw * CARD_H / CARD_W;
        let grid_w = cw * cols as f64 + gap * (cols - 1) as f64;
        let left = f64::from(rect.left) + (f64::from(rect.width()) - grid_w) / 2.0;
        let air = (12.0 * k) as f32;
        // The viewport reaches the layer's edges and pads back to `rect`, so cards run under
        // the shell's trays and blur there instead of stopping short.
        let view = if crate::blur::active() {
            let clip = canvas.local_clip_bounds().unwrap_or(rect);
            Rect::from_ltrb(
                rect.left,
                clip.top.min(rect.top),
                rect.right,
                clip.bottom.max(rect.bottom),
            )
        } else {
            rect
        };
        let (pad_top, pad_bottom) = (rect.top - view.top + air, view.bottom - rect.bottom + air);

        let current = ctx.settings.ui_palette.clone();
        let mut tree = std::mem::take(&mut self.tree);
        tree.tick(dt as f32);
        let current = &current;
        let rows = PALETTES.chunks(cols).enumerate().map(|(r, chunk)| {
            El::row()
                .gap(gap as f32)
                .children(chunk.iter().enumerate().map(move |(c, p)| {
                    let i = r * cols + c;
                    El::paint(move |canvas, cell| {
                        draw_card(canvas, fonts, p, cell, p.id == current, k);
                    })
                    .id(card_id(i))
                    .focusable((CARD_CORNER * k * cw / (CARD_W * k)) as f32)
                    .size(cw as f32, ch as f32)
                }))
        });
        let root = El::scroll(grid, Axis::Vertical)
            .gap(gap as f32)
            .style(|s| {
                s.align_items = Some(taffy::AlignItems::START);
                s.padding.left =
                    taffy::LengthPercentage::length((left - f64::from(rect.left)) as f32);
                s.padding.top = taffy::LengthPercentage::length(pad_top);
                s.padding.bottom = taffy::LengthPercentage::length(pad_bottom);
            })
            .children(rows);
        let frame = tree.layout(root, view);
        // Centre the focused card's row, eased, while no finger has the grid.
        let (_, max) = frame.scroll(grid).expect("the grid is a scroll");
        let target = frame
            .rect(card_id(self.cursor))
            .map_or(0.0, |r| (r.center_y() - rect.center_y()).clamp(0.0, max));
        if !tree.moving(grid) {
            let next = approach(f64::from(tree.offset(grid)), f64::from(target), dt, 0.08) as f32;
            tree.set_offset(
                grid,
                if (next - target).abs() < 0.25 {
                    target
                } else {
                    next
                },
            );
        }
        tree.set_focus(Some(card_id(self.cursor)));
        let cheap = super::settings::rows::reduce_ui_res(
            ctx.settings,
            ctx.device.platform,
            ctx.device.fallback_ui,
        );
        if view == rect {
            let scrolled = (tree.offset(grid), max);
            crate::widgets::soft_scroll(canvas, rect, rect, scrolled, k, || {
                tree.paint_focus(canvas, frame, k as f32, dt, cheap);
            });
        } else {
            tree.paint_focus(canvas, frame, k as f32, dt, cheap);
        }
        self.geom = (0..PALETTES.len())
            .map(|i| tree.rect(card_id(i)).unwrap_or_else(Rect::new_empty))
            .collect();
        self.tree = tree;
    }

    fn title(&self) -> String {
        "Background".into()
    }
}

/// One card: the palette's field in miniature, its name in its own ink, a check when chosen.
fn draw_card(canvas: &Canvas, fonts: &Fonts, p: &Palette, rect: Rect, chosen: bool, k: f64) {
    let corner = (CARD_CORNER * k * f64::from(rect.width()) / (CARD_W * k)) as f32;
    let rr = RRect::new_rect_xy(rect, corner, corner);
    let rgb = |(r, g, b): (f64, f64, f64), a: f32| Color4f::new(r as f32, g as f32, b as f32, a);
    canvas.save();
    canvas.clip_rrect(rr, ClipOp::Intersect, true);
    // The field's gradient along its backdrop diagonal: the stops the shader blends,
    // without the noise. Enough to tell the palettes apart at card size.
    let colors: Vec<Color4f> = p
        .stops
        .unwrap_or(&VIOLET_FIELD)
        .iter()
        .map(|c| rgb(*c, 1.0))
        .collect();
    let mut paint = fill(rgb(p.ground, 1.0));
    paint.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            Point::new(rect.left, rect.top),
            Point::new(rect.right, rect.bottom),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(&colors, TileMode::Clamp, None),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rect(rect, &paint);
    canvas.restore();
    // Ink is the card's, not the shell's: a pale card under a dark shell still reads.
    let ink = if p.light {
        Color4f::new(0.08, 0.07, 0.12, 1.0)
    } else {
        Color4f::new(1.0, 1.0, 1.0, 1.0)
    };
    let hair = (k.max(1.0)) as f32;
    canvas.draw_rrect(
        rr.with_inset((hair / 2.0, hair / 2.0)),
        &stroke(Color4f { a: 0.22, ..ink }, hair),
    );
    let pad = 16.0 * k;
    fonts.draw_clipped(
        canvas,
        p.name,
        f64::from(rect.left) + pad,
        f64::from(rect.bottom) - pad,
        W::Bold,
        17.0 * k,
        ink,
        f64::from(rect.width()) - 2.0 * pad,
    );
    if chosen {
        let r = (13.0 * k) as f32;
        let (cx, cy) = (rect.right - pad as f32 - r, rect.top + pad as f32 + r);
        canvas.draw_circle((cx, cy), r, &fill(rgb(p.accent, 1.0)));
        let on_accent = if crate::theme::luma(p.accent) > 0.6 {
            Color4f::new(0.05, 0.05, 0.08, 1.0)
        } else {
            Color4f::new(1.0, 1.0, 1.0, 1.0)
        };
        if let Some(icon) = by_name("check") {
            draw_icon_weight(canvas, icon, cx, cy, r * 1.3, (2.2 * k) as f32, on_accent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::Outbox;
    use pf_client_core::trust::Settings;

    /// Opens on the palette in force; a pick lands in the store, the same pick is a thud.
    #[test]
    fn confirm_saves_the_focused_palette_once() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        ctx.store.save(ctx.settings);
        let mut s = PaletteScreen::new("graphite");
        assert_eq!(PALETTES[s.cursor].id, "graphite");
        let mut fx = Outbox::default();
        assert!(matches!(
            s.menu(MenuEvent::Confirm, &mut ctx, &mut fx),
            Some(MenuPulse::Confirm)
        ));
        assert_eq!(ctx.store.load().ui_palette, "graphite");
        assert!(matches!(
            s.menu(MenuEvent::Confirm, &mut ctx, &mut fx),
            Some(MenuPulse::Boundary)
        ));
        assert_eq!(
            s.announcement(&ctx).as_deref(),
            Some("Graphite, dark, selected")
        );
        s.menu(MenuEvent::Back, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
        assert_eq!(
            PaletteScreen::new("chartreuse").cursor,
            0,
            "unknown opens on the default"
        );
    }
}
