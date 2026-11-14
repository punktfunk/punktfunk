//! The Stream controls screen: a short note on the in-stream capture model plus this device's
//! keyboard, touch and controller shortcuts — reached from the Shortcuts button on the host
//! list, with Change into Settings → Shortcuts. The Windows counterpart of the GTK client's
//! Keyboard Shortcuts window; both read `pf_client_core::shortcuts`.

use super::style::*;
use super::{AppCtx, AsyncSetState, Screen};
use pf_client_core::overlay_actions::{OverlayConfig, RingPlatform};
use pf_client_core::shortcuts::Client;
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common, desktop};
use std::sync::Arc;
use windows_reactor::*;

/// A subtle key-cap chip for the shortcuts reference — the chord on a filled, bordered pill.
pub(super) fn key_chip(pal: &Palette, keys: &str) -> Border {
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

/// A read-only card per group of this device's shortcuts: keyboard, touch, controller. One
/// grid per card, chord chip then action, so the actions line up across rows.
fn shortcuts_reference(pal: &Palette, ctx: &AppCtx) -> Vec<View> {
    let s = ctx.settings.lock().unwrap().clone();
    let ring = OverlayConfig::parse(&s.overlay_actions, RingPlatform::Desktop);
    super::shortcuts::resolved(&s)
        .rows_in(&ring, Client::Desktop, true)
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

/// The Stream controls screen: a `page`-column with an intro card on the capture model, whose
/// Change opens the Shortcuts settings, and the reference. The title bar's Back leads home.
pub(crate) fn help_page(
    pal: &Palette,
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    set_nav: &AsyncSetState<String>,
) -> View {
    let change = button(common::common_change())
        .on_click({
            let (ctx, ss, nav) = (ctx.clone(), set_screen.clone(), set_nav.clone());
            move || {
                super::settings::refresh_snapshot(&ctx);
                nav.call("shortcuts".into());
                ss.call(Screen::Settings);
            }
        })
        .horizontal_alignment(HorizontalAlignment::Left);

    let intro = card(
        vstack((
            text(desktop::desktop_controls_intro_title())
                .font_size(15.0)
                .font_weight(FontWeight::SEMI_BOLD),
            text(desktop::desktop_controls_intro_body())
                .font_size(12.0)
                .text_wrapping(TextWrapping::Wrap)
                .foreground(pal.get(Token::SecondaryText)),
            change,
        ))
        .spacing(8.0),
    );

    let mut children = vec![
        page_header(client::controls_title(), Vec::new()),
        intro.into(),
    ];
    children.extend(shortcuts_reference(pal, ctx));
    page(children)
}
