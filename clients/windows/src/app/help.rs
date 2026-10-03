//! The Shortcuts screen: a short note on the in-stream capture model plus the keyboard,
//! touch and controller controls — reached from the Shortcuts button on the host list. The
//! Windows counterpart of the GTK client's Keyboard Shortcuts window; both read
//! `pf_client_core::shortcuts`.

use super::lucide;
use super::style::*;
use super::Screen;
use pf_client_core::shortcuts;
use windows_reactor::*;

/// A subtle key-cap chip for the shortcuts reference — the chord on a filled, bordered pill.
fn key_chip(keys: &str) -> Element {
    border(text_block(keys).font_size(12.0).semibold())
        .background(ThemeRef::SubtleFill)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(6.0)
        .padding(edges(8.0, 3.0, 8.0, 3.0))
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// A read-only reference card per group of the shared list: keyboard, touch, controller. One
/// grid per card, chord chip then action, so the actions line up across rows.
fn shortcuts_reference() -> Vec<Element> {
    shortcuts::groups(shortcuts::Client::Desktop, true)
        .into_iter()
        .map(|g| {
            let mut children: Vec<Element> = Vec::new();
            for (i, item) in g.items.iter().enumerate() {
                let row = i as i32;
                children.push(key_chip(item.keys).grid_row(row).grid_column(0));
                let action_cell: Element = text_block(item.text)
                    .wrap()
                    .foreground(ThemeRef::SecondaryText)
                    .vertical_alignment(VerticalAlignment::Center)
                    .into();
                children.push(action_cell.grid_row(row).grid_column(1));
            }
            let table = grid(children)
                .columns([GridLength::Auto, GridLength::Star(1.0)])
                .rows(vec![GridLength::Auto; g.items.len()])
                .column_spacing(12.0)
                .row_spacing(6.0);
            card(vstack((
                text_block(g.title)
                    .semibold()
                    .margin(edges(0.0, 0.0, 0.0, 8.0)),
                table,
            )))
            .into()
        })
        .collect()
}

/// The Shortcuts screen: a `page`-column with a Back button to the host list, an intro card on
/// the capture model, and the shortcuts reference. Hook-free — called inline from `root` like
/// the other static screens.
pub(crate) fn help_page(set_screen: &AsyncSetState<Screen>) -> Element {
    let back_btn = button("Back")
        .accent()
        .icon(lucide::icon("arrow-left"))
        .on_click({
            let ss = set_screen.clone();
            move || ss.call(Screen::Hosts)
        });

    let intro = card(
        vstack((
            text_block("During a stream").font_size(15.0).semibold(),
            text_block(
                "Click the stream to capture your mouse and keyboard \u{2014} the shortcuts below \
                 then work while you play. Release capture to hand the cursor back to this \
                 computer, and click the stream again to retake it.",
            )
            .font_size(12.0)
            .wrap()
            .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(8.0),
    );

    let mut children = vec![page_header("Shortcuts", back_btn), intro.into()];
    children.extend(shortcuts_reference());
    page(children)
}
