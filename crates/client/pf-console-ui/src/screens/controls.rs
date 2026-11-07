//! The stream controls: what to press while a stream runs, as the badges the hint bar
//! draws — a pad's face buttons in its own style, keycaps, a remote's colour keys — one
//! card per device. The rows are [`pf_client_core::shortcuts`], so a binding moves with the
//! code that binds it. Keys and a finger scroll; nothing on the page takes focus.

use crate::glyphs::{self, GlyphStyle, Hint, HintKey};
use crate::icons::{self, Icon};
use crate::pointer::Pointer;
use crate::screens::licenses::wrap;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::{panel, stroke, Fonts, Frame, PanelStroke, HAIRLINE, W};
use crate::widgets::{bleed_view, soft_scroll, Flick};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::shortcuts::{self, Client, Kind, Press};
use skia_safe::{Canvas, Rect};
use std::borrow::Cow;

/// The column's widest, design units: badges and a line of text read best under it.
const MAX_W: f64 = 960.0;
const CORNER: f32 = 18.0;
/// A card's inner margin, its header band, the least a row takes and the gap to the next.
const PAD: f64 = 22.0;
const HEAD_H: f64 = 58.0;
const ROW_MIN_H: f64 = 56.0;
const CARD_GAP: f64 = 18.0;
/// Air between the badges and the text. Under [`TEXT_MIN_W`] of text, badges go above it.
const COL_GAP: f64 = 24.0;
const TEXT_MIN_W: f64 = 240.0;
const TEXT_SIZE: f64 = 15.0;
const LINE_H: f64 = TEXT_SIZE * 1.4;

struct Card {
    kind: Kind,
    icon: Icon,
    title: &'static str,
    /// Quieter beside the title.
    note: Option<Cow<'static, str>>,
    style: GlyphStyle,
    rows: Vec<Row>,
    /// Laid out: top from the content's top, height and the badge column's width, px. A
    /// card off the page has no height.
    top: f64,
    h: f64,
    keys_w: f64,
    /// Badges above the text: the column was too narrow for both.
    stacked: bool,
}

struct Row {
    keys: Press,
    text: Cow<'static, str>,
    lines: Vec<String>,
    /// Top from the card's top and height, px.
    top: f64,
    h: f64,
}

pub(crate) struct ControlsScreen {
    cards: Vec<Card>,
    /// The keyboard card waits for a keyboard: an iPhone's or iPad's keys need one attached.
    keys_need_keyboard: bool,
    /// One is attached, as the shell last said ([`Self::set_keyboard`]).
    keyboard: bool,
    /// What the cards were laid out for: width and scale.
    laid: Option<(f32, f64)>,
    flick: Flick,
}

impl ControlsScreen {
    /// This platform's controls. The console is built for the device it runs on, so a
    /// Mac's build is what tells it from an iPad.
    pub(crate) fn new(ctx: &Ctx) -> ControlsScreen {
        use crate::platform::Platform;
        let client = match ctx.device.platform {
            Platform::Desktop => Client::Desktop,
            Platform::Android => Client::Android,
            Platform::Apple if cfg!(target_os = "macos") => Client::Mac,
            Platform::Apple => Client::Apple,
            Platform::WebOS => Client::WebOS,
            Platform::Web => Client::Web,
            Platform::Tizen => Client::Tizen,
            Platform::Ps5 => Client::Ps5,
        };
        ControlsScreen::with_client(client, ctx)
    }

    /// `client`'s controls. Pad chords draw in the first connected pad's style.
    fn with_client(client: Client, ctx: &Ctx) -> ControlsScreen {
        let pad = ctx.pads.first();
        let pad_style = pad.map_or(GlyphStyle::Letters, |p| GlyphStyle::from_pref(Some(p.pref)));
        let pad_icon = pad.map_or(icons::PAD_XBOX_360, |p| glyphs::pad_icon(p.pref));
        let cards = shortcuts::groups(client, !ctx.device.tv)
            .into_iter()
            .map(|g| {
                let (icon, style) = match g.kind {
                    Kind::Remote => (icons::REMOTE, GlyphStyle::Remote),
                    Kind::Keyboard => (icons::KEYBOARD, GlyphStyle::Keyboard),
                    // Back is the system's arrow here, not a pad's B.
                    Kind::Touch => (Icon(pf_client_core::lucide::POINTER), GlyphStyle::Remote),
                    Kind::Pad => (pad_icon, pad_style),
                };
                Card {
                    kind: g.kind,
                    icon,
                    title: g.title(),
                    note: g.note(),
                    style,
                    rows: g
                        .items
                        .iter()
                        .map(|i| Row {
                            keys: i.keys,
                            text: i.text(),
                            lines: Vec::new(),
                            top: 0.0,
                            h: 0.0,
                        })
                        .collect(),
                    top: 0.0,
                    h: 0.0,
                    keys_w: 0.0,
                    stacked: false,
                }
            })
            .collect();
        ControlsScreen {
            cards,
            keys_need_keyboard: client == Client::Apple,
            keyboard: false,
            laid: None,
            flick: Flick::default(),
        }
    }

    /// Whether a keyboard is attached, from the shell each frame: one can arrive or leave
    /// while the page is up.
    pub(crate) fn set_keyboard(&mut self, attached: bool) {
        if self.keyboard != attached {
            self.keyboard = attached;
            self.laid = None;
        }
    }

    /// Every card's rows for `width`: the badge column as wide as its widest chord, the text
    /// wrapped into what is left, or under the badges where that is too little.
    fn layout(&mut self, fonts: &Fonts, width: f32, k: f64) {
        let inner = f64::from(width) - 2.0 * PAD * k;
        let badge_h = glyphs::chord_height(k);
        let mut y = TEXT_SIZE * 1.45 * k + 18.0 * k;
        let no_keys = self.keys_need_keyboard && !self.keyboard;
        for card in &mut self.cards {
            if no_keys && card.kind == Kind::Keyboard {
                card.h = 0.0;
                continue;
            }
            card.keys_w = card
                .rows
                .iter()
                .map(|r| glyphs::chord_width(fonts, r.keys, card.style, k))
                .fold(0.0, f64::max);
            card.stacked = inner - card.keys_w - COL_GAP * k < TEXT_MIN_W * k;
            let text_w = if card.stacked {
                inner
            } else {
                inner - card.keys_w - COL_GAP * k
            };
            card.top = y;
            let mut ry = HEAD_H * k;
            for row in &mut card.rows {
                row.lines = wrap(&row.text, text_w as f32, |s| {
                    fonts.measure(s, W::Regular, TEXT_SIZE * k)
                });
                let text_h = row.lines.len() as f64 * LINE_H * k;
                row.top = ry;
                row.h = if card.stacked {
                    badge_h + text_h + 30.0 * k
                } else {
                    (text_h + 22.0 * k).max(ROW_MIN_H * k)
                };
                ry += row.h;
            }
            card.h = ry + 4.0 * k;
            y += card.h + CARD_GAP * k;
        }
        self.flick.content_h = y - CARD_GAP * k + 12.0 * k;
        self.flick.step = LINE_H * k;
    }

    fn draw_card(
        &self,
        canvas: &Canvas,
        ui: &Frame,
        fonts: &Fonts,
        card: &Card,
        rect: Rect,
        k: f64,
    ) {
        let (x0, top, w) = (
            f64::from(rect.left),
            f64::from(rect.top),
            f64::from(rect.width()),
        );
        panel(canvas, ui, rect, CORNER, None, PanelStroke::Edge, k as f32);
        let (lx, rx) = (x0 + PAD * k, x0 + w - PAD * k);
        let hcy = top + HEAD_H * k / 2.0;
        icons::draw_icon(
            canvas,
            card.icon,
            (lx + 12.0 * k) as f32,
            hcy as f32,
            (24.0 * k) as f32,
            k as f32,
            ui.fg(0.9),
        );
        let tx = lx + 38.0 * k;
        let tw = fonts.draw(
            canvas,
            card.title,
            tx,
            hcy + 18.0 * k * 0.36,
            W::SemiBold,
            18.0 * k,
            ui.fg(0.95),
        );
        if let Some(note) = &card.note {
            let nx = tx + f64::from(tw) + 14.0 * k;
            fonts.draw_clipped(
                canvas,
                note,
                nx,
                hcy + 14.0 * k * 0.36,
                W::Regular,
                14.0 * k,
                ui.fg(0.5),
                rx - nx,
            );
        }
        rule(canvas, ui, lx, rx, top + HEAD_H * k, 0.12, k);
        let badge_h = glyphs::chord_height(k);
        for (i, row) in card.rows.iter().enumerate() {
            let rt = top + row.top;
            if i > 0 {
                rule(canvas, ui, lx, rx, rt, 0.07, k);
            }
            let text_h = row.lines.len() as f64 * LINE_H * k;
            let (cy, tx, mut by) = if card.stacked {
                let cy = rt + 14.0 * k + badge_h / 2.0;
                (cy, lx, cy + badge_h / 2.0 + 10.0 * k + LINE_H * k * 0.72)
            } else {
                let cy = rt + row.h / 2.0;
                (
                    cy,
                    lx + card.keys_w + COL_GAP * k,
                    rt + (row.h - text_h) / 2.0 + LINE_H * k * 0.72,
                )
            };
            glyphs::draw_chord(canvas, ui, fonts, row.keys, card.style, lx, cy, k);
            for line in &row.lines {
                fonts.draw(canvas, line, tx, by, W::Regular, TEXT_SIZE * k, ui.fg(0.85));
                by += LINE_H * k;
            }
        }
    }
}

/// A hairline across the card at `y`.
fn rule(canvas: &Canvas, ui: &Frame, x1: f64, x2: f64, y: f64, alpha: f32, k: f64) {
    canvas.draw_line(
        (x1 as f32, y as f32),
        (x2 as f32, y as f32),
        &stroke(ui.fg(alpha), (HAIRLINE * k) as f32),
    );
}

impl ScreenView for ControlsScreen {
    fn title(&self) -> String {
        pf_l10n::client::controls_title().into()
    }

    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        self.flick.menu(ev)
    }

    fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        self.flick.pointer(p);
        true
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.flick.pan(p)
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Adjust, pf_l10n::console::console_hint_page()),
            Hint::new(HintKey::Back, pf_l10n::common::common_done()),
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
        let ui = ctx.ui;
        let w = (f64::from(rect.width()) - 2.0 * ui.edge(k)).min(MAX_W * k);
        let x0 = f64::from(rect.center_x()) - w / 2.0;
        let key = (w as f32, k);
        if self.laid != Some(key) {
            self.layout(fonts, w as f32, k);
            self.laid = Some(key);
        }
        self.flick.view_h = f64::from(rect.height());
        self.flick.reduced = ui.reduce_motion;
        self.flick.tick(dt);

        // With the trays on, the page runs on under the title and the legend and they blur
        // it, as a list does; without them it fades at its own edges.
        let view = bleed_view(canvas, rect);
        let paint = || {
            let top = f64::from(rect.top) + 6.0 * k - self.flick.scroll;
            fonts.draw(
                canvas,
                pf_l10n::client::controls_lead(),
                x0,
                top + TEXT_SIZE * k,
                W::Regular,
                TEXT_SIZE * k,
                ui.fg(0.66),
            );
            for card in &self.cards {
                let ct = top + card.top;
                let on_page = card.h > 0.0;
                if on_page && ct < f64::from(view.bottom) && ct + card.h > f64::from(view.top) {
                    let rect = Rect::from_xywh(x0 as f32, ct as f32, w as f32, card.h as f32);
                    self.draw_card(canvas, ui, fonts, card, rect, k);
                }
            }
        };
        if crate::blur::active() {
            paint();
        } else {
            soft_scroll(canvas, ui, view, view, self.flick.scrolled(), k, paint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::build_fonts;

    fn screen() -> ControlsScreen {
        let mut settings = pf_client_core::trust::Settings::default();
        let lib = crate::library::LibraryShared::default();
        ControlsScreen::new(&Ctx::test(&mut settings, &lib))
    }

    /// A desktop lists keys, touch and the pad; wide, badges and text share a row.
    #[test]
    fn a_desktop_lays_out_three_cards_in_two_columns() {
        let fonts = build_fonts().unwrap();
        let mut s = screen();
        s.layout(&fonts, 960.0, 1.0);
        assert_eq!(s.cards.len(), 3);
        assert!(s.cards.iter().all(|c| !c.stacked && c.keys_w > 100.0));
        assert!(s
            .cards
            .iter()
            .flat_map(|c| &c.rows)
            .all(|r| r.lines.len() == 1));
        assert!(s.flick.content_h > 600.0, "{}", s.flick.content_h);
    }

    /// Every title, note and row is a message; the chords stay key names.
    #[test]
    fn the_page_reads_the_pseudo_language() {
        let s = pf_l10n::with_language("en-XA", screen);
        let pseudo = |t: &str| t.starts_with('[');
        assert!(s.cards.iter().all(|c| pseudo(c.title)));
        assert!(s.cards.iter().filter_map(|c| c.note.as_deref()).all(pseudo));
        assert!(s
            .cards
            .iter()
            .flat_map(|c| &c.rows)
            .all(|r| pseudo(&r.text)));
    }

    /// An iPad's keys need a keyboard, which can arrive while the page is up. A Mac has one.
    #[test]
    fn an_ipad_lists_its_keys_only_while_a_keyboard_is_attached() {
        let fonts = build_fonts().unwrap();
        let mut settings = pf_client_core::trust::Settings::default();
        let lib = crate::library::LibraryShared::default();
        let ctx = Ctx::test(&mut settings, &lib);
        let on_page = |s: &mut ControlsScreen| {
            s.layout(&fonts, 960.0, 1.0);
            let shown = s.cards.iter().filter(|c| c.h > 0.0);
            shown.map(|c| c.kind).collect::<Vec<_>>()
        };

        let mut ipad = ControlsScreen::with_client(Client::Apple, &ctx);
        assert_eq!(on_page(&mut ipad), [Kind::Pad]);
        ipad.set_keyboard(true);
        assert!(
            ipad.laid.is_none(),
            "a keyboard arriving lays the page out again"
        );
        assert_eq!(on_page(&mut ipad), [Kind::Keyboard, Kind::Pad]);
        ipad.set_keyboard(false);
        assert_eq!(on_page(&mut ipad), [Kind::Pad]);

        let mut mac = ControlsScreen::with_client(Client::Mac, &ctx);
        assert_eq!(on_page(&mut mac), [Kind::Keyboard, Kind::Pad]);
    }

    /// A phone's width stacks the badges over the text; a narrower column wraps the text whole.
    #[test]
    fn a_narrow_column_stacks_badges_over_text() {
        let fonts = build_fonts().unwrap();
        let mut s = screen();
        s.layout(&fonts, 320.0, 1.0);
        assert!(s.cards.iter().all(|c| c.stacked));
        s.layout(&fonts, 250.0, 1.0);
        assert!(s
            .cards
            .iter()
            .flat_map(|c| &c.rows)
            .any(|r| r.lines.len() > 1));
    }
}
