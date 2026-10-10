//! Stream-mode presets grouped by aspect ratio. One table so every client's
//! picker offers the same families and sizes; twins in
//! `PunktfunkShared/Resolutions.swift` and Android `Settings.kt`.
//!
//! A picker shows one family at a time behind an aspect switch, plus its own
//! native / match-window rows. Picking a family moves to its size nearest the
//! current height ([`nearest`]), so the switch and the list always agree
//! without any picker-side state. A device whose screen is none of these
//! shapes (a phone) leads the switch with its own two ([`families`]). Pure;
//! tested here.

/// One family of sizes with the same shape.
pub struct Aspect {
    /// The switch label: `"16:9"`.
    pub label: &'static str,
    /// `(width, height)` units of the shape.
    pub ratio: (u32, u32),
    /// Common panels of this shape, ascending. All sides even (the host
    /// rejects odd modes).
    pub sizes: &'static [(u32, u32)],
}

/// Families in the order the switch shows them: most common first.
pub const ASPECTS: [Aspect; 6] = [
    Aspect {
        label: "16:9",
        ratio: (16, 9),
        sizes: &[
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
            (5120, 2880),
        ],
    },
    Aspect {
        label: "16:10",
        ratio: (16, 10),
        sizes: &[
            (1280, 800),
            (1920, 1200),
            (2560, 1600),
            (2880, 1800),
            (3840, 2400),
        ],
    },
    Aspect {
        label: "21:9",
        ratio: (21, 9),
        sizes: &[(2560, 1080), (3440, 1440), (3840, 1600), (5120, 2160)],
    },
    Aspect {
        label: "32:9",
        ratio: (32, 9),
        sizes: &[(3840, 1080), (5120, 1440), (7680, 2160)],
    },
    Aspect {
        label: "3:2",
        ratio: (3, 2),
        sizes: &[(2160, 1440), (2256, 1504), (2880, 1920), (3000, 2000)],
    },
    Aspect {
        label: "4:3",
        ratio: (4, 3),
        sizes: &[(1024, 768), (1600, 1200), (2048, 1536)],
    },
];

/// Shape tolerance for [`aspect_of`]. "21:9" panels are really 2.37–2.40, so
/// 4 % keeps them in one family and still parts 16:10 (1.60) from 3:2 (1.50).
const TOLERANCE: f64 = 0.04;

/// The family `w`×`h` belongs to by shape, not by membership: a custom
/// 1500×1000 is 3:2. `None` for a zero side (native) or a shape no family has.
pub fn aspect_of(w: u32, h: u32) -> Option<usize> {
    if w == 0 || h == 0 {
        return None;
    }
    let shape = f64::from(w) / f64::from(h);
    ASPECTS.iter().position(|a| {
        let want = f64::from(a.ratio.0) / f64::from(a.ratio.1);
        (shape / want - 1.0).abs() < TOLERANCE
    })
}

/// One entry of a device's aspect switch: a standard family, or this screen's
/// own shape ([`families`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub label: &'static str,
    /// Width over height.
    pub shape: f64,
    /// Ascending, all sides even.
    pub sizes: Vec<(u32, u32)>,
}

pub const SCREEN_LABEL: &str = "Screen";
pub const SAFE_AREA_LABEL: &str = "Safe area";

/// Heights a device family offers below the screen's own.
const DEVICE_HEIGHTS: [u32; 4] = [720, 1080, 1440, 2160];

/// A device family's sizes sit within this of its shape (even-flooring costs
/// under 0.3 %), tight enough to part a phone's screen from its safe area.
const DEVICE_TOLERANCE: f64 = 0.01;

/// The aspect switch on a device with `screen` and a `safe` area (landscape
/// `(w, h)`): "Screen", then "Safe area", then [`ASPECTS`]. Each device entry
/// appears only when no standard family already has its shape, and the safe
/// area only when it differs from the screen.
pub fn families(screen: Option<(u32, u32)>, safe: Option<(u32, u32)>) -> Vec<Family> {
    let mut out = Vec::new();
    let screen = screen.filter(|&(w, h)| w > 0 && h > 0);
    for (label, dims) in [(SCREEN_LABEL, screen), (SAFE_AREA_LABEL, safe)] {
        let Some((w, h)) = dims.filter(|&(w, h)| w > 0 && h > 0) else {
            continue;
        };
        if label == SAFE_AREA_LABEL && Some((w, h)) == screen {
            continue;
        }
        if aspect_of(w, h).is_some() {
            continue;
        }
        out.push(Family {
            label,
            shape: f64::from(w) / f64::from(h),
            sizes: device_sizes(w, h),
        });
    }
    out.extend(ASPECTS.iter().map(|a| Family {
        label: a.label,
        shape: f64::from(a.ratio.0) / f64::from(a.ratio.1),
        sizes: a.sizes.to_vec(),
    }));
    out
}

/// `w`×`h` at the standard heights below it, then itself; widths even-floored.
fn device_sizes(w: u32, h: u32) -> Vec<(u32, u32)> {
    let mut sizes: Vec<(u32, u32)> = DEVICE_HEIGHTS
        .iter()
        .filter(|&&dh| dh < h)
        .map(|&dh| {
            (
                (u64::from(w) * u64::from(dh) / u64::from(h)) as u32 / 2 * 2,
                dh,
            )
        })
        .collect();
    sizes.push((w / 2 * 2, h / 2 * 2));
    sizes
}

/// The entry of `families` that `w`×`h` belongs to by shape: a device family
/// first, within [`DEVICE_TOLERANCE`], then a standard one. `None` for a zero
/// side or a shape none has.
pub fn family_of(families: &[Family], w: u32, h: u32) -> Option<usize> {
    if w == 0 || h == 0 {
        return None;
    }
    let shape = f64::from(w) / f64::from(h);
    let within = |f: &Family, tol: f64| (shape / f.shape - 1.0).abs() < tol;
    let device = |f: &Family| f.label == SCREEN_LABEL || f.label == SAFE_AREA_LABEL;
    families
        .iter()
        .position(|f| device(f) && within(f, DEVICE_TOLERANCE))
        .or_else(|| {
            families
                .iter()
                .position(|f| !device(f) && within(f, TOLERANCE))
        })
}

/// The size in `family` nearest in height to `h`; a native `0` looks for 1080.
/// Ties go to the smaller size.
pub fn nearest_in(family: &Family, h: u32) -> (u32, u32) {
    let h = if h == 0 { 1080 } else { h };
    *family
        .sizes
        .iter()
        .min_by_key(|(_, sh)| sh.abs_diff(h))
        .expect("every family lists a size")
}

/// Smallest stream mode the host accepts, per side.
pub const MIN_WIDTH: u32 = 320;
pub const MIN_HEIGHT: u32 = 200;

/// A typed `w`×`h` as a mode the host takes: each side at least [`MIN_WIDTH`]×[`MIN_HEIGHT`],
/// at most the codec's per-side ceiling, then floored even. Twins in Swift and Kotlin run
/// `clients/shared/custom-resolution-vectors.json`.
pub fn custom(w: u32, h: u32, codec: &str) -> (u32, u32) {
    let max = crate::render_scale::max_dimension(codec);
    (
        w.clamp(MIN_WIDTH, max) / 2 * 2,
        h.clamp(MIN_HEIGHT, max) / 2 * 2,
    )
}

/// The size in family `aspect` nearest in height to `h`; a native `0`
/// looks for 1080. Ties go to the smaller size.
pub fn nearest(aspect: usize, h: u32) -> (u32, u32) {
    let h = if h == 0 { 1080 } else { h };
    *ASPECTS[aspect]
        .sizes
        .iter()
        .min_by_key(|(_, sh)| sh.abs_diff(h))
        .expect("every family lists a size")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_size_maps_back_to_its_family() {
        for (i, a) in ASPECTS.iter().enumerate() {
            for &(w, h) in a.sizes {
                assert_eq!(aspect_of(w, h), Some(i), "{w}x{h} → {}", a.label);
                assert!(w % 2 == 0 && h % 2 == 0, "{w}x{h} has an odd side");
            }
            assert!(
                a.sizes.windows(2).all(|p| p[0].1 <= p[1].1),
                "{} ascending",
                a.label
            );
        }
    }

    #[test]
    fn shape_not_membership() {
        assert_eq!(aspect_of(1500, 1000), Some(4), "a custom 3:2");
        assert_eq!(
            aspect_of(3456, 2234),
            Some(1),
            "a MacBook panel reads 16:10"
        );
        assert_eq!(aspect_of(2556, 1179), None, "a phone panel is nobody's");
        assert_eq!(aspect_of(0, 0), None, "native");
        assert_eq!(aspect_of(1920, 0), None);
    }

    /// A OnePlus 9 Pro: 3216×1440, 127 px of cutout on one side.
    #[test]
    fn a_phone_leads_with_its_screen_and_safe_area() {
        let f = families(Some((3216, 1440)), Some((3088, 1440)));
        assert_eq!(f.len(), ASPECTS.len() + 2);
        assert_eq!(
            (f[0].label, f[1].label, f[2].label),
            ("Screen", "Safe area", "16:9")
        );
        assert_eq!(f[0].sizes, [(1608, 720), (2412, 1080), (3216, 1440)]);
        assert_eq!(f[1].sizes, [(1544, 720), (2316, 1080), (3088, 1440)]);
        assert_eq!(family_of(&f, 2412, 1080), Some(0));
        assert_eq!(family_of(&f, 2316, 1080), Some(1));
        assert_eq!(family_of(&f, 1920, 1080), Some(2));
        assert_eq!(nearest_in(&f[1], 1080), (2316, 1080));
        for fam in &f {
            assert!(fam.sizes.iter().all(|&(w, h)| w % 2 == 0 && h % 2 == 0));
        }
    }

    #[test]
    fn a_standard_screen_adds_nothing() {
        let plain = families(None, None);
        assert_eq!(families(Some((1920, 1080)), Some((1920, 1080))), plain);
        assert_eq!(
            families(Some((2560, 1600)), Some((2560, 1500))),
            plain,
            "both standard shapes"
        );
        let no_cutout = families(Some((2556, 1179)), Some((2556, 1179)));
        assert_eq!(
            no_cutout.len(),
            ASPECTS.len() + 1,
            "safe area equal to the screen"
        );
    }

    /// The cross-language contract; Swift and Kotlin read the same file.
    #[test]
    fn custom_matches_the_shared_vectors() {
        let raw = include_str!("../../../../clients/shared/custom-resolution-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let cases = file["custom"].as_array().expect("custom");
        assert!(
            cases.len() >= 10,
            "the vector file is the contract; keep it rich"
        );
        for case in cases {
            let pair = |k: &str| {
                let a = case[k].as_array().unwrap();
                (a[0].as_u64().unwrap() as u32, a[1].as_u64().unwrap() as u32)
            };
            let (w, h) = pair("typed");
            let got = custom(w, h, case["codec"].as_str().unwrap());
            assert_eq!(got, pair("want"), "{}", case["name"]);
        }
    }

    #[test]
    fn nearest_follows_height_and_native_means_1080() {
        assert_eq!(nearest(0, 0), (1920, 1080));
        assert_eq!(nearest(1, 1080), (1920, 1200));
        assert_eq!(nearest(2, 1440), (3440, 1440));
        assert_eq!(nearest(3, 2160), (7680, 2160));
        assert_eq!(nearest(4, 800), (2160, 1440));
        assert_eq!(nearest(5, 1000), (1600, 1200));
    }
}
