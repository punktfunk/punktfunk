//! The stream status page: streams run in the spawned `punktfunk-session` child's own window,
//! so the shell shows a status card in the app's card language — host header, the child's live
//! stats window as a chip row + stage lines, the in-window shortcuts, and a Disconnect.

use super::style::{edges, hstack, icon_button, text, uniform, vstack};
use pf_client_core::overlay_actions::SlotId;
use pf_client_core::shortcuts::{Action, Device};
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
    use super::style::{card, host_tile, pill, Pill};
    let target = ctx.shared.target.lock().unwrap().clone();
    let host = target.host.name.clone();
    // The settings this launch resolved, so a preset's own shortcuts are the ones named.
    let (launched, _) = pf_client_core::settings::effective_settings(
        target.host.fp_hex.as_deref(),
        &target.host.addr,
        target.host.port,
        target.preset.as_deref(),
        target.launch.as_deref(),
    );
    let sc = super::shortcuts::resolved(&launched);
    let key = |slot: SlotId| sc.hint(&Action::Slot(slot), Device::Keyboard);
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
            host_tile(pal, &host, "", true, 44.0)
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
            // The child prints stats only while its overlay is on (the Settings tier, or the
            // Statistics shortcut) — say so instead of waiting forever. Browse idles in the
            // library between launches, so no stats there is simply normal: no line.
            let msg = if launched.show_stats {
                Some(desktop::desktop_stream_stats_waiting().to_string())
            } else {
                key(SlotId::Stats)
                    .or_else(|| sc.hint(&Action::Slot(SlotId::Stats), Device::Pad))
                    .map(|keys| desktop::desktop_stream_stats_off(&keys))
            };
            if let Some(msg) = msg.filter(|_| !browse) {
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
                    .fields
                    .iter()
                    .map(|c| c.trim())
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

    // The in-window keys as this launch binds them; an action with no key drops out.
    let keys = [
        sc.hint(&Action::Ring, Device::Keyboard)
            .map(|k| desktop::desktop_shortcut_quick_actions(&k)),
        key(SlotId::ReleaseInput).map(|k| desktop::desktop_shortcut_release_input(&k)),
        key(SlotId::EndStream).map(|k| desktop::desktop_capture_disconnect(&k)),
        key(SlotId::Stats).map(|k| desktop::desktop_capture_stats(&k)),
        key(SlotId::Fullscreen).map(|k| desktop::desktop_shortcut_fullscreen(&k)),
    ];
    body.push(
        text(
            keys.into_iter()
                .flatten()
                .collect::<Vec<_>>()
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
