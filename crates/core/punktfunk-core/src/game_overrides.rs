//! Game overrides: the stream settings a host sets for one game, the same for every device
//! that plays it. The host resolves them (`game ?? source`, a shared override's values inlined)
//! onto each library entry; the client applies them on top of its own settings at the connect
//! that launches the game, then [`fit`] clamps the result to the device.
//!
//! A set field wins over the device's pick. `lines` and `hz` never go above the panel unless
//! `beyond_screen`, and never above the codec's wall. `hdr` and `gamepad` replace the device's
//! value before its own gates (an HDR panel, a pad the host can create). The Swift and Kotlin
//! twins run `clients/shared/game-override-vectors.json`; change the rule there first.

use crate::config::{GamepadPref, Mode};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub const MIN_LINES: u32 = 360;
pub const MAX_LINES: u32 = 4320;
pub const MIN_HZ: u32 = 24;
pub const MAX_HZ: u32 = 240;
pub const MAX_NAME_LEN: usize = 64;

/// One game's resolved overrides. Every field absent means the device decides; `name` is the
/// shared override they came from, for the client's "Host: …" line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct GameOverrides {
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub name: Option<String>,
    /// Height in lines: 2160 for 4K, 1080, 720. Width follows the device's aspect.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub lines: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hz: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hdr: Option<bool>,
    /// A [`GamepadPref`] name as [`GamepadPref::from_name`] reads it.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub gamepad: Option<String>,
    /// `lines` also reaches a device whose panel is smaller, decoded and scaled down.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "std::ops::Not::not")
    )]
    pub beyond_screen: bool,
}

impl GameOverrides {
    /// No field set: the entry carries nothing and should not be shown.
    pub fn is_empty(&self) -> bool {
        self.lines.is_none()
            && self.hz.is_none()
            && self.hdr.is_none()
            && self.gamepad.is_none()
            && !self.beyond_screen
    }

    /// The host's range and name checks; `Err` names the field. Clients trust the host.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(l) = self.lines {
            if !(MIN_LINES..=MAX_LINES).contains(&l) {
                return Err("lines");
            }
        }
        if let Some(z) = self.hz {
            if !(MIN_HZ..=MAX_HZ).contains(&z) {
                return Err("hz");
            }
        }
        if let Some(g) = &self.gamepad {
            if GamepadPref::from_name(g).is_none() {
                return Err("gamepad");
            }
        }
        if let Some(n) = &self.name {
            let n = n.trim();
            if n.is_empty() || n.chars().count() > MAX_NAME_LEN {
                return Err("name");
            }
        }
        Ok(())
    }

    /// The pad the override asks for, if it names one.
    pub fn gamepad_pref(&self) -> Option<GamepadPref> {
        self.gamepad.as_deref().and_then(GamepadPref::from_name)
    }
}

/// Nearest even number; host `validate_dimensions` rejects odd sizes.
pub fn even(x: f64) -> u32 {
    ((x / 2.0).round() as u32) * 2
}

/// The mode a device asks for once `o` is applied to `mode`, the size it would ask on its own.
///
/// `lines` replaces the height and the width follows `mode`'s aspect; capped at `native`'s
/// lines unless `beyond_screen`. `hz` is capped at `native`'s. A zero in `native` means
/// unknown and caps nothing (the host clamps with no panel in view). Then both axes scale down
/// to `codec_max` together. A field the override leaves unset leaves `mode` alone — a device's
/// own choice above its panel is not this function's business.
pub fn fit(mode: Mode, native: Mode, o: &GameOverrides, codec_max: u32) -> Mode {
    let mut out = mode;
    if let Some(lines) = o.lines {
        let lines = if o.beyond_screen || native.height == 0 {
            lines
        } else {
            lines.min(native.height)
        };
        if lines != mode.height && mode.height > 0 {
            out.width = even(f64::from(mode.width) * f64::from(lines) / f64::from(mode.height));
            out.height = even(f64::from(lines));
        }
    }
    if let Some(hz) = o.hz {
        out.refresh_hz = if native.refresh_hz == 0 {
            hz
        } else {
            hz.min(native.refresh_hz)
        };
    }
    let longest = out.width.max(out.height);
    if longest > codec_max && longest > 0 {
        let s = f64::from(codec_max) / f64::from(longest);
        out.width = even(f64::from(out.width) * s).min(codec_max);
        out.height = even(f64::from(out.height) * s).min(codec_max);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(a: &serde_json::Value) -> Mode {
        let n = |i: usize| a[i].as_u64().expect("number") as u32;
        Mode {
            width: n(0),
            height: n(1),
            refresh_hz: n(2),
        }
    }

    fn overrides(row: &serde_json::Value) -> GameOverrides {
        GameOverrides {
            name: row["name"].as_str().map(str::to_string),
            lines: row["lines"].as_u64().map(|v| v as u32),
            hz: row["hz"].as_u64().map(|v| v as u32),
            hdr: row["hdr"].as_bool(),
            gamepad: row["gamepad"].as_str().map(str::to_string),
            beyond_screen: row["beyond_screen"].as_bool().unwrap_or(false),
        }
    }

    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../../clients/shared/game-override-vectors.json");
        serde_json::from_str(raw).expect("vector file parses")
    }

    #[test]
    fn shared_fit_vectors() {
        for row in vectors()["fit"].as_array().expect("fit rows") {
            let got = fit(
                mode(&row["mode"]),
                mode(&row["native"]),
                &overrides(row),
                row["max"].as_u64().expect("max") as u32,
            );
            assert_eq!(got, mode(&row["expect"]), "{}", row["why"]);
            assert!(got.width % 2 == 0 && got.height % 2 == 0, "{}", row["why"]);
        }
    }

    #[test]
    fn shared_valid_vectors() {
        for row in vectors()["valid"].as_array().expect("valid rows") {
            let ok = overrides(row).validate().is_ok();
            assert_eq!(ok, row["ok"].as_bool().expect("ok"), "{}", row["why"]);
        }
    }

    #[test]
    fn empty_means_nothing_set() {
        assert!(GameOverrides::default().is_empty());
        let named = GameOverrides {
            name: Some("4K".into()),
            ..Default::default()
        };
        assert!(named.is_empty(), "a name alone sets nothing");
        let beyond = GameOverrides {
            beyond_screen: true,
            ..Default::default()
        };
        assert!(!beyond.is_empty());
    }

    #[test]
    fn even_rounds_to_the_nearest_even() {
        assert_eq!(even(1727.9), 1728);
        assert_eq!(even(1729.0), 1730);
        assert_eq!(even(1731.0), 1732);
        assert_eq!(even(0.4), 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn wire_shape_omits_what_is_unset() {
        let o = GameOverrides {
            lines: Some(1080),
            gamepad: Some("dualsense".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(json, r#"{"lines":1080,"gamepad":"dualsense"}"#);
        let back: GameOverrides =
            serde_json::from_str(r#"{"lines":1080,"beyond_screen":true,"future":1}"#).unwrap();
        assert_eq!(back.lines, Some(1080));
        assert!(back.beyond_screen);
    }
}
