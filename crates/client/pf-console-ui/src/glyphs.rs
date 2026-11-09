//! Face-button glyphs, device marks, and the bottom-leading hint-bar pill.
//!
//! Style is the last driver (`Shell::glyph_style`): PlayStation shapes, Nintendo
//! engravings, ABXY letters, desktop keycaps, or Android TV-remote marks. A
//! remote has no Y/X/shoulders; those hints resolve to `None` and take no layout.
//! Apple draws SF Symbols via `sfSymbolsName`; this crate draws the shapes.
//!
//! Pin pads with [`GlyphStyle::from_pref`]. Keyboard vs Remote is the shell
//! (platform). Tests in this file cover the Nintendo swap and the remote hide-set.

use crate::icons::{self, Icon};
use crate::platform::Platform;
use crate::theme::{fill, stroke, Fonts, Frame, GLYPH_STROKE, HAIRLINE, W};
use pf_client_core::shortcuts::Press;
use punktfunk_core::config::GamepadPref;
use skia_safe::{Canvas, Color4f, PathBuilder, Point, RRect, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlyphStyle {
    /// Xbox / Steam Deck / generic.
    Letters,
    /// DualSense / DualShock 4.
    Shapes,
    /// Switch: south is B, east is A. Without this the legend says "A Select"
    /// over the engraved B.
    Nintendo,
    /// Desktop keys: keycaps, not a remote.
    Keyboard,
    /// Android keys: TV remote. No Y/X/shoulders — those hints resolve to `None`.
    Remote,
}

impl GlyphStyle {
    /// Pad family from [`PadInfo::pref`](pf_client_core::menu_nav::PadInfo).
    /// `None` is Keyboard. Keyboard vs Remote is the shell (platform).
    pub fn from_pref(pref: Option<GamepadPref>) -> GlyphStyle {
        match pref {
            Some(GamepadPref::DualSense | GamepadPref::DualSenseEdge | GamepadPref::DualShock4) => {
                GlyphStyle::Shapes
            }
            Some(
                GamepadPref::SwitchPro
                | GamepadPref::EightBitDoPro2
                | GamepadPref::EightBitDoPro3
                | GamepadPref::JoyConPair
                | GamepadPref::Switch2Pro
                | GamepadPref::Switch2GameCube,
            ) => GlyphStyle::Nintendo,
            Some(_) => GlyphStyle::Letters,
            None => GlyphStyle::Keyboard,
        }
    }

    /// The platform's key device. TV platforms get a remote, and so does Apple,
    /// where this shell's key device is the Siri Remote.
    pub fn keys(platform: Platform) -> GlyphStyle {
        match platform {
            Platform::Android
            | Platform::WebOS
            | Platform::Apple
            | Platform::Tizen
            | Platform::Ps5
            | Platform::Switch => GlyphStyle::Remote,
            Platform::Desktop | Platform::Web => GlyphStyle::Keyboard,
        }
    }
}

/// A pad's family silhouette. `None` is the platform's key device. `PadInfo.pref`
/// is already resolved; a stray `Auto` draws the Xbox 360 fallback.
pub fn device_icon(pref: Option<GamepadPref>, platform: Platform) -> Icon {
    let Some(pref) = pref else {
        return match GlyphStyle::keys(platform) {
            GlyphStyle::Remote => icons::REMOTE,
            _ => icons::KEYBOARD,
        };
    };
    pad_icon(pref)
}

/// A pad kind's family silhouette; `Auto` draws the Xbox 360 fallback.
pub fn pad_icon(pref: GamepadPref) -> Icon {
    match pref {
        GamepadPref::Auto | GamepadPref::Xbox360 => icons::PAD_XBOX_360,
        // No mark of their own: the nearest silhouette.
        GamepadPref::XboxOne | GamepadPref::EightBitDoUltimate2 | GamepadPref::HoripadSteam => {
            icons::PAD_XBOX_ONE
        }
        GamepadPref::XboxElite => icons::PAD_XBOX_ELITE,
        GamepadPref::DualShock4 => icons::PAD_DUALSHOCK_4,
        GamepadPref::DualSense => icons::PAD_DUALSENSE,
        GamepadPref::DualSenseEdge => icons::PAD_DUALSENSE_EDGE,
        GamepadPref::SwitchPro
        | GamepadPref::EightBitDoPro2
        | GamepadPref::EightBitDoPro3
        | GamepadPref::JoyConPair
        | GamepadPref::Switch2Pro
        | GamepadPref::Switch2GameCube => icons::PAD_SWITCH_PRO,
        GamepadPref::SteamController => icons::PAD_STEAM_CONTROLLER,
        GamepadPref::SteamController2 => icons::PAD_STEAM_CONTROLLER_2,
        GamepadPref::SteamController2Puck => icons::PAD_STEAM_PUCK,
        GamepadPref::SteamDeck => icons::PAD_STEAM_DECK,
    }
}

/// One device mark: left edge `x`, centred on `cy`, in a `w`-wide 24-unit box. The pen is
/// [`GLYPH_STROKE`] from 24 dp up; under that it thins with the box, or the 15 dp chip's
/// silhouette clogs round its buttons.
pub fn pad_mark(canvas: &Canvas, icon: Icon, x: f64, cy: f64, w: f64, k: f64, ink: Color4f) {
    let weight = GLYPH_STROKE * k * (w / (24.0 * k)).min(1.0);
    icons::draw_icon_weight(
        canvas,
        icon,
        (x + w / 2.0) as f32,
        cy as f32,
        w as f32,
        weight as f32,
        ink,
    );
}

/// Outline is always the full cell so the chip never reflows as bars drop.
///
/// Charging uses accent and outranks the low warning (4 % on the cable is
/// not 4 % off it). Under 20 % is a fixed red, not accent: on `moss`/`mint`
/// the accent means "fine".
pub fn battery_pip(
    canvas: &Canvas,
    ui: &Frame,
    x: f64,
    cy: f64,
    w: f64,
    k: f64,
    b: pf_client_core::menu_nav::PadBattery,
) {
    let h = w * 0.5;
    let cell = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, (w * 0.86) as f32, h as f32);
    let ink = if b.charging {
        ui.accent(1.0)
    } else if b.percent < 20 {
        skia_safe::Color4f::new(0.93, 0.31, 0.28, 1.0)
    } else {
        ui.fg(0.7)
    };
    let outline = stroke(ink, (HAIRLINE * k) as f32);
    let r = (2.0 * k) as f32;
    canvas.draw_rrect(RRect::new_rect_xy(cell, r, r), &outline);
    // Terminal nub — without it the cell reads as a text field.
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(
                cell.right + (1.5 * k) as f32,
                (cy - h * 0.22) as f32,
                (1.8 * k) as f32,
                (h * 0.44) as f32,
            ),
            r,
            r,
        ),
        &fill(ink),
    );
    // Ceil so any remaining charge shows at least one bar; empty-looking-but-alive reads as broken.
    let filled = ((f32::from(b.percent) / 100.0) * 4.0)
        .ceil()
        .clamp(0.0, 4.0) as i32;
    let pad = (1.6 * k) as f32;
    let seg_w = (cell.width() - 2.0 * pad) / 4.0;
    for i in 0..filled {
        let sx = cell.left + pad + i as f32 * seg_w;
        canvas.draw_rect(
            Rect::from_xywh(
                sx + 0.4 * k as f32,
                cell.top + pad,
                seg_w - 0.8 * k as f32,
                cell.height() - 2.0 * pad,
            ),
            &fill(ink),
        );
    }
}

/// `Key` is a literal keycap in any style (Deck "Steam + X").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HintKey {
    Confirm,
    Back,
    Secondary,
    Tertiary,
    Shoulders,
    Adjust,
    /// Context menu when up is spare. The library grid spends up on rows, so
    /// Options hangs off [`HintKey::Tertiary`] there.
    Up,
    /// Settings on the home carousel. Shown instead of [`HintKey::Tertiary`]
    /// when no pad — a remote has no X.
    Down,
    Key(&'static str),
}

pub struct Hint {
    pub key: HintKey,
    pub label: String,
}

impl Hint {
    pub fn new(key: HintKey, label: impl Into<String>) -> Hint {
        Hint {
            key,
            label: label.into(),
        }
    }
}

const LABEL_SIZE: f64 = 14.0;
const BADGE_D: f64 = 22.0; // dp
/// The legend pill's inner pad, dp: a bar placed this far left of a column starts its
/// first glyph on it.
pub const HINT_PAD: f64 = 13.0;

pub struct HintBar {
    pub size: (f64, f64),
    /// Pointers have no face buttons, so this is the button bar.
    pub rects: Vec<(HintKey, Rect)>,
}

/// Anchored at its bottom-left.
#[allow(clippy::too_many_arguments)]
pub fn hint_bar(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    hints: &[Hint],
    style: GlyphStyle,
    x: f64,
    bottom: f64,
    k: f64,
) -> HintBar {
    if hints.is_empty() {
        return HintBar {
            size: (0.0, 0.0),
            rects: Vec::new(),
        };
    }
    let pad = HINT_PAD * k;
    let gap_hint = 18.0 * k;
    let gap_glyph = 7.0 * k;
    // Drop unresolvable hints before layout so they take no width or hit box.
    let shown: Vec<&Hint> = hints
        .iter()
        .filter(|h| resolved(h.key, style).is_some())
        .collect();
    if shown.is_empty() {
        return HintBar {
            size: (0.0, 0.0),
            rects: Vec::new(),
        };
    }
    let widths: Vec<(f64, f64)> = shown
        .iter()
        .map(|h| {
            (
                glyph_width(fonts, h.key, style, k).expect("filtered to resolvable"),
                fonts.measure(&h.label, W::SemiBold, LABEL_SIZE * k) as f64,
            )
        })
        .collect();
    let content_w: f64 = widths.iter().map(|(g, l)| g + gap_glyph + l).sum::<f64>()
        + gap_hint * (shown.len() - 1) as f64;
    let h = BADGE_D * k + 2.0 * pad;
    let w = content_w + 2.0 * pad;
    let rect = Rect::from_xywh((x) as f32, (bottom - h) as f32, w as f32, h as f32);
    // Scrim then shared glass. The pill sits on the aurora at full contrast; glass
    // alone does not separate it. Same construction as the toast.
    let corner = (h / 2.0 / k) as f32;
    canvas.draw_rrect(
        RRect::new_rect_xy(rect, (h / 2.0) as f32, (h / 2.0) as f32),
        &fill(ui.shade(0.30)),
    );
    crate::theme::panel(
        canvas,
        ui,
        rect,
        corner,
        None,
        crate::theme::PanelStroke::Plain(0.12),
        k as f32,
    );

    let cy = bottom - h / 2.0;
    let mut pen = x + pad;
    let mut rects = Vec::with_capacity(shown.len());
    for (hint, (gw, lw)) in shown.iter().zip(&widths) {
        // Half the gap to the neighbour so the hit box is generous without overlapping.
        rects.push((
            hint.key,
            Rect::from_xywh(
                (pen - gap_glyph / 2.0) as f32,
                (bottom - h) as f32,
                (gw + gap_glyph + lw + gap_hint / 2.0) as f32,
                h as f32,
            ),
        ));
        draw_glyph(canvas, ui, fonts, hint.key, style, pen, cy, k);
        pen += gw + gap_glyph;
        // +0.36 em ≈ half Geist cap-height (0.72 em), centering on the badge.
        fonts.draw(
            canvas,
            &hint.label,
            pen,
            cy + LABEL_SIZE * k * 0.36,
            W::SemiBold,
            LABEL_SIZE * k,
            ui.fg(0.85),
        );
        pen += lw + gap_hint;
    }
    HintBar {
        size: (w, h),
        rects,
    }
}

/// `None` when the style has no glyph; the hint takes no space.
fn glyph_width(fonts: &Fonts, key: HintKey, style: GlyphStyle, k: f64) -> Option<f64> {
    Some(match resolved(key, style)? {
        Resolved::Badge(_) | Resolved::Adjust => BADGE_D * k,
        Resolved::Shoulders => 2.0 * shoulder_w(fonts, k) + 3.0 * k,
        Resolved::Up | Resolved::Down | Resolved::Ok | Resolved::BackArrow => BADGE_D * k,
        Resolved::Key(text) => keycap_w(fonts, text, k),
    })
}

fn shoulder_w(fonts: &Fonts, k: f64) -> f64 {
    fonts.measure("L1", W::SemiBold, 10.0 * k) as f64 + 10.0 * k
}

fn keycap_w(fonts: &Fonts, text: &str, k: f64) -> f64 {
    fonts.measure(text, W::SemiBold, 11.0 * k) as f64 + 14.0 * k
}

enum Resolved {
    Badge(Face),
    Shoulders,
    Adjust,
    /// Style-free: a direction, not a pad-labelled button.
    Up,
    /// Style-free, same reason as [`Resolved::Up`].
    Down,
    Ok,
    BackArrow,
    Key(&'static str),
}

#[derive(Clone, Copy)]
enum Face {
    A,
    B,
    X,
    Y,
}

/// `None` when the style has no glyph (a remote has no Y/X). Advertising a
/// missing button is worse than silence. Touch loses those two bar buttons
/// in Remote too; Remote only rules while keys drove last, and each action
/// still has an on-screen path.
fn resolved(key: HintKey, style: GlyphStyle) -> Option<Resolved> {
    if style == GlyphStyle::Keyboard {
        return Some(match key {
            HintKey::Confirm => Resolved::Key("Enter"),
            HintKey::Back => Resolved::Key("Esc"),
            HintKey::Secondary => Resolved::Key("Y"),
            HintKey::Tertiary => Resolved::Key("X"),
            // Tab, not PgUp/PgDn: naming both makes the legend wider than the hint is worth.
            HintKey::Shoulders => Resolved::Key("Tab"),
            HintKey::Adjust => Resolved::Adjust,
            HintKey::Up => Resolved::Up,
            HintKey::Down => Resolved::Down,
            HintKey::Key(t) => Resolved::Key(t),
        });
    }
    if style == GlyphStyle::Remote {
        return match key {
            HintKey::Confirm => Some(Resolved::Ok),
            HintKey::Back => Some(Resolved::BackArrow),
            HintKey::Secondary | HintKey::Tertiary => None,
            // No shoulders: Up from the top row is the remote's section switcher.
            HintKey::Shoulders => Some(Resolved::Up),
            HintKey::Adjust => Some(Resolved::Adjust),
            HintKey::Up => Some(Resolved::Up),
            HintKey::Down => Some(Resolved::Down),
            HintKey::Key(t) => Some(Resolved::Key(t)),
        };
    }
    Some(match key {
        HintKey::Confirm => Resolved::Badge(Face::A),
        HintKey::Back => Resolved::Badge(Face::B),
        HintKey::Tertiary => Resolved::Badge(Face::X),
        HintKey::Secondary => Resolved::Badge(Face::Y),
        HintKey::Shoulders => Resolved::Shoulders,
        HintKey::Adjust => Resolved::Adjust,
        HintKey::Down => Resolved::Down,
        HintKey::Up => Resolved::Up,
        HintKey::Key(t) => Resolved::Key(t),
    })
}

/// Nintendo swaps both pairs (south is B, east is A).
fn face_letter(face: Face, style: GlyphStyle) -> &'static str {
    match (style, face) {
        (GlyphStyle::Nintendo, Face::A) => "B",
        (GlyphStyle::Nintendo, Face::B) => "A",
        (GlyphStyle::Nintendo, Face::X) => "Y",
        (GlyphStyle::Nintendo, Face::Y) => "X",
        (_, Face::A) => "A",
        (_, Face::B) => "B",
        (_, Face::X) => "X",
        (_, Face::Y) => "Y",
    }
}

/// One glyph, left edge at `x`, vertically centered on `cy`.
#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    key: HintKey,
    style: GlyphStyle,
    x: f64,
    cy: f64,
    k: f64,
) {
    let Some(resolved) = resolved(key, style) else {
        return;
    };
    match resolved {
        Resolved::Badge(face) => {
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(ui.fg(0.10)));
            canvas.draw_circle(
                center,
                r as f32,
                &stroke(ui.fg(0.32), (HAIRLINE * k) as f32),
            );
            if style == GlyphStyle::Shapes {
                let (r, w) = ((5.2 * k) as f32, (1.8 * k) as f32);
                draw_ps_shape(canvas, face, center, r, w, ui.fg(0.92));
            } else {
                let letter = face_letter(face, style);
                let size = 12.0 * k;
                let w = fonts.measure(letter, W::SemiBold, size) as f64;
                fonts.draw(
                    canvas,
                    letter,
                    x + r - w / 2.0,
                    cy + size * 0.36,
                    W::SemiBold,
                    size,
                    ui.fg(0.92),
                );
            }
        }
        Resolved::Ok => {
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(ui.fg(0.10)));
            canvas.draw_circle(
                center,
                r as f32,
                &stroke(ui.fg(0.32), (HAIRLINE * k) as f32),
            );
            let size = 9.5 * k;
            let w = fonts.measure("OK", W::SemiBold, size) as f64;
            fonts.draw(
                canvas,
                "OK",
                x + r - w / 2.0,
                cy + size * 0.36,
                W::SemiBold,
                size,
                ui.fg(0.92),
            );
        }
        Resolved::BackArrow => {
            let r = BADGE_D * k / 2.0;
            let center = Point::new((x + r) as f32, cy as f32);
            canvas.draw_circle(center, r as f32, &fill(ui.fg(0.10)));
            canvas.draw_circle(
                center,
                r as f32,
                &stroke(ui.fg(0.32), (HAIRLINE * k) as f32),
            );
            // 12.5 dp sits inside the 22 dp badge.
            crate::icons::draw_icon(
                canvas,
                crate::icons::CORNER_DOWN_LEFT,
                center.x,
                center.y,
                (12.5 * k) as f32,
                k as f32,
                ui.fg(0.92),
            );
        }
        Resolved::Shoulders => {
            let mut pen = x;
            for label in ["L1", "R1"] {
                let w = shoulder_w(fonts, k);
                let h = 15.0 * k;
                let rect = Rect::from_xywh(pen as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
                canvas.draw_rrect(
                    RRect::new_rect_xy(rect, (4.0 * k) as f32, (4.0 * k) as f32),
                    &fill(ui.fg(0.10)),
                );
                // Same hairline as the keycaps so every container shares one edge.
                canvas.draw_rrect(
                    RRect::new_rect_xy(rect, (4.0 * k) as f32, (4.0 * k) as f32),
                    &stroke(ui.fg(0.28), (HAIRLINE * k) as f32),
                );
                let size = 10.0 * k;
                let tw = fonts.measure(label, W::SemiBold, size) as f64;
                fonts.draw(
                    canvas,
                    label,
                    pen + (w - tw) / 2.0,
                    cy + size * 0.36,
                    W::SemiBold,
                    size,
                    ui.fg(0.92),
                );
                pen += w + 3.0 * k;
            }
        }
        g @ (Resolved::Up | Resolved::Down) => {
            // Direction, not a button: chevron, no badge.
            let r = BADGE_D * k / 2.0;
            let icon = if matches!(g, Resolved::Down) {
                crate::icons::CHEVRON_DOWN
            } else {
                crate::icons::CHEVRON_UP
            };
            crate::icons::draw_icon(
                canvas,
                icon,
                (x + r) as f32,
                cy as f32,
                (BADGE_D * k) as f32,
                k as f32,
                ui.fg(0.92),
            );
        }
        Resolved::Adjust => {
            let r = BADGE_D * k / 2.0;
            let (cx, cyf) = ((x + r) as f32, cy as f32);
            let b = (20.0 * k) as f32;
            crate::icons::draw_icon(
                canvas,
                crate::icons::CHEVRON_LEFT,
                cx - (4.6 * k) as f32,
                cyf,
                b,
                k as f32,
                ui.fg(0.92),
            );
            crate::icons::draw_icon(
                canvas,
                crate::icons::CHEVRON_RIGHT,
                cx + (4.6 * k) as f32,
                cyf,
                b,
                k as f32,
                ui.fg(0.92),
            );
        }
        Resolved::Key(text) => {
            let w = keycap_w(fonts, text, k);
            let h = 18.0 * k;
            let rect = Rect::from_xywh(x as f32, (cy - h / 2.0) as f32, w as f32, h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &fill(ui.fg(0.10)),
            );
            canvas.draw_rrect(
                RRect::new_rect_xy(rect, (5.0 * k) as f32, (5.0 * k) as f32),
                &stroke(ui.fg(0.28), (HAIRLINE * k) as f32),
            );
            let size = 11.0 * k;
            let tw = fonts.measure(text, W::SemiBold, size) as f64;
            fonts.draw(
                canvas,
                text,
                x + (w - tw) / 2.0,
                cy + size * 0.36,
                W::SemiBold,
                size,
                ui.fg(0.92),
            );
        }
    }
}

/// DualSense layout: Confirm=✕, Back=○, X-position=□, Y-position=△.
fn draw_ps_shape(canvas: &Canvas, face: Face, center: Point, r: f32, width: f32, ink: Color4f) {
    let mut p = stroke(ink, width);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_stroke_join(skia_safe::PaintJoin::Round);
    let (cx, cy) = (center.x, center.y);
    match face {
        Face::A => {
            canvas.draw_line((cx - r, cy - r), (cx + r, cy + r), &p);
            canvas.draw_line((cx - r, cy + r), (cx + r, cy - r), &p);
        }
        Face::B => {
            canvas.draw_circle(center, r * 1.1, &p);
        }
        Face::X => {
            canvas.draw_rect(Rect::from_xywh(cx - r, cy - r, 2.0 * r, 2.0 * r), &p);
        }
        Face::Y => {
            let mut tri = PathBuilder::new();
            tri.move_to((cx, cy - r * 1.2));
            tri.line_to((cx + r * 1.15, cy + r * 0.85));
            tri.line_to((cx - r * 1.15, cy + r * 0.85));
            tri.close();
            canvas.draw_path(&tri.detach(), &p);
        }
    }
}

/// A reference page's badges are this much bigger than the hint bar's: read, not glanced at.
const CHORD_K: f64 = 1.25;
/// Air either side of a word between badges, dp, and the word's size.
const WORD_GAP: f64 = 6.0;
const WORD_SIZE: f64 = 13.0;

/// One part of a [`Press`] as the reference page draws it.
#[derive(Debug, PartialEq)]
enum Piece {
    Glyph(HintKey),
    /// A TV remote's colour key.
    Colour(Color4f),
    /// `+`, or a word of the message around the badges ("Hold", "or").
    Word(String),
}

/// Stand-ins the hold and either messages are filled with, so the badges go where each
/// language puts its placeholders. Control characters: no message contains one.
const FIRST: char = '\u{1}';
const SECOND: char = '\u{2}';

/// "Select + A", "Ctrl+Alt+Shift+Q", "Hold Back", "F11 or Alt+Enter", a gesture's keycap.
fn pieces(press: Press, style: GlyphStyle) -> Vec<Piece> {
    let mut out = Vec::new();
    let (text, first, second) = match press {
        Press::Keys(keys) => {
            push_keys(&mut out, keys, style);
            return out;
        }
        Press::Gesture(gesture) => {
            out.push(Piece::Glyph(HintKey::Key(gesture.label())));
            return out;
        }
        Press::Hold(keys) => (pf_l10n::client::controls_hold(FIRST), keys, ""),
        Press::Either(a, b) => (pf_l10n::client::controls_either(FIRST, SECOND), a, b),
    };
    let mut word = String::new();
    for ch in text.chars() {
        let keys = match ch {
            FIRST => first,
            SECOND => second,
            _ => {
                word.push(ch);
                continue;
            }
        };
        push_word(&mut out, &mut word);
        push_keys(&mut out, keys, style);
    }
    push_word(&mut out, &mut word);
    out
}

/// The message text gathered so far, as one word between badges.
fn push_word(out: &mut Vec<Piece>, word: &mut String) {
    let w = word.trim();
    if !w.is_empty() {
        out.push(Piece::Word(w.to_string()));
    }
    word.clear();
}

/// Keys joined by `+`. A letter is a face button only in a pad style; the keyboard's own
/// letters stay keycaps. Back and OK are the remote's badges in [`GlyphStyle::Remote`],
/// keycaps elsewhere.
fn push_keys(out: &mut Vec<Piece>, keys: &'static str, style: GlyphStyle) {
    let pad = !matches!(style, GlyphStyle::Keyboard | GlyphStyle::Remote);
    let remote = style == GlyphStyle::Remote;
    for (j, key) in keys.split('+').map(str::trim).enumerate() {
        if j > 0 {
            out.push(Piece::Word("+".into()));
        }
        out.push(match key {
            "A" if pad => Piece::Glyph(HintKey::Confirm),
            "B" if pad => Piece::Glyph(HintKey::Back),
            "X" if pad => Piece::Glyph(HintKey::Tertiary),
            "Y" if pad => Piece::Glyph(HintKey::Secondary),
            "Back" if remote => Piece::Glyph(HintKey::Back),
            "OK" if remote => Piece::Glyph(HintKey::Confirm),
            "Red" => Piece::Colour(Color4f::new(0.90, 0.27, 0.22, 1.0)),
            "Green" => Piece::Colour(Color4f::new(0.24, 0.72, 0.33, 1.0)),
            "Yellow" => Piece::Colour(Color4f::new(0.95, 0.78, 0.20, 1.0)),
            "Blue" => Piece::Colour(Color4f::new(0.25, 0.50, 0.95, 1.0)),
            _ => Piece::Glyph(HintKey::Key(key)),
        });
    }
}

fn piece_width(fonts: &Fonts, piece: &Piece, style: GlyphStyle, k: f64) -> f64 {
    match piece {
        Piece::Glyph(key) => glyph_width(fonts, *key, style, k).unwrap_or(0.0),
        Piece::Colour(_) => BADGE_D * k,
        Piece::Word(w) => {
            f64::from(fonts.measure(w, W::Regular, WORD_SIZE * k)) + 2.0 * WORD_GAP * k
        }
    }
}

/// A chord's badges run this tall, px.
pub fn chord_height(k: f64) -> f64 {
    BADGE_D * k * CHORD_K
}

/// The width [`draw_chord`] takes, px.
pub fn chord_width(fonts: &Fonts, press: Press, style: GlyphStyle, k: f64) -> f64 {
    let k = k * CHORD_K;
    pieces(press, style)
        .iter()
        .map(|p| piece_width(fonts, p, style, k))
        .sum()
}

/// A chord's badges in a row, left edge at `x`, centred on `cy`.
#[allow(clippy::too_many_arguments)]
pub fn draw_chord(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    press: Press,
    style: GlyphStyle,
    x: f64,
    cy: f64,
    k: f64,
) {
    let k = k * CHORD_K;
    let mut pen = x;
    for piece in pieces(press, style) {
        let w = piece_width(fonts, &piece, style, k);
        match piece {
            Piece::Glyph(key) => draw_glyph(canvas, ui, fonts, key, style, pen, cy, k),
            Piece::Colour(colour) => {
                let r = BADGE_D * k / 2.0;
                let center = Point::new((pen + r) as f32, cy as f32);
                canvas.draw_circle(center, r as f32, &fill(colour));
                canvas.draw_circle(
                    center,
                    r as f32,
                    &stroke(ui.fg(0.25), (HAIRLINE * k) as f32),
                );
            }
            Piece::Word(word) => {
                let size = WORD_SIZE * k;
                fonts.draw(
                    canvas,
                    &word,
                    pen + WORD_GAP * k,
                    cy + size * 0.36,
                    W::Regular,
                    size,
                    ui.fg(0.5),
                );
            }
        }
        pen += w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chord splits into badges and the words between them; a letter is a face button
    /// only on a pad, and Back is the remote's arrow only on a remote.
    #[test]
    fn a_chord_splits_into_badges_and_words() {
        use Press::{Either, Hold, Keys};
        let word = |w: &str| Piece::Word(w.into());
        let p = pieces(Keys("L1 + R1 + Start + Select"), GlyphStyle::Letters);
        assert_eq!(p.len(), 7);
        assert_eq!(p[0], Piece::Glyph(HintKey::Key("L1")));
        assert_eq!(p[1], word("+"));
        let p = pieces(Hold("Back"), GlyphStyle::Remote);
        assert_eq!(p, [word("Hold"), Piece::Glyph(HintKey::Back)]);
        let p = pieces(Either("F11", "Alt+Enter"), GlyphStyle::Keyboard);
        assert!(p.len() == 5 && p[1] == word("or"));
        assert_eq!(
            pieces(Keys("Select + A"), GlyphStyle::Shapes)[2],
            Piece::Glyph(HintKey::Confirm)
        );
        assert_eq!(
            pieces(Keys("Ctrl+Alt+Shift+A"), GlyphStyle::Keyboard)[6],
            Piece::Glyph(HintKey::Key("A"))
        );
        assert!(matches!(
            pieces(Keys("Green"), GlyphStyle::Remote)[0],
            Piece::Colour(_)
        ));
        let tap = pf_client_core::shortcuts::Gesture::ThreeFingerTap;
        assert_eq!(
            pieces(Press::Gesture(tap), GlyphStyle::Remote),
            [Piece::Glyph(HintKey::Key("Three-finger tap"))]
        );
    }

    /// The words around the badges are the message's, wherever it puts its placeholders.
    #[test]
    fn a_held_chord_reads_the_pseudo_language() {
        let p = pf_l10n::with_language("en-XA", || pieces(Press::Hold("Back"), GlyphStyle::Remote));
        assert!(p.contains(&Piece::Glyph(HintKey::Back)), "{p:?}");
        assert!(
            matches!(&p[0], Piece::Word(w) if w.starts_with('[') && w != "Hold"),
            "{p:?}"
        );
    }

    /// A longer chord is wider, and the page's badges outsize the hint bar's.
    #[test]
    fn a_chord_measures_its_badges() {
        let fonts = crate::theme::build_fonts().unwrap();
        let one = chord_width(&fonts, Press::Keys("A"), GlyphStyle::Letters, 1.0);
        let two = chord_width(&fonts, Press::Keys("Select + A"), GlyphStyle::Letters, 1.0);
        assert!(one > BADGE_D && two > one + BADGE_D, "{one} {two}");
    }

    #[test]
    fn style_follows_pad_kind() {
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::DualSense)),
            GlyphStyle::Shapes
        );
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SteamDeck)),
            GlyphStyle::Letters
        );
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SteamController2)),
            GlyphStyle::Letters
        );
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SteamController2Puck)),
            GlyphStyle::Letters
        );
        assert_eq!(
            GlyphStyle::from_pref(Some(GamepadPref::SwitchPro)),
            GlyphStyle::Nintendo
        );
        assert_eq!(GlyphStyle::from_pref(None), GlyphStyle::Keyboard);
    }

    /// Every family draws its own mark; `Auto` and no pad fall back as documented.
    #[test]
    fn each_pad_family_draws_its_own_mark() {
        let mark = |p| device_icon(Some(p), Platform::Desktop).0;
        let families: Vec<_> = (1..12).map(GamepadPref::from_u8).collect();
        for (i, a) in families.iter().enumerate() {
            for b in &families[i + 1..] {
                assert_ne!(mark(*a), mark(*b), "{a:?} and {b:?} share a mark");
            }
        }
        assert_eq!(mark(GamepadPref::Auto), mark(GamepadPref::Xbox360));
        assert_eq!(device_icon(None, Platform::Desktop).0, icons::KEYBOARD.0);
        assert_eq!(device_icon(None, Platform::Android).0, icons::REMOTE.0);
    }

    #[test]
    fn nintendo_badges_read_the_pads_own_letters() {
        assert_eq!(face_letter(Face::A, GlyphStyle::Nintendo), "B");
        assert_eq!(face_letter(Face::B, GlyphStyle::Nintendo), "A");
        assert_eq!(face_letter(Face::X, GlyphStyle::Nintendo), "Y");
        assert_eq!(face_letter(Face::Y, GlyphStyle::Nintendo), "X");
        assert_eq!(face_letter(Face::A, GlyphStyle::Letters), "A");
    }

    #[test]
    fn remote_hides_the_buttons_a_remote_does_not_have() {
        assert!(resolved(HintKey::Secondary, GlyphStyle::Remote).is_none());
        assert!(resolved(HintKey::Tertiary, GlyphStyle::Remote).is_none());
        assert!(matches!(
            resolved(HintKey::Confirm, GlyphStyle::Remote),
            Some(Resolved::Ok)
        ));
        assert!(matches!(
            resolved(HintKey::Back, GlyphStyle::Remote),
            Some(Resolved::BackArrow)
        ));
        assert!(matches!(
            resolved(HintKey::Shoulders, GlyphStyle::Remote),
            Some(Resolved::Up)
        ));
        // Every other style still resolves every hint.
        for style in [
            GlyphStyle::Letters,
            GlyphStyle::Shapes,
            GlyphStyle::Nintendo,
            GlyphStyle::Keyboard,
        ] {
            for key in [
                HintKey::Confirm,
                HintKey::Back,
                HintKey::Secondary,
                HintKey::Tertiary,
                HintKey::Shoulders,
                HintKey::Adjust,
                HintKey::Up,
                HintKey::Down,
            ] {
                assert!(resolved(key, style).is_some(), "{style:?} lost a hint");
            }
        }
    }
}
