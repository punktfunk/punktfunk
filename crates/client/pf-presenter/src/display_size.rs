//! The `display_mm` a connect sends: the size the picture has on the glass, in millimetres.
//! The panel's size comes from its EDID: the registry's copy on Windows, the DRM connector's
//! on Linux, which every compositor, gamescope and X11 share.

/// The panel's size in millimetres from an EDID's basic block. `None` when a side is zero, as
/// projectors and size-less TVs leave it, or exactly 16×9 cm, the aspect ratio many TVs and AV
/// receivers write there instead of a size.
pub(crate) fn edid_size_mm(edid: &[u8]) -> Option<(u32, u32)> {
    const HEADER: [u8; 8] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];
    let (w, h) = (*edid.get(21)?, *edid.get(22)?);
    let aspect_only = matches!((w, h), (16, 9) | (9, 16));
    (edid.starts_with(&HEADER) && w > 0 && h > 0 && !aspect_only)
        .then(|| (u32::from(w) * 10, u32::from(h) * 10))
}

/// The monitor name an EDID states in its `0xFC` descriptor, e.g. `DELL U2720Q`.
#[cfg_attr(windows, allow(dead_code))]
fn edid_name(edid: &[u8]) -> Option<&str> {
    let d = edid
        .get(54..126)?
        .chunks(18)
        .find(|d| d[..4] == [0, 0, 0, 0xFC])?;
    let name = std::str::from_utf8(&d[5..]).ok()?;
    Some(name.split('\n').next()?.trim()).filter(|n| !n.is_empty())
}

/// The panel size of the connector showing `display`, SDL's name for it, among the `connected`
/// (connector, EDID) pairs. Candidates are those whose connector or monitor name `display`
/// mentions, else all; they must agree on one size. Windows reads the window's monitor directly.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn panel_mm(display: &str, connected: &[(String, Vec<u8>)]) -> Option<(u32, u32)> {
    let words: Vec<&str> = display
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .collect();
    let named: Vec<&[u8]> = connected
        .iter()
        .filter(|(c, e)| {
            words.contains(&c.as_str()) || edid_name(e).is_some_and(|n| display.contains(n))
        })
        .map(|(_, e)| e.as_slice())
        .collect();
    let pick = if named.is_empty() {
        connected.iter().map(|(_, e)| e.as_slice()).collect()
    } else {
        named
    };
    let (first, rest) = pick.split_first()?;
    let mm = edid_size_mm(first)?;
    rest.iter()
        .all(|e| edid_size_mm(e) == Some(mm))
        .then_some(mm)
}

/// Every connected, lit DRM connector's name (`DP-1`) and EDID.
#[cfg(target_os = "linux")]
pub(crate) fn connected_edids() -> Vec<(String, Vec<u8>)> {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (_, connector) = name.split_once('-')?;
            let path = e.path();
            let read = |f: &str| std::fs::read_to_string(path.join(f)).unwrap_or_default();
            let lit = read("status").trim() == "connected" && read("enabled").trim() == "enabled";
            lit.then(|| {
                (
                    connector.to_owned(),
                    std::fs::read(path.join("edid")).unwrap_or_default(),
                )
            })
        })
        .collect()
}

/// The display pixels the picture is shown over, from what the client asked for and never
/// from a window still on its way there: fullscreen covers the `desktop`, a window sized just
/// before covers `asked`, any other covers the size it has.
pub(crate) fn shown_px(
    fullscreen: bool,
    desktop: (u32, u32),
    asked: Option<(u32, u32)>,
    window: (u32, u32),
) -> (u32, u32) {
    if fullscreen {
        desktop
    } else {
        asked.unwrap_or(window)
    }
}

/// The size `mode` has on the glass in millimetres, width then height, as `fit` places it
/// over `shown` pixels of a `desktop`-pixel display whose panel measures `panel`. Under
/// `match_window` the mode follows the window, so it is the whole panel: the host keeps one
/// size for the session, and the panel's is right at full screen and never too small. `None`
/// for a panel outside 40–1000 pixels per inch, which only a wrong size gives.
pub(crate) fn picture_mm(
    mode: (u32, u32),
    shown: (u32, u32),
    desktop: (u32, u32),
    panel: (u32, u32),
    fit: punktfunk_core::video_fit::VideoFit,
    match_window: bool,
) -> Option<(u16, u16)> {
    // A rotated desktop turns the panel with it.
    let (pw, ph) = if (desktop.0 >= desktop.1) == (panel.0 >= panel.1) {
        panel
    } else {
        (panel.1, panel.0)
    };
    let dense = |px: u32, mm: u32| {
        mm > 0 && (40.0..=1000.0).contains(&(f64::from(px) * 25.4 / f64::from(mm)))
    };
    if !(dense(desktop.0, pw) && dense(desktop.1, ph)) {
        return None;
    }
    if match_window {
        return Some((u16::try_from(pw).ok()?, u16::try_from(ph).ok()?));
    }
    let p = punktfunk_core::video_fit::place(fit, shown, mode);
    if p.is_empty() {
        return None;
    }
    let side = |px: u32, scale: f64, mm: u32, desktop: u32| {
        let v = (f64::from(px) * scale * f64::from(mm) / f64::from(desktop)).round();
        (v >= 1.0).then(|| v.min(f64::from(u16::MAX)) as u16)
    };
    Some((
        side(mode.0, p.scale_x, pw, desktop.0)?,
        side(mode.1, p.scale_y, ph, desktop.1)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::video_fit::VideoFit;

    /// A 128-byte EDID of `w`×`h` cm naming `name`.
    fn edid(w: u8, h: u8, name: &str) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        (e[21], e[22]) = (w, h);
        e[72..76].copy_from_slice(&[0, 0, 0, 0xFC]);
        e[77..90].fill(b' ');
        e[77..77 + name.len()].copy_from_slice(name.as_bytes());
        e[77 + name.len()] = b'\n';
        e
    }

    const K4: (u32, u32) = (3840, 2160);

    const PANEL: (u32, u32) = (600, 340);

    #[test]
    fn a_picture_filling_the_screen_is_the_panel() {
        let mm = edid_size_mm(&edid(60, 34, "DELL U2720Q")).unwrap();
        assert_eq!(
            picture_mm(K4, K4, K4, mm, VideoFit::Fit, false),
            Some((600, 340))
        );
    }

    /// X11 and Wayland apply a size or fullscreen later, so the window still reads its old size.
    #[test]
    fn the_shown_size_is_the_one_asked_for_not_the_windows_old_one() {
        let old = (1280, 720);
        assert_eq!(shown_px(true, K4, None, old), K4);
        assert_eq!(shown_px(true, K4, Some((1920, 1080)), old), K4);
        assert_eq!(shown_px(false, K4, Some((1920, 1080)), old), (1920, 1080));
        assert_eq!(shown_px(false, K4, None, old), old);
    }

    /// Fewer pixels over the same glass: the size holds at every mode.
    #[test]
    fn render_scale_keeps_the_size() {
        let mm = picture_mm((1920, 1080), K4, K4, PANEL, VideoFit::Fit, false);
        assert_eq!(mm, Some((600, 340)));
    }

    #[test]
    fn a_window_is_only_the_glass_it_covers() {
        // 1080p one to one on a 4K panel is a quarter of it; letterboxed, the bars are left out.
        let quarter = picture_mm((1920, 1080), (1920, 1080), K4, PANEL, VideoFit::Fit, false);
        assert_eq!(quarter, Some((300, 170)));
        let deck = (1280, 800);
        let bars = picture_mm((1920, 1080), deck, deck, (150, 90), VideoFit::Fit, false);
        assert_eq!(bars, Some((150, 81)));
    }

    /// Match-window resizes the mode in place and the host's display keeps its size, so the
    /// size is the panel's whatever the window is now.
    #[test]
    fn match_window_is_the_whole_panel() {
        let small = (1280, 720);
        assert_eq!(
            picture_mm(small, small, K4, PANEL, VideoFit::Fit, true),
            Some((600, 340))
        );
        assert_eq!(
            picture_mm(small, (0, 0), K4, PANEL, VideoFit::Fit, true),
            Some((600, 340))
        );
    }

    #[test]
    fn a_portrait_panel_turned_landscape_swaps_its_sides() {
        let deck = (1280, 800);
        let mm = picture_mm(deck, deck, deck, (90, 150), VideoFit::Fit, false);
        assert_eq!(mm, Some((150, 90)));
    }

    #[test]
    fn an_unknown_or_absurd_size_is_none() {
        assert_eq!(edid_size_mm(&edid(0, 0, "Projector")), None);
        assert_eq!(edid_size_mm(&edid(60, 0, "TV")), None);
        assert_eq!(
            edid_size_mm(&edid(16, 9, "TV")),
            None,
            "an aspect ratio, not a size"
        );
        assert_eq!(edid_size_mm(&edid(9, 16, "TV")), None);
        assert_eq!(edid_size_mm(&edid(16, 10, "Deck OLED")), Some((160, 100)));
        assert_eq!(edid_size_mm(&[0; 128]), None, "no EDID header");
        assert_eq!(edid_size_mm(&[]), None);
        for wrong in [(10, 10), (5000, 3000), (0, 340)] {
            assert_eq!(picture_mm(K4, K4, K4, wrong, VideoFit::Fit, false), None);
            assert_eq!(picture_mm(K4, K4, K4, wrong, VideoFit::Fit, true), None);
        }
        assert_eq!(
            picture_mm(K4, (0, 0), K4, PANEL, VideoFit::Fit, false),
            None
        );
    }

    #[test]
    fn the_connector_is_the_one_the_display_names() {
        let dell = ("DP-1".to_owned(), edid(60, 34, "DELL U2720Q"));
        let lap = ("eDP-1".to_owned(), edid(30, 20, "0x0BCA"));
        let both = [dell.clone(), lap.clone()];
        assert_eq!(
            panel_mm("Anything", std::slice::from_ref(&dell)),
            Some((600, 340))
        );
        assert_eq!(panel_mm("Dell Inc. DELL U2720Q", &both), Some((600, 340)));
        assert_eq!(
            panel_mm("Dell Inc. DELL U2720Q ABC (DP-1)", &both),
            Some((600, 340))
        );
        assert_eq!(panel_mm("BOE 0x0BCA (eDP-1)", &both), Some((300, 200)));
        assert_eq!(panel_mm("Dell 27\"", &both), None, "two sizes, no name");
        let twin = ("DP-2".to_owned(), dell.1.clone());
        assert_eq!(panel_mm("Dell 27\"", &[dell, twin]), Some((600, 340)));
        let projector = ("HDMI-A-1".to_owned(), edid(0, 0, "EPSON"));
        assert_eq!(panel_mm("EPSON PJ", &[lap, projector]), None);
        assert_eq!(panel_mm("DP-1", &[]), None);
    }
}
