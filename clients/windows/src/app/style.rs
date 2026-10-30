//! Shared styling primitives for every screen, following the WinUI gallery's look: theme
//! brushes (reactor's `ThemeBrush`, and the [`Palette`] for the rest), rounded cards, small
//! section labels, a centred max-width column per page, and the scrim sheet every overlay sits
//! on.

use super::lucide;
use pf_fluent::{Palette, Token};
use windows_reactor::*;

pub(crate) fn uniform(v: f64) -> Thickness {
    Thickness::uniform(v)
}

pub(crate) fn edges(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness::new(left, top, right, bottom)
}

pub(crate) fn text(s: impl AsRef<str>) -> TextBlock {
    TextBlock::new().text(s)
}

/// Text in the theme's secondary text colour: captions, details, hints.
pub(crate) fn muted(pal: &Palette, s: impl AsRef<str>) -> TextBlock {
    text(s).foreground(pal.get(Token::SecondaryText))
}

pub(crate) fn vstack(children: impl IntoViews) -> StackPanel {
    StackPanel::new().children(children)
}

pub(crate) fn hstack(children: impl IntoViews) -> StackPanel {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .children(children)
}

/// A blank element of a fixed size: a coloured dot or a divider once it has a background.
pub(crate) fn blank(width: f64, height: f64) -> Border {
    Border::new().width(width).height(height)
}

pub(crate) fn button(label: impl Into<View>) -> Button {
    Button::new().content(label)
}

/// A button with a Lucide mark before its label, 8 px apart.
pub(crate) fn icon_button(mark: &str, label: impl Into<View>) -> Button {
    Button::new().content(
        hstack((
            lucide::mark(mark).vertical_alignment(VerticalAlignment::Center),
            label.into(),
        ))
        .spacing(8.0),
    )
}

/// A button showing only a Lucide mark. `name` is what a screen reader announces.
pub(crate) fn mark_button(mark: &str, name: &str) -> Button {
    Button::new()
        .content(lucide::mark(mark))
        .automation_name(name)
}

/// A rounded, bordered surface in the theme's card colours.
pub(crate) fn card(child: impl Into<View>) -> Border {
    Border::new()
        .content(child)
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(8.0)
        .padding(uniform(16.0))
}

/// Card chrome with no padding — for cards whose interactive regions (tap-to-connect area vs.
/// action buttons) must own their padding so hit areas reach the card edges.
pub(crate) fn card_flush(child: impl Into<View>) -> Border {
    card(child).padding(uniform(0.0))
}

/// An OPAQUE modal surface. `card`'s `CardBackground` is translucent, so content behind a
/// floating dialog would bleed through; `SolidBackground` is the opaque base layer.
pub(crate) fn dialog_surface(pal: &Palette, child: impl Into<View>) -> Border {
    Border::new()
        .content(child)
        .background(ThemeBrush::SolidBackground)
        .border_brush(pal.get(Token::SurfaceStroke))
        .border_thickness(uniform(1.0))
        .corner_radius(8.0)
        .padding(uniform(20.0))
}

/// A fully transparent brush: paints nothing but (unlike a null background) makes the whole
/// element hit-testable, so a tap region catches clicks in its blank space too.
pub(crate) fn hit_test_backstop() -> Color {
    Color::transparent()
}

/// A tap on `target`: a release of the press that started on it. A drag that leaves before
/// the release is no tap.
pub(crate) fn on_tap(target: Border, tap: impl Fn() + 'static) -> Border {
    let pressed = std::rc::Rc::new(std::cell::Cell::new(false));
    let down = pressed.clone();
    target
        .on_pointer_pressed(move |_| down.set(true))
        .on_pointer_exited({
            let pressed = pressed.clone();
            move |_| pressed.set(false)
        })
        .on_pointer_released(move |_| {
            if pressed.replace(false) {
                tap();
            }
        })
}

/// A modal sheet: `body` on an opaque surface, centred over a scrim that blocks the page. A
/// tap on the scrim runs `cancel`; a tap on the surface does not. Escape is the shell's
/// (`Msg::Escape`), so it reaches the sheet wherever focus is.
pub(crate) fn sheet(
    pal: &Palette,
    body: impl Into<View>,
    max_width: f64,
    progress: f64,
    cancel: impl Fn() + 'static,
) -> View {
    // A press inside the surface bubbles on to the scrim; the flag makes the scrim swallow
    // the release that ends it, wherever that lands.
    let inside = std::rc::Rc::new(std::cell::Cell::new(false));
    let surface = dialog_surface(pal, ScrollView::new().content(body))
        .on_pointer_pressed({
            let inside = inside.clone();
            move |_| inside.set(true)
        })
        .max_width(max_width)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(uniform(24.0))
        // The entrance: fade in and rise 28 px with `progress` 0 → 1.
        .opacity(progress)
        .translation(0.0, (1.0 - progress) * 28.0);
    on_tap(
        Border::new()
            .content(surface)
            .background(Color::argb((140.0 * progress) as u8, 0, 0, 0)),
        move || {
            if !inside.replace(false) {
                cancel();
            }
        },
    )
    .into()
}

/// A small all-caps section label above a group of cards.
pub(crate) fn section(pal: &Palette, label: &str) -> View {
    muted(pal, label)
        .font_size(12.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .margin(edges(2.0, 14.0, 0.0, 2.0))
        .into()
}

/// Wrap a screen's children in a scrollable, centred, max-width column. Alignment stays the
/// default Stretch: with a MaxWidth that still centres the column, but the children get the
/// column's REAL width — an explicit Center would size the column to its content and leave
/// every card at its minimum width no matter how large the window is.
pub(crate) fn page(children: Vec<View>) -> View {
    let col = vstack(children)
        .spacing(10.0)
        .max_width(640.0)
        .margin(edges(24.0, 24.0, 24.0, 40.0));
    ScrollView::new().content(col).into()
}

/// Like [`page`], but wide and airier — for screens whose cards lay out in a responsive grid
/// and should use the window instead of a narrow settings column.
pub(crate) fn page_wide(children: Vec<View>) -> View {
    let col = vstack(children)
        .spacing(14.0)
        .max_width(1120.0)
        .margin(edges(32.0, 28.0, 32.0, 48.0));
    ScrollView::new().content(col).into()
}

/// Lay tiles into a `cols`-wide grid of equal-width star columns (rows share the height of
/// their tallest tile, so a grid row always lines up). Shared by the hosts page's host
/// tiles and the library page's posters.
pub(crate) fn tile_grid(tiles: Vec<View>, cols: usize, gap: f64) -> View {
    let rows = tiles.len().div_ceil(cols);
    let cells: Vec<View> = tiles
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            Border::new()
                .content(t)
                .grid_row((i / cols) as i32)
                .grid_column((i % cols) as i32)
                .into()
        })
        .collect();
    Grid::new()
        .children(cells)
        .columns(vec![GridLength::Star(1.0); cols])
        .rows(vec![GridLength::Auto; rows])
        .column_spacing(gap)
        .row_spacing(gap)
        .into()
}

/// A page header: a large bold title on the left, one action on the right.
pub(crate) fn page_header(title: &str, action: impl Into<View>) -> View {
    Grid::new()
        .children((
            text(title)
                .font_size(30.0)
                .font_weight(FontWeight::BOLD)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            Border::new()
                .content(action)
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::STAR, GridLength::Auto])
        .margin(edges(0.0, 0.0, 0.0, 6.0))
        .into()
}

/// A full-screen centred "busy" scene: [`notice_page`] under a spinner. Shared by Connecting /
/// RequestAccess / Waking / SpeedTest.
pub(crate) fn busy_page(pal: &Palette, headline: &str, detail: &str, extra: Vec<View>) -> View {
    let ring = ProgressRing::new()
        .is_indeterminate(true)
        .width(48.0)
        .height(48.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into();
    notice_page(pal, Some(ring), headline, detail, extra)
}

/// A full-screen centred scene: an optional lead visual, headline, secondary detail line, and
/// trailing elements (e.g. a Cancel button).
pub(crate) fn notice_page(
    pal: &Palette,
    lead: Option<View>,
    headline: &str,
    detail: &str,
    extra: Vec<View>,
) -> View {
    let mut children: Vec<View> = lead.into_iter().collect();
    children.extend([
        text(headline)
            .font_size(18.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .text_wrapping(TextWrapping::Wrap)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
        muted(pal, detail)
            .text_wrapping(TextWrapping::Wrap)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    ]);
    children.extend(extra);
    // max_width + side margins so the text column reads well wide AND wraps instead of
    // clipping narrow.
    vstack(children)
        .spacing(16.0)
        .max_width(520.0)
        .margin(edges(24.0, 0.0, 24.0, 0.0))
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// The host card's leading visual: a circle on an accent fill carrying the host's OS mark —
/// which machine this is answers a more useful question than which letter its name starts with,
/// and the name is spelled out directly under the circle anyway. A host that advertises no OS
/// chain (an older one) keeps the monogram, so nothing regresses to an empty circle.
pub(crate) fn avatar(pal: &Palette, name: &str, os: &str) -> Border {
    const SIZE: f64 = 44.0;
    // The marks bake white for this fill; see `os_icons`.
    let face: View = match super::os_icons::image(os) {
        Some(mark) => Image::new()
            .source_data(mark)
            .stretch(Stretch::Uniform)
            .width(SIZE * 0.5)
            .height(SIZE * 0.5)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        None => {
            let initial = name
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".into());
            text(initial)
                .font_size(17.0)
                .font_weight(FontWeight::SEMI_BOLD)
                // Not AccentText: that is accent-coloured text for normal surfaces, unreadable
                // on an accent fill.
                .foreground(pal.get(Token::TextOnAccent))
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
    };
    Border::new()
        .content(face)
        .background(ThemeBrush::Accent)
        .corner_radius(SIZE / 2.0)
        .width(SIZE)
        .height(SIZE)
}

/// A circle of `size` carrying `text`: a profile's initials on its accent, or on the app accent
/// when `fill` is `None`.
pub(crate) fn monogram(pal: &Palette, label: &str, size: f64, fill: Option<Color>) -> Border {
    let label = text(label)
        .font_size(size * 0.38)
        .font_weight(FontWeight::SEMI_BOLD)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center);
    let disc = match fill {
        Some(c) => Border::new()
            .content(label.foreground(Color::rgb(255, 255, 255)))
            .background(c),
        None => Border::new()
            .content(label.foreground(pal.get(Token::TextOnAccent)))
            .background(ThemeBrush::Accent),
    };
    disc.corner_radius(size / 2.0).width(size).height(size)
}

/// A profile's face: its picture, else its initials on `fill`.
pub(crate) fn profile_face(
    pal: &Palette,
    name: &str,
    picture: Option<EncodedImage>,
    size: f64,
    fill: Option<Color>,
) -> Border {
    let Some(picture) = picture else {
        return monogram(pal, &pf_client_core::profiles::initials(name), size, fill);
    };
    let image = Image::new()
        .source_data(picture)
        .stretch(Stretch::UniformToFill)
        .width(size)
        .height(size);
    Border::new()
        .content(image)
        .corner_radius(size / 2.0)
        .width(size)
        .height(size)
}

/// Pill chip colour intent.
#[derive(Clone, Copy)]
pub(crate) enum Pill {
    Info,
    Good,
    Neutral,
}

/// A small rounded status chip (paired/PIN/HDR/etc.) — subtle tinted fills with matching
/// system foregrounds (the InfoBar palette), never solid accent (white-on-bright is unreadable).
pub(crate) fn pill(pal: &Palette, label: &str, kind: Pill) -> Border {
    let (bg, fg) = match kind {
        Pill::Info => (Token::SystemAttentionBackground, Token::SystemAttention),
        Pill::Good => (Token::SystemSuccessBackground, Token::SystemSuccess),
        Pill::Neutral => (Token::SubtleFill, Token::SecondaryText),
    };
    Border::new()
        .content(
            text(label)
                .font_size(11.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(pal.get(fg)),
        )
        .background(pal.get(bg))
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(10.0)
        .padding(edges(9.0, 2.0, 9.0, 2.0))
}

/// A small presence dot (host online/offline).
pub(crate) fn presence_dot(pal: &Palette, online: bool) -> Border {
    blank(8.0, 8.0)
        .background(pal.get(if online {
            Token::SystemSuccess
        } else {
            Token::SystemNeutral
        }))
        .corner_radius(4.0)
}
