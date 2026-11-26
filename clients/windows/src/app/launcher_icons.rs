//! The library's launcher-tile brand marks: the monochrome PNGs under `assets/launchers/`
//! (mid-gray — legible on both WinUI themes; derived from the `assets/launcher-icons` masters,
//! see that README for provenance/licensing), embedded in the exe. Baked taller than the OS
//! marks (`WIN_HEIGHT` in scripts/gen-launcher-icons.sh vs gen-os-icons.sh), because this mark
//! fills a poster tile rather than an avatar.

use windows_reactor::EncodedImage;

/// PNG per icon token. A plugin may name a mark a newer build ships; a tile whose token
/// isn't here falls back to naming its launcher.
static ICONS: &[(&str, &[u8])] = &[
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
    (
        "ubisoft",
        include_bytes!("../../assets/launchers/ubisoft.png"),
    ),
];

/// The mark for an entry's `icon` token, or `None` — draw the launcher's name instead — when
/// the entry carries no token or names one we ship no art for.
pub fn image(token: Option<&str>) -> Option<EncodedImage> {
    let token = token?;
    ICONS
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, png)| EncodedImage::from_static(png))
}
