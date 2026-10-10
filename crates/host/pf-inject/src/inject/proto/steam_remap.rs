//! Steam Controller / Deck rich-input fallback when the host backend is not
//! virtual `hid-steam` (DualSense / DualShock 4 / Xbox), plus Deck motion rescale.
//!
//! `PUNKTFUNK_STEAM_REMAP`: `key=value`, `,`/`;`-separated (`paddles=stickclicks`).
//! Pin paddles here; motion units in `crates/host/pf-inject/tests/motion_contract.rs`.
//!
//! uinput Xbox already exposes the grips as Elite paddles (`BTN_TRIGGER_HAPPY5-8`);
//! only DualSense / DS4 (no native back-button slot) fold them.

use punktfunk_core::input::gamepad as gs;

/// Target for the four Steam back grips when the backend has no native HID slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaddleFallback {
    #[default]
    Drop,
    /// L4/L5 → left-stick click, R4/R5 → right-stick click.
    StickClicks,
    /// L4/L5 → left bumper, R4/R5 → right bumper.
    Shoulders,
}

/// Parsed from `PUNKTFUNK_STEAM_REMAP`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemapConfig {
    pub paddles: PaddleFallback,
}

impl RemapConfig {
    pub fn from_env() -> RemapConfig {
        std::env::var("PUNKTFUNK_STEAM_REMAP")
            .map(|s| RemapConfig::parse(&s))
            .unwrap_or_default()
    }

    /// Unknown `paddles=` values → [`PaddleFallback::Drop`].
    pub fn parse(s: &str) -> RemapConfig {
        let mut cfg = RemapConfig::default();
        for kv in s.split([',', ';']) {
            let mut it = kv.splitn(2, '=');
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if k.trim().eq_ignore_ascii_case("paddles") {
                    cfg.paddles = match v.trim().to_ascii_lowercase().as_str() {
                        "stickclicks" | "l3r3" | "sticks" => PaddleFallback::StickClicks,
                        "shoulders" | "lbrb" | "bumpers" => PaddleFallback::Shoulders,
                        _ => PaddleFallback::Drop,
                    };
                }
            }
        }
        cfg
    }
}

/// Wire PADDLE1/2/3/4 = R4/L4/R5/L5, not L then R.
pub fn fold_paddles(mut buttons: u32, policy: PaddleFallback) -> u32 {
    let left = buttons & (gs::BTN_PADDLE2 | gs::BTN_PADDLE4) != 0; // L4 | L5
    let right = buttons & (gs::BTN_PADDLE1 | gs::BTN_PADDLE3) != 0; // R4 | R5
    buttons &= !(gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4);
    let (lbit, rbit) = match policy {
        PaddleFallback::Drop => return buttons,
        PaddleFallback::StickClicks => (gs::BTN_LS_CLICK, gs::BTN_RS_CLICK),
        PaddleFallback::Shoulders => (gs::BTN_LB, gs::BTN_RB),
    };
    if left {
        buttons |= lbit;
    }
    if right {
        buttons |= rbit;
    }
    buttons
}

// hid-steam STEAM_DECK_GYRO_RES_PER_DPS = 16, ACCEL_RES_PER_G = 16384. Wire is DualSense
// (`gs::MOTION_*`). DualSense / DS4 consume 1:1 (cal blobs declare wire units).
// Pin: `crates/host/pf-inject/tests/motion_contract.rs`.
const GYRO_NUM: i32 = 16;
const GYRO_DEN: i32 = gs::MOTION_GYRO_LSB_PER_DEG_S;
const ACCEL_NUM: i32 = 16384;
const ACCEL_DEN: i32 = gs::MOTION_ACCEL_LSB_PER_G;

fn scale(v: i16, num: i32, den: i32) -> i16 {
    ((v as i32 * num) / den).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Wire DualSense (`gs::MOTION_*`) → Deck `hid-steam` (16 LSB/°·s, 16384 LSB/g).
pub fn motion_wire_to_deck(gyro: [i16; 3], accel: [i16; 3]) -> ([i16; 3], [i16; 3]) {
    (
        gyro.map(|g| scale(g, GYRO_NUM, GYRO_DEN)),
        accel.map(|a| scale(a, ACCEL_NUM, ACCEL_DEN)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paddle_policy() {
        assert_eq!(RemapConfig::parse("").paddles, PaddleFallback::Drop);
        assert_eq!(
            RemapConfig::parse("paddles=stickclicks").paddles,
            PaddleFallback::StickClicks
        );
        assert_eq!(
            RemapConfig::parse("foo=bar; paddles = Shoulders").paddles,
            PaddleFallback::Shoulders
        );
        assert_eq!(
            RemapConfig::parse("paddles=nonsense").paddles,
            PaddleFallback::Drop
        );
    }

    #[test]
    fn fold_paddles_maps_and_clears() {
        let b = gs::BTN_A | gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4;
        assert_eq!(fold_paddles(b, PaddleFallback::Drop), gs::BTN_A);
        assert_eq!(
            fold_paddles(b, PaddleFallback::StickClicks),
            gs::BTN_A | gs::BTN_LS_CLICK | gs::BTN_RS_CLICK
        );
        assert_eq!(
            fold_paddles(gs::BTN_PADDLE2, PaddleFallback::Shoulders),
            gs::BTN_LB
        );
    }

    #[test]
    fn motion_rescale_to_deck_units() {
        // gyro × 16/20 = 0.8; accel × 16384/10000 = 1.6384.
        let (g, a) = motion_wire_to_deck([1000, -2000, 0], [10000, -5000, 0]);
        assert_eq!(g, [800, -1600, 0]);
        assert_eq!(a, [16384, -8192, 0]);
        // Saturates rather than wraps.
        let (_, a) = motion_wire_to_deck([0; 3], [32767, i16::MIN, 0]);
        assert_eq!(a[0], i16::MAX);
        assert_eq!(a[1], i16::MIN);
    }
}
