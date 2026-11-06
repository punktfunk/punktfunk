//! The stream status page: streams run in the spawned `punktfunk-session` child's own window,
//! so the shell shows a status card in the app's card language — host header, the child's live
//! stats window as a chip row + stage lines, the in-window shortcuts, and a Disconnect.

use super::style::{edges, hstack, icon_button, text, uniform, vstack};
use pf_fluent::{Palette, Token};
use pf_l10n::{common, desktop};
use std::sync::Arc;
use windows_reactor::*;

/// One HUD refresh: the session child's latest `stats-json:` window, mirrored into root state
/// by the poll thread (`pf-hud`) and passed down as a prop.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct HudSample {
    /// `None` before the child's first stats window.
    pub(crate) stats: Option<punktfunk_core::hud::StatsSnapshot>,
}

/// Spawn mode's Stream screen: the stream runs in the punktfunk-session child's own
/// window, so the shell shows a status card in the app's card language — monogram +
/// host header, the child's live stats window as a chip row + stage lines, the
/// in-window shortcuts, and a Disconnect that kills the child (its exit event routes
/// the app back to the host list, same as the child's window closing).
pub(crate) fn session_page(pal: &Palette, ctx: &Arc<super::AppCtx>, hud: &HudSample) -> View {
    use super::style::{avatar, card, pill, Pill};
    let host = ctx.shared.target.lock().unwrap().host.name.clone();
    let browse = ctx.shared.browse.load(std::sync::atomic::Ordering::SeqCst);
    let title = match (browse, host.is_empty()) {
        (true, true) => desktop::desktop_stream_console_library().to_string(),
        (true, false) => desktop::desktop_stream_console_library_host(&host),
        (false, true) => desktop::desktop_stream_streaming().to_string(),
        (false, false) => desktop::desktop_stream_streaming_to(&host),
    };

    // Header: monogram + title + the one thing worth knowing (where the video went).
    let header: View = Grid::new()
        .children((
            // Monogram, as the comment above says: `Target` carries no OS chain to mark with.
            avatar(pal, &host, "")
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack((
                text(&title)
                    .font_size(18.0)
                    .font_weight(FontWeight::SEMI_BOLD),
                text(desktop::desktop_stream_own_window())
                    .font_size(12.0)
                    .foreground(pal.get(Token::SecondaryText)),
            ))
            .spacing(2.0)
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .margin(edges(12.0, 0.0, 0.0, 0.0)),
        ))
        .columns([GridLength::Auto, GridLength::STAR])
        .into();

    // The child's latest window in the vocabulary the user picked: line one becomes a chip
    // row (the decode path gets the status colour), the rest dim lines.
    let mut body: Vec<View> = vec![header];
    match &hud.stats {
        None => {
            // The child prints stats only while its overlay is on (the Settings tier, or
            // Ctrl+Alt+Shift+S) — say so instead of waiting forever. Browse idles in the
            // library between launches, so no stats there is simply normal: no line.
            if !browse {
                let msg = if ctx.settings.lock().unwrap().show_stats {
                    desktop::desktop_stream_stats_waiting().to_string()
                } else {
                    desktop::desktop_stream_stats_off("Ctrl+Alt+Shift+S")
                };
                body.push(
                    text(msg)
                        .font_size(11.0)
                        .foreground(pal.get(Token::SecondaryText))
                        .into(),
                );
            }
        }
        Some(snap) => {
            use punktfunk_core::hud::{format, StatsVerbosity};
            let advanced = ctx.settings.lock().unwrap().advanced_stats;
            let mut lines = format(snap, StatsVerbosity::Detailed, advanced).into_iter();
            if let Some(first) = lines.next() {
                let chips: Vec<View> = first
                    .text
                    .split(" \u{00B7} ")
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(|c| {
                        let kind = match c {
                            _ if c != snap.decoder => Pill::Neutral,
                            "software" => Pill::Info,
                            _ => Pill::Good,
                        };
                        pill(pal, c, kind).into()
                    })
                    .collect();
                body.push(hstack(chips).spacing(6.0).into());
            }
            for line in lines {
                body.push(
                    text(&line.text)
                        .font_size(11.0)
                        .foreground(pal.get(Token::SecondaryText))
                        .into(),
                );
            }
        }
    }

    body.push(
        text(
            [
                desktop::desktop_shortcut_quick_actions("Ctrl+Alt+Shift+O"),
                desktop::desktop_shortcut_release_input("Ctrl+Alt+Shift+Q"),
                desktop::desktop_capture_disconnect("Ctrl+Alt+Shift+D"),
                desktop::desktop_capture_stats("Ctrl+Alt+Shift+S"),
                desktop::desktop_shortcut_fullscreen("F11"),
            ]
            .join(" \u{00B7} "),
        )
        .font_size(11.0)
        .text_wrapping(TextWrapping::Wrap)
        .foreground(pal.get(Token::SecondaryText))
        .margin(edges(0.0, 4.0, 0.0, 0.0))
        .into(),
    );
    body.push({
        let ctx = ctx.clone();
        icon_button("x", common::common_disconnect())
            .on_click(move || {
                // Kill the child; its exit event (the reader thread) navigates to the
                // host list, exactly like the session window closing.
                ctx.shared.session.lock().unwrap().kill();
            })
            .margin(edges(0.0, 6.0, 0.0, 0.0))
            .into()
    });

    // One centred card, sized like the app's dialogs — Stretch + max_width (the `page`
    // pattern) instead of a fixed width, so a narrow window shrinks the card instead of
    // clipping it while a wide one still centres it at 520.
    Border::new()
        .content(card(vstack(body).spacing(12.0)))
        .max_width(520.0)
        .margin(uniform(24.0))
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}
