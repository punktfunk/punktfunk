//! The Shortcuts screen: a short note on the in-stream capture model plus the keyboard,
//! touch and controller controls — reached from the Shortcuts button on the host list. The
//! Windows counterpart of the GTK client's Keyboard Shortcuts window; both read
//! `pf_client_core::shortcuts`.

use super::style::*;
use super::{AsyncSetState, Screen};
use pf_client_core::shortcuts;
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common, desktop};
use windows_reactor::*;

/// A subtle key-cap chip for the shortcuts reference — the chord on a filled, bordered pill.
fn key_chip(pal: &Palette, keys: &str) -> Border {
    Border::new()
        .content(
            text(keys)
                .font_size(12.0)
                .font_weight(FontWeight::SEMI_BOLD),
        )
        .background(pal.get(Token::SubtleFill))
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(6.0)
        .padding(edges(8.0, 3.0, 8.0, 3.0))
        .horizontal_alignment(HorizontalAlignment::Left)
}

/// A read-only reference card per group of the shared list: keyboard, touch, controller. One
/// grid per card, chord chip then action, so the actions line up across rows.
fn shortcuts_reference(pal: &Palette) -> Vec<View> {
    shortcuts::groups(shortcuts::Client::Desktop, true)
        .into_iter()
        .map(|g| {
            let mut children: Vec<View> = Vec::new();
            for (i, item) in g.items.iter().enumerate() {
                let row = i as i32;
                children.push(
                    key_chip(pal, &item.keys.to_string())
                        .grid_row(row)
                        .grid_column(0)
                        .into(),
                );
                children.push(
                    muted(pal, item.text())
                        .text_wrapping(TextWrapping::Wrap)
                        .vertical_alignment(VerticalAlignment::Center)
                        .grid_row(row)
                        .grid_column(1)
                        .into(),
                );
            }
            let table = Grid::new()
                .children(children)
                .columns([GridLength::Auto, GridLength::STAR])
                .rows(vec![GridLength::Auto; g.items.len()])
                .column_spacing(12.0)
                .row_spacing(6.0);
            card(vstack((
                text(g.heading())
                    .font_weight(FontWeight::SEMI_BOLD)
                    .margin(edges(0.0, 0.0, 0.0, 8.0)),
                table,
            )))
            .into()
        })
        .collect()
}

/// The Shortcuts screen: a `page`-column with a Back button to the host list, an intro card on
/// the capture model, and the shortcuts reference.
pub(crate) fn help_page(pal: &Palette, set_screen: &AsyncSetState<Screen>) -> View {
    let back_btn = icon_button("arrow-left", common::common_back())
        .style(ButtonStyle::Accent)
        .on_click({
            let ss = set_screen.clone();
            move || ss.call(Screen::Hosts)
        });

    let intro = card(
        vstack((
            text(desktop::desktop_controls_intro_title())
                .font_size(15.0)
                .font_weight(FontWeight::SEMI_BOLD),
            text(desktop::desktop_controls_intro_body())
                .font_size(12.0)
                .text_wrapping(TextWrapping::Wrap)
                .foreground(pal.get(Token::SecondaryText)),
        ))
        .spacing(8.0),
    );

    let mut children = vec![
        page_header(client::controls_title(), back_btn),
        intro.into(),
    ];
    children.extend(shortcuts_reference(pal));
    page(children)
}
