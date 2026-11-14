//! Shared styling primitives for every screen, following the WinUI gallery's look: theme
//! brushes (reactor's `ThemeBrush`, and the [`Palette`] for the rest), rounded cards, groups of
//! rows, a page column, and the scrim sheet every overlay sits on.
//!
//! The type ramp is WinUI's: page title 28 semibold, group heading 14 semibold, body 14,
//! caption 12 in the secondary text colour. Spacing sits on a 4 px grid.

use super::lucide;
use pf_fluent::{Palette, Token};
use pf_l10n::common;
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

/// A button with a Lucide mark before its label, 8 px apart. The label is also its name: a
/// button whose content is a panel has none, and a screen reader announces "button".
pub(crate) fn icon_button(mark: &str, label: impl AsRef<str>) -> Button {
    let label = label.as_ref();
    Button::new()
        .content(
            hstack((
                lucide::mark(mark).vertical_alignment(VerticalAlignment::Center),
                text(label),
            ))
            .spacing(8.0),
        )
        .automation_name(label)
}

/// A button showing only a Lucide mark. `name` is what a screen reader announces.
pub(crate) fn mark_button(mark: &str, name: &str) -> Button {
    Button::new()
        .content(lucide::mark(mark))
        .automation_name(name)
}

/// One group of a header flyout: a heading over radio buttons, the choice in force picked.
/// `on_pick` runs for a pick that changes it.
pub(crate) fn choice_group(
    heading: &str,
    labels: Vec<String>,
    picked: Option<usize>,
    on_pick: impl Fn(usize) + 'static,
) -> View {
    RadioButtons::new()
        .header(text(heading).font_weight(FontWeight::SEMI_BOLD))
        .items_source(labels)
        .selected_index(picked)
        .on_selection_changed(move |i: Option<usize>| {
            if let Some(i) = i.filter(|i| Some(*i) != picked) {
                on_pick(i);
            }
        })
        .into()
}

/// A page header's Sort and group button over its [`choice_group`]s. A flyout, not a menu: a
/// button's menu opens upward, over the title bar, and has no placement to set.
pub(crate) fn arrange_flyout(name: &str, groups: Vec<View>) -> View {
    let body = vstack(groups).spacing(16.0).min_width(200.0);
    mark_button("arrow-up-down", name)
        .flyout_with(Flyout::rich(body).placement(FlyoutPlacement::BottomEdgeAlignedRight))
        .tooltip(name)
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

/// A small section label above a group of cards, drawn as written.
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
    scroller(col)
}

/// A page's vertical scroller. No horizontal bar: at a fractional scale the column measures a
/// pixel wider than the viewport, and the bar would show for it.
fn scroller(col: StackPanel) -> View {
    ScrollView::new()
        .content(col)
        .horizontal_scroll_bar_visibility(ScrollingScrollBarVisibility::Hidden)
        .into()
}

/// The gutter beside a page's content.
pub(crate) const GUTTER: f64 = 28.0;

/// Like [`page`], but as wide as the window gives: for pages whose cards lay out in a grid.
pub(crate) fn page_fill(children: Vec<View>) -> View {
    let col = vstack(children)
        .spacing(16.0)
        .margin(edges(GUTTER, 20.0, GUTTER, 40.0));
    scroller(col)
}

/// The widest a form's column grows.
const FORM_WIDTH: f64 = 760.0;

/// Like [`page`], at the width of a form: a host's page. The column starts at the gutter, where
/// every page's content starts, so it takes its width from the page's (`width`): a stack
/// aligned left sizes to its content, not to its maximum.
pub(crate) fn page_form(children: Vec<View>, width: f64) -> View {
    let col = vstack(children)
        .spacing(8.0)
        .width((width - 2.0 * GUTTER).clamp(240.0, FORM_WIDTH))
        .horizontal_alignment(HorizontalAlignment::Left)
        .margin(edges(GUTTER, 20.0, GUTTER, 40.0));
    scroller(col)
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

/// A page's title, at WinUI's title size.
pub(crate) fn page_title(title: &str) -> TextBlock {
    text(title)
        .font_size(28.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .text_trimming(TextTrimming::CharacterEllipsis)
}

/// A page header: the title on the left, the page's actions on the right.
pub(crate) fn page_header(title: &str, actions: Vec<View>) -> View {
    Grid::new()
        .children((
            page_title(title)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            hstack(actions)
                .spacing(8.0)
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .into()
}

/// A group's heading over its card.
pub(crate) fn group_heading(label: &str) -> View {
    text(label)
        .font_weight(FontWeight::SEMI_BOLD)
        .margin(edges(0.0, 16.0, 0.0, 0.0))
        .into()
}

/// A card of rows with a hairline between them: one group of a Windows Settings page.
pub(crate) fn row_card(pal: &Palette, rows: Vec<View>) -> View {
    let mut items: Vec<View> = Vec::with_capacity(rows.len() * 2);
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            items.push(
                Border::new()
                    .height(1.0)
                    .background(pal.get(Token::DividerStroke))
                    .into(),
            );
        }
        items.push(row);
    }
    card_flush(vstack(items)).into()
}

/// One row of a [`row_card`]: what it is on the left, with a caption under it where one says
/// more, and its control on the right.
pub(crate) fn row(
    pal: &Palette,
    title: &str,
    caption: Option<&str>,
    control: impl Into<View>,
) -> View {
    let mut words: Vec<View> = vec![text(title).text_wrapping(TextWrapping::Wrap).into()];
    words.extend(caption.filter(|c| !c.is_empty()).map(|c| {
        muted(pal, c)
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .into()
    }));
    Border::new()
        .content(
            Grid::new()
                .children((
                    vstack(words)
                        .spacing(2.0)
                        .grid_column(0)
                        .vertical_alignment(VerticalAlignment::Center),
                    Border::new()
                        .content(control)
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center),
                ))
                .columns([GridLength::STAR, GridLength::Auto])
                .column_spacing(16.0),
        )
        .padding(edges(16.0, 12.0, 16.0, 12.0))
        .min_height(56.0)
        .into()
}

/// A switch for a [`row`]: its state in words, then the switch at the row's edge. The control
/// keeps 154 px for labels of its own; the margin gives back the 114 the bare switch leaves.
/// `name` is the row's label, what a screen reader announces with the state.
pub(crate) fn switch(
    pal: &Palette,
    name: &str,
    on: bool,
    toggled: impl Fn(bool) + 'static,
) -> View {
    hstack((
        muted(
            pal,
            match on {
                true => common::common_on(),
                false => common::common_off(),
            },
        )
        .vertical_alignment(VerticalAlignment::Center),
        ToggleSwitch::new()
            .is_on(on)
            .on_content("")
            .off_content("")
            .on_toggled(toggled)
            .automation_name(name)
            .margin(edges(0.0, 0.0, -114.0, 0.0)),
    ))
    .spacing(12.0)
    .into()
}

/// A card that is a button: it takes focus, Enter and Space, and the control's own hover and
/// pressed fills, inset in the card as a list row's are. `content` fills the card; `name` is
/// what a screen reader announces.
pub(crate) fn card_button(
    content: impl Into<View>,
    name: &str,
    click: impl Fn() + 'static,
) -> Border {
    card_flush(
        Button::new()
            .content(content)
            .style(ButtonStyle::Subtle)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .automation_name(name)
            .on_click(click)
            .margin(uniform(3.0)),
    )
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

/// The fill of a host that is not paired: grey enough for the white mark on either theme.
const UNPAIRED_FILL: Color = Color {
    a: 255,
    r: 118,
    g: 118,
    b: 118,
};

/// A host's tile: its OS mark on the accent, or on grey while it is not paired. A host that
/// advertises no OS chain shows the first letter of its name.
pub(crate) fn host_tile(pal: &Palette, name: &str, os: &str, paired: bool, size: f64) -> Border {
    // The marks bake white for these fills; see `os_icons`.
    let face: View = match super::os_icons::image(os) {
        Some(mark) => Image::new()
            .source_data(mark)
            .stretch(Stretch::Uniform)
            .width(size * 0.5)
            .height(size * 0.5)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        None => {
            let initial = name
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".into());
            let label = text(initial)
                .font_size(size * 0.4)
                .font_weight(FontWeight::SEMI_BOLD)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center);
            // Not AccentText: that is accent-coloured text for normal surfaces, unreadable
            // on an accent fill.
            match paired {
                true => label.foreground(pal.get(Token::TextOnAccent)),
                false => label.foreground(Color::rgb(255, 255, 255)),
            }
            .into()
        }
    };
    let tile = Border::new()
        .content(face)
        .corner_radius(size * 0.18)
        .width(size)
        .height(size);
    match paired {
        true => tile.background(ThemeBrush::Accent),
        false => tile.background(UNPAIRED_FILL),
    }
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
    /// Over artwork: white on [`ART_SCRIM`].
    OnArt,
}

/// The veil behind a badge drawn over artwork. A cover is any colour, so a theme fill gives
/// no contrast there: white ink on this reads on all of them.
pub(crate) const ART_SCRIM: Color = Color::argb(160, 0, 0, 0);
pub(crate) const ART_INK: Color = Color::rgb(255, 255, 255);

/// A small rounded status chip (paired/PIN/HDR/etc.) — subtle tinted fills with matching
/// system foregrounds (the InfoBar palette), never solid accent (white-on-bright is unreadable).
pub(crate) fn pill(pal: &Palette, label: &str, kind: Pill) -> Border {
    let tokens = |bg, fg| (pal.get(bg), pal.get(fg));
    let (bg, fg) = match kind {
        Pill::Info => tokens(Token::SystemAttentionBackground, Token::SystemAttention),
        Pill::Good => tokens(Token::SystemSuccessBackground, Token::SystemSuccess),
        Pill::Neutral => tokens(Token::SubtleFill, Token::SecondaryText),
        Pill::OnArt => (ART_SCRIM.into(), ART_INK.into()),
    };
    Border::new()
        .content(
            text(label)
                .font_size(11.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .foreground(fg),
        )
        .background(bg)
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
