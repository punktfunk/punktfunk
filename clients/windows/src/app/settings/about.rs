//! About: identity and version, the log folder, licenses.

use super::{described_labeled, group};
use crate::app::Screen;
use windows_reactor::*;

pub(super) fn about_section(set_screen: &AsyncSetState<Screen>) -> Vec<Element> {
    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };
    // The client log's folder, so the rotated `.old` generation is in reach too. `real_dir`,
    // not the literal %LOCALAPPDATA% path: Explorer lives outside the MSIX container and opens
    // Documents for a redirected path. The `is_dir` guard keeps that fallback unreachable; a
    // failed spawn stays silent.
    let logs_button = button("Open log folder").on_click(|| {
        if let Some(dir) = crate::logfile::real_dir().filter(|d| d.is_dir()) {
            let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
        }
    });
    // App identity + version at the top of the About card (the WinUI Settings convention).
    // CARGO_PKG_VERSION is the workspace version, baked in at compile time.
    let about_identity = vstack((
        text_block("Punktfunk").font_size(20.0).semibold(),
        text_block(concat!("Version ", env!("CARGO_PKG_VERSION")))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(2.0);

    group(
        None,
        vec![
            about_identity.into(),
            described_labeled(
                "Diagnostics",
                logs_button,
                "The client log (client.log, plus the session\u{2019}s whole \
                 receive/decode/present trail) \u{2014} attach it to a bug report.",
            ),
            licenses_button.into(),
        ],
        None,
    )
}
