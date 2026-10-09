//! The stream status page: streams run in the spawned `punktfunk-session` child's own window,
//! so the shell shows a status card in the app's card language — host header, the child's live
//! stats window as a chip row + stage lines, the in-window shortcuts, and a Disconnect.

use super::lucide;
use super::style::{edges, uniform};
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
/// the app back to the host list, same as the child's window closing). No hooks.
pub(crate) fn session_page(ctx: &Arc<super::AppCtx>, hud: &HudSample) -> Element {
    use super::style::{avatar, card, pill, Pill};
    let host = ctx.shared.target.lock().unwrap().host.name.clone();
    let browse = ctx.shared.browse.load(std::sync::atomic::Ordering::SeqCst);
    let title = match (browse, host.is_empty()) {
        (true, true) => "Console library".to_string(),
        (true, false) => format!("Console library \u{00B7} {host}"),
        (false, true) => "Streaming".to_string(),
        (false, false) => format!("Streaming to {host}"),
    };

    // Header: monogram + title + the one thing worth knowing (where the video went).
    let header: Element = grid((
        // Monogram, as the comment above says: `Target` carries no OS chain to mark with.
        avatar(&host, "")
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
        vstack((
            text_block(&title).font_size(18.0).semibold(),
            text_block(
                "The stream has its own window \u{2014} this one returns when the session ends.",
            )
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(2.0)
        .grid_column(1)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(edges(12.0, 0.0, 0.0, 0.0)),
    ))
    .columns([GridLength::Auto, GridLength::Star(1.0)])
    .into();

    // The child's latest window in the vocabulary the user picked: line one becomes a chip
    // row (the decode path gets the status colour), the rest dim lines.
    let mut body: Vec<Element> = vec![header];
    match &hud.stats {
        None => {
            // The child prints stats only while its overlay is on (the Settings tier, or
            // Ctrl+Alt+Shift+S) — say so instead of waiting forever. Browse idles in the
            // library between launches, so no stats there is simply normal: no line.
            if !browse {
                let msg = if ctx.settings.lock().unwrap().show_stats {
                    "Waiting for the first stats window\u{2026}"
                } else {
                    "Stats are off \u{2014} Ctrl+Alt+Shift+S in the stream window turns them on."
                };
                body.push(
                    text_block(msg)
                        .font_size(11.0)
                        .foreground(ThemeRef::SecondaryText)
                        .into(),
                );
            }
        }
        Some(snap) => {
            use punktfunk_core::hud::{format, StatsVerbosity};
            let advanced = ctx.settings.lock().unwrap().advanced_stats;
            let mut lines = format(snap, StatsVerbosity::Detailed, advanced).into_iter();
            if let Some(first) = lines.next() {
                let chips: Vec<Element> = first
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
                        pill(c, kind).into()
                    })
                    .collect();
                body.push(hstack(chips).spacing(6.0).into());
            }
            for line in lines {
                body.push(
                    text_block(&line.text)
                        .font_size(11.0)
                        .foreground(ThemeRef::SecondaryText)
                        .into(),
                );
            }
        }
    }

    body.push(
        text_block(
            "Ctrl+Alt+Shift+O quick actions \u{00B7} Ctrl+Alt+Shift+Q releases input \u{00B7} \
             Ctrl+Alt+Shift+D disconnects \u{00B7} Ctrl+Alt+Shift+S stats \u{00B7} F11 fullscreen",
        )
        .font_size(11.0)
        .wrap()
        .foreground(ThemeRef::SecondaryText)
        .margin(edges(0.0, 4.0, 0.0, 0.0))
        .into(),
    );
    body.push({
        let ctx = ctx.clone();
        button("Disconnect")
            .icon(lucide::icon("x"))
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
    border(card(vstack(body).spacing(12.0)))
        .max_width(520.0)
        .margin(uniform(24.0))
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}
