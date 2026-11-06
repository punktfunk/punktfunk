//! About: identity and version, the log folder, licenses.

use super::{described_labeled, group};
use crate::app::style::*;
use crate::app::{AsyncSetState, Screen};
use pf_fluent::{Palette, Token};
use pf_l10n::{client, desktop};
use windows_reactor::*;

pub(super) fn about_section(pal: &Palette, set_screen: &AsyncSetState<Screen>) -> Vec<View> {
    let licenses_button = {
        let ss = set_screen.clone();
        button(client::about_licenses()).on_click(move || ss.call(Screen::Licenses))
    };
    // The client log's folder, so the rotated `.old` generation is in reach too. `real_dir`,
    // not the literal %LOCALAPPDATA% path: Explorer lives outside the MSIX container and opens
    // Documents for a redirected path. The `is_dir` guard keeps that fallback unreachable; a
    // failed spawn stays silent.
    let logs_button = button(desktop::desktop_about_open_logs()).on_click(|| {
        if let Some(dir) = crate::logfile::real_dir().filter(|d| d.is_dir()) {
            let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
        }
    });
    // App identity + version at the top of the About card (the WinUI Settings convention).
    // CARGO_PKG_VERSION is the workspace version, baked in at compile time.
    let about_identity = vstack((
        text("Punktfunk")
            .font_size(20.0)
            .font_weight(FontWeight::SEMI_BOLD),
        text(client::about_version_line(env!("CARGO_PKG_VERSION")))
            .font_size(12.0)
            .foreground(pal.get(Token::SecondaryText)),
    ))
    .spacing(2.0);

    group(
        pal,
        None,
        vec![
            about_identity.into(),
            described_labeled(
                pal,
                desktop::desktop_about_diagnostics(),
                logs_button,
                desktop::desktop_about_logs_hint(),
            ),
            licenses_button.into(),
        ],
        None,
    )
}
