//! The library's launcher-tile brand marks: the monochrome PNGs under `assets/launchers/`
//! (mid-gray — legible on both WinUI themes; derived from the `assets/launcher-icons` masters,
//! see that README for provenance/licensing), staged through [`EmbeddedPngs`]. Baked much taller
//! than the OS marks (128 px vs 32), because this mark fills a poster tile rather than sitting
//! in a status row.

use super::embedded_png::EmbeddedPngs;

/// Embedded PNG per icon token. A plugin may name a mark a newer build ships; a tile whose token
/// isn't here falls back to naming its launcher, which is how every launcher tile looked before
/// icons existed.
static ICONS: EmbeddedPngs = EmbeddedPngs::new(
    "launcher-icons",
    &[
        ("steam", include_bytes!("../../assets/launchers/steam.png")),
        (
            "lutris",
            include_bytes!("../../assets/launchers/lutris.png"),
        ),
        (
            "heroic",
            include_bytes!("../../assets/launchers/heroic.png"),
        ),
        (
            "playnite",
            include_bytes!("../../assets/launchers/playnite.png"),
        ),
        ("epic", include_bytes!("../../assets/launchers/epic.png")),
        ("gog", include_bytes!("../../assets/launchers/gog.png")),
        ("xbox", include_bytes!("../../assets/launchers/xbox.png")),
        ("hydra", include_bytes!("../../assets/launchers/hydra.png")),
    ],
);

/// Stage the marks on disk. Called once at GUI startup, before any tile renders.
pub fn install() {
    ICONS.install();
}

/// The `file:///` URI of the mark for an entry's `icon` token, or `None` — draw the launcher's
/// name instead — when the entry carries no token or names one we ship no art for.
pub fn uri(token: Option<&str>) -> Option<String> {
    ICONS.uri(token?)
}
