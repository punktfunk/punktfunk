//! Shared styling primitives for every screen, following the windows-reactor gallery's look:
//! theme brushes (`ThemeRef`), rounded `border` cards, small all-caps section labels, and a
//! centred max-width column per page.

use windows_reactor::*;

pub(crate) fn uniform(v: f64) -> Thickness {
    Thickness::uniform(v)
}

pub(crate) fn edges(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

/// A rounded, bordered surface in the theme's card colours.
pub(crate) fn card(child: impl Into<Element>) -> Border {
    border(child.into())
        .background(ThemeRef::CardBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(8.0)
        .padding(uniform(16.0))
}

/// Card chrome with no padding — for cards whose interactive regions (tap-to-connect area vs.
/// action buttons) must own their padding so hit areas reach the card edges.
pub(crate) fn card_flush(child: impl Into<Element>) -> Border {
    card(child).padding(uniform(0.0))
}

/// An OPAQUE modal/dialog surface. `card`'s `CardBackground` is a translucent acrylic brush — fine
/// layered on the page, but a floating dialog over a scrim needs a solid fill or the content behind
/// bleeds through (looks "transparent"). `SolidBackground` is the opaque base-layer brush.
pub(crate) fn dialog_surface(child: impl Into<Element>) -> Border {
    border(child.into())
        .background(ThemeRef::SolidBackground)
        .border_brush(ThemeRef::SurfaceStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(8.0)
        .padding(uniform(20.0))
}

/// A fully transparent brush: paints nothing but (unlike a null background) makes the whole
/// element hit-testable, so a tap region catches clicks in its blank space too.
pub(crate) fn hit_test_backstop() -> Color {
    Color {
        a: 0,
        r: 0,
        g: 0,
        b: 0,
    }
}

/// A small all-caps section label above a group of cards.
pub(crate) fn section(label: &str) -> Element {
    text_block(label)
        .font_size(12.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .margin(edges(2.0, 14.0, 0.0, 2.0))
        .into()
}

/// Wrap a screen's children in a scrollable, centred, max-width column. Alignment stays the
/// default Stretch: with a MaxWidth that still centres the column, but the children get the
/// column's REAL width — an explicit Center would size the column to its content and leave
/// every card at its minimum width no matter how large the window is.
pub(crate) fn page(children: Vec<Element>) -> Element {
    let col = vstack(children)
        .spacing(10.0)
        .max_width(640.0)
        .margin(edges(24.0, 24.0, 24.0, 40.0));
    scroll_view(col).into()
}

/// Like [`page`], but wide and airier — for screens whose cards lay out in a responsive grid
/// and should use the window instead of a narrow settings column.
pub(crate) fn page_wide(children: Vec<Element>) -> Element {
    let col = vstack(children)
        .spacing(14.0)
        .max_width(1120.0)
        .margin(edges(32.0, 28.0, 32.0, 48.0));
    scroll_view(col).into()
}

/// Lay tiles into a `cols`-wide grid of equal-width star columns (rows share the height of
/// their tallest tile, so a grid row always lines up). Shared by the hosts page's host
/// tiles and the library page's posters.
pub(crate) fn tile_grid(tiles: Vec<Element>, cols: usize, gap: f64) -> Element {
    let rows = tiles.len().div_ceil(cols);
    let mut children = Vec::with_capacity(tiles.len());
    for (i, t) in tiles.into_iter().enumerate() {
        children.push(t.grid_row((i / cols) as i32).grid_column((i % cols) as i32));
    }
    grid(children)
        .columns(vec![GridLength::Star(1.0); cols])
        .rows(vec![GridLength::Auto; rows])
        .column_spacing(gap)
        .row_spacing(gap)
        .into()
}

/// A page header: a large bold title on the left, one action button on the right.
pub(crate) fn page_header(title: &str, action: Button) -> Element {
    grid((
        text_block(title)
            .font_size(30.0)
            .bold()
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
        action
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center),
    ))
    .columns([GridLength::Star(1.0), GridLength::Auto])
    .margin(edges(0.0, 0.0, 0.0, 6.0))
    .into()
}

/// A full-screen centred "busy" scene: spinner, headline, secondary detail line, and optional
/// trailing elements (e.g. a Cancel button). Shared by Connecting / RequestAccess / SpeedTest.
pub(crate) fn busy_page(headline: &str, detail: &str, extra: Vec<Element>) -> Element {
    let mut children: Vec<Element> = vec![
        ProgressRing::indeterminate()
            .width(48.0)
            .height(48.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
        text_block(headline)
            .font_size(18.0)
            .semibold()
            .wrap()
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
        text_block(detail)
            .wrap()
            .foreground(ThemeRef::SecondaryText)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into(),
    ];
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
pub(crate) fn avatar(name: &str, os: &str) -> Border {
    const SIZE: f64 = 44.0;
    // `Image`, not `Icon` — an Image takes an explicit size, and reactor's icon elements do
    // not. The marks bake white for this fill; see `os_icons`.
    let face: Element = match super::os_icons::uri(os) {
        Some(uri) => Image::new_with_uri(uri)
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
            text_block(initial)
                .font_size(17.0)
                .semibold()
                // NOT ThemeRef::AccentText — that's accent-COLOURED text for normal surfaces;
                // on an accent fill it's accent-on-accent (unreadable). The on-accent brush.
                .foreground(ThemeRef::custom("TextOnAccentFillColorPrimaryBrush"))
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
    };
    border(face)
        .background(ThemeRef::Accent)
        .corner_radius(SIZE / 2.0)
        .width(SIZE)
        .height(SIZE)
}

/// A circle of `size` carrying `text`: a profile's initials on its accent, or on the app accent
/// when `fill` is `None`.
pub(crate) fn monogram(text: &str, size: f64, fill: Option<Color>) -> Border {
    let label = text_block(text)
        .font_size(size * 0.38)
        .semibold()
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center);
    let disc = match fill {
        Some(c) => border(label.foreground(Color {
            a: 255,
            r: 255,
            g: 255,
            b: 255,
        }))
        .background(c),
        None => border(label.foreground(ThemeRef::custom("TextOnAccentFillColorPrimaryBrush")))
            .background(ThemeRef::Accent),
    };
    disc.corner_radius(size / 2.0).width(size).height(size)
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
pub(crate) fn pill(text: &str, kind: Pill) -> Border {
    let (bg, fg) = match kind {
        Pill::Info => (
            ThemeRef::SystemAttentionBackground,
            ThemeRef::SystemAttention,
        ),
        Pill::Good => (ThemeRef::SystemSuccessBackground, ThemeRef::SystemSuccess),
        Pill::Neutral => (ThemeRef::SubtleFill, ThemeRef::SecondaryText),
    };
    border(text_block(text).font_size(11.0).semibold().foreground(fg))
        .background(bg)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(10.0)
        .padding(edges(9.0, 2.0, 9.0, 2.0))
}

/// A small presence dot (host online/offline).
pub(crate) fn presence_dot(online: bool) -> Border {
    border(vstack(Vec::<Element>::new()))
        .background(if online {
            ThemeRef::SystemSuccess
        } else {
            ThemeRef::SystemNeutral
        })
        .corner_radius(4.0)
        .width(8.0)
        .height(8.0)
}
