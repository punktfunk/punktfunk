//! Lucide chrome marks from the shared table [`pf_client_core::lucide`]
//! (`assets/lucide/*.svg`; ISC — THIRD-PARTY-NOTICES.txt).
//!
//! Named constants are this shell's own chrome. Ring slots resolve a Lucide
//! name through [`by_name`] so this list cannot drift from the slot table.
//! GTK strokes the same path strings with `gsk::Path`.
//!
//! Device marks (`PAD_*`, [`KEYBOARD`], [`REMOTE`]) are this crate's own. Pad
//! outlines come from Kenney Input Prompts 1.5 (CC0,
//! https://kenney.nl/assets/input-prompts), refit to the 24-unit box. The tells
//! inside them are drawn here.

use crate::theme::stroke;
use skia_safe::{utils::parse_path, Canvas, Color4f, PaintCap, PaintJoin};

/// One icon's 24×24 path data.
#[derive(Clone, Copy)]
pub struct Icon(pub &'static str);

pub const CHEVRON_DOWN: Icon = Icon(pf_client_core::lucide::CHEVRON_DOWN);
pub const CHEVRON_LEFT: Icon = Icon(pf_client_core::lucide::CHEVRON_LEFT);
pub const CHEVRON_RIGHT: Icon = Icon(pf_client_core::lucide::CHEVRON_RIGHT);
pub const CHEVRON_UP: Icon = Icon(pf_client_core::lucide::CHEVRON_UP);
pub const CORNER_DOWN_LEFT: Icon = Icon(pf_client_core::lucide::CORNER_DOWN_LEFT);
pub const PLUS: Icon = Icon(pf_client_core::lucide::PLUS);

/// Kenney `controller_xbox360.svg` left half, mirrored about x = 12; sticks and the
/// guide ring between them drawn here.
pub const PAD_XBOX_360: Icon = Icon(
    "M12 15.6Q8.5 15.6 7.45 16.6Q5.65 18.66 4.3 19.18Q2.3 20.06 1.7 18.77Q1 17.08 1.35 14.26Q1.7 11.28 2.5 8.71L2.87 7.62Q3.3 6.4 3.95 5.35Q5.4 3.94 7.3 4.26L8.49 4.94Q9.39 5.41 10.48 5.5L12 5.5L13.52 5.5Q14.61 5.41 15.51 4.94L16.7 4.26Q18.6 3.94 20.05 5.35Q20.7 6.4 21.13 7.62L21.5 8.71Q22.3 11.28 22.65 14.26Q23 17.08 22.3 18.77Q21.7 20.06 19.7 19.18Q18.35 18.66 16.55 16.6Q15.5 15.6 12 15.6ZM4.9 9.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM13.9 12.8a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM10.8 8a1.2 1.2 0 1 0 2.4 0a1.2 1.2 0 1 0 -2.4 0Z",
);
/// Kenney `controller_xboxone.svg` outline; sticks drawn here.
pub const PAD_XBOX_ONE: Icon = Icon(
    "M8.2 15.4Q7.12 15.35 6.42 16.34L4.4 18.97Q3.97 19.47 3.54 19.69Q1.85 19.4 1.52 18.23Q1 16.48 1.34 14.52Q1.97 11.51 3.07 8.65Q3.9 6.24 4.44 6.2L4.58 5.72Q4.69 5.32 5.77 4.89L7.07 4.49Q7.95 4.31 8.29 4.49Q8.9 4.85 9.23 4.85L14.74 4.85Q15.08 4.85 15.71 4.49Q16.05 4.31 16.93 4.49L18.21 4.89Q19.31 5.32 19.42 5.72L19.56 6.2Q20.1 6.24 20.93 8.65Q22.03 11.51 22.66 14.52Q23 16.48 22.48 18.23Q22.12 19.4 20.46 19.69Q20.03 19.47 19.58 18.97Q18.55 17.78 17.58 16.34Q16.88 15.35 15.78 15.4L8.2 15.4ZM5.3 9.1a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM12.7 12.9a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0Z",
);
/// Kenney `controller_xboxone.svg` outline; sticks and back paddles drawn here.
pub const PAD_XBOX_ELITE: Icon = Icon(
    "M8.2 15.4Q7.12 15.35 6.42 16.34L4.4 18.97Q3.97 19.47 3.54 19.69Q1.85 19.4 1.52 18.23Q1 16.48 1.34 14.52Q1.97 11.51 3.07 8.65Q3.9 6.24 4.44 6.2L4.58 5.72Q4.69 5.32 5.77 4.89L7.07 4.49Q7.95 4.31 8.29 4.49Q8.9 4.85 9.23 4.85L14.74 4.85Q15.08 4.85 15.71 4.49Q16.05 4.31 16.93 4.49L18.21 4.89Q19.31 5.32 19.42 5.72L19.56 6.2Q20.1 6.24 20.93 8.65Q22.03 11.51 22.66 14.52Q23 16.48 22.48 18.23Q22.12 19.4 20.46 19.69Q20.03 19.47 19.58 18.97Q18.55 17.78 17.58 16.34Q16.88 15.35 15.78 15.4L8.2 15.4ZM5.3 9.1a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM12.7 12.9a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM8.4 17L7.9 19.2M15.6 17L16.1 19.2",
);
/// Kenney `controller_playstation4.svg` outline; sticks and touchpad drawn here.
pub const PAD_DUALSHOCK_4: Icon = Icon(
    "M15.4 5.83Q15.65 5.83 15.72 5.97L16.26 5.97Q16.99 5.99 17.15 5.74L17.31 5.74L17.31 5.47Q17.85 5.15 18.71 5.17Q19.48 5.17 20.03 5.47L20.03 5.74L20.3 5.88Q21.03 6.83 21.55 8.37L22.41 11.82Q22.8 13.54 22.86 15.54Q23 16.97 22.32 17.99Q21.75 18.85 20.51 18.78Q19.44 18.69 18.96 17.94L18.06 16.29L17.33 14.15L16.92 13.91Q16.26 14.56 15.36 14.47Q14.63 14.47 13.93 13.88L10.07 13.88Q9.37 14.47 8.64 14.47Q7.74 14.56 7.08 13.91Q6.76 13.97 6.67 14.15L5.94 16.29L5.01 17.94Q4.56 18.69 3.49 18.78Q2.25 18.85 1.68 17.99Q1 16.97 1.14 15.54Q1.2 13.54 1.59 11.82L2.45 8.37Q2.97 6.83 3.7 5.88L3.97 5.74L3.97 5.47Q4.49 5.17 5.29 5.17Q6.15 5.15 6.69 5.47L6.69 5.74L6.85 5.74Q7.01 5.99 7.74 5.97L8.28 5.97Q8.33 5.83 8.6 5.83L15.4 5.83ZM9.9 6.6h4.2a0.6 0.6 0 0 1 0.6 0.6v2.2a0.6 0.6 0 0 1 -0.6 0.6h-4.2a0.6 0.6 0 0 1 -0.6 -0.6v-2.2a0.6 0.6 0 0 1 0.6 -0.6ZM7.1 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM13.9 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0Z",
);
/// Kenney `controller_playstation5.svg` outline; sticks, touchpad, light bars drawn here.
pub const PAD_DUALSENSE: Icon = Icon(
    "M12 4.91L17.06 5.28L17.31 5.23L17.38 4.82L18.71 4.75Q19.51 4.89 20.16 5.32L20.23 5.75L20.57 5.84Q21.64 8.02 22.27 10.4Q22.68 12.12 22.84 13.96Q23 15.66 22.64 17.32Q22.5 18.36 21.48 18.97L19.92 19.25Q19.69 19.2 19.53 18.84L18.67 16.73Q18.26 15.55 17.53 14.53Q17.22 14.1 16.35 14.08L15.9 14.14L15.88 14.14L14.49 14.23L9.51 14.23L8.12 14.14L8.1 14.14L7.65 14.08Q6.78 14.1 6.47 14.53Q5.74 15.55 5.33 16.73L4.47 18.84Q4.31 19.2 4.08 19.25L2.52 18.97Q1.5 18.36 1.36 17.32Q1 15.66 1.16 13.96Q1.32 12.12 1.73 10.4Q2.36 8.02 3.43 5.84L3.77 5.75L3.84 5.32Q4.49 4.89 5.29 4.75L6.62 4.82L6.69 5.23L6.94 5.28L12 4.91ZM8.9 5.2l0.8 4.4h4.6l0.8-4.4M7.5 5.6L8.3 9.5M16.5 5.6L15.7 9.5M7.1 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM13.9 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0Z",
);
/// [`PAD_DUALSENSE`] plus back paddles drawn here.
pub const PAD_DUALSENSE_EDGE: Icon = Icon(
    "M12 4.91L17.06 5.28L17.31 5.23L17.38 4.82L18.71 4.75Q19.51 4.89 20.16 5.32L20.23 5.75L20.57 5.84Q21.64 8.02 22.27 10.4Q22.68 12.12 22.84 13.96Q23 15.66 22.64 17.32Q22.5 18.36 21.48 18.97L19.92 19.25Q19.69 19.2 19.53 18.84L18.67 16.73Q18.26 15.55 17.53 14.53Q17.22 14.1 16.35 14.08L15.9 14.14L15.88 14.14L14.49 14.23L9.51 14.23L8.12 14.14L8.1 14.14L7.65 14.08Q6.78 14.1 6.47 14.53Q5.74 15.55 5.33 16.73L4.47 18.84Q4.31 19.2 4.08 19.25L2.52 18.97Q1.5 18.36 1.36 17.32Q1 15.66 1.16 13.96Q1.32 12.12 1.73 10.4Q2.36 8.02 3.43 5.84L3.77 5.75L3.84 5.32Q4.49 4.89 5.29 4.75L6.62 4.82L6.69 5.23L6.94 5.28L12 4.91ZM8.9 5.2l0.8 4.4h4.6l0.8-4.4M7.5 5.6L8.3 9.5M16.5 5.6L15.7 9.5M7.1 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM13.9 12.4a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM8.4 15.8L7.9 18M15.6 15.8L16.1 18",
);
/// Kenney `controller_switch_pro.svg` outline; sticks and the − + buttons drawn here.
pub const PAD_SWITCH_PRO: Icon = Icon(
    "M8.51 4.81L15.49 4.81Q15.94 4.61 16.39 4.47Q18.81 4.43 20.08 5.54L20.51 6.24L20.71 6.33L21.08 6.81Q21.98 10.04 22.59 14.25Q23 16.52 22.8 17.81Q22.46 18.94 21.3 19.34Q20.31 19.59 19.58 18.87L18.18 16.36Q17.66 14.91 16.53 15L12 15L12 14.98L7.47 15Q6.34 14.91 5.8 16.36L4.4 18.87Q3.69 19.57 2.7 19.34Q1.54 18.94 1.2 17.81Q1 16.52 1.41 14.25Q2.02 10.04 2.92 6.81L3.29 6.33L3.49 6.24L3.92 5.52Q5.19 4.41 7.61 4.47L8.51 4.81ZM4.8 8.6a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM13.2 12.3a1.5 1.5 0 1 0 3 0a1.5 1.5 0 1 0 -3 0ZM9.5 7.2L11.1 7.2M12.9 7.2L14.5 7.2M13.7 6.4L13.7 8",
);
/// Kenney `controller_steam.svg` outline; round pads drawn here.
pub const PAD_STEAM_CONTROLLER: Icon = Icon(
    "M9.51 14.61Q8.05 14.63 7.55 15.4L5.81 18.62Q5.2 19.62 3.88 19.26Q2.25 18.67 1.54 17.4Q1 16.15 1.16 14.31Q1.39 12.48 1.75 10.73Q2.18 8.73 2.86 6.96L3.22 6.04Q3.36 5.24 4.04 4.79Q4.61 4.4 6.24 4.38L7.69 4.38Q7.96 4.38 8.05 4.54L15.95 4.54Q16.04 4.38 16.33 4.38L17.76 4.38Q19.39 4.4 19.96 4.79Q20.64 5.24 20.78 6.04L21.14 6.96Q21.84 8.73 22.25 10.73L22.86 14.31Q23 16.15 22.46 17.4Q21.78 18.67 20.12 19.26Q18.83 19.62 18.19 18.62L16.47 15.4Q15.97 14.63 14.52 14.61L9.51 14.61ZM3.8 8.6a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0 -5 0ZM15.2 8.6a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0 -5 0Z",
);
/// Kenney `controller_steam_new.svg` outline; square pads and sticks drawn here.
pub const PAD_STEAM_CONTROLLER_2: Icon = Icon(
    "M7.18 16.61Q6.02 16.63 5.74 17.47Q4.76 19.93 3.26 19.45Q1 18.73 1.05 16.06Q1.64 11.41 2.44 8.56Q3.17 4.77 4.74 4.48L11.99 4.07L19.24 4.48Q20.81 4.77 21.54 8.56Q22.34 11.41 22.93 16.06Q23 18.73 20.72 19.45Q19.22 19.93 18.24 17.47Q17.98 16.63 16.8 16.61L7.18 16.61ZM7.4 11.6h2.4a1 1 0 0 1 1 1v2.4a1 1 0 0 1 -1 1h-2.4a1 1 0 0 1 -1 -1v-2.4a1 1 0 0 1 1 -1ZM14.2 11.6h2.4a1 1 0 0 1 1 1v2.4a1 1 0 0 1 -1 1h-2.4a1 1 0 0 1 -1 -1v-2.4a1 1 0 0 1 1 -1ZM7.8 8a1.2 1.2 0 1 0 2.4 0a1.2 1.2 0 1 0 -2.4 0ZM13.8 8a1.2 1.2 0 1 0 2.4 0a1.2 1.2 0 1 0 -2.4 0Z",
);
/// Drawn here: the Steam Controller 2 puck under a wireless arc.
pub const PAD_STEAM_PUCK: Icon = Icon(
    "M4 11a8 3 0 1 0 16 0a8 3 0 1 0-16 0ZM4 11v3a8 3 0 0 0 16 0v-3M9.5 5.6a4 2.4 0 0 1 5 0M7.6 3.6a7 3.4 0 0 1 8.8 0",
);
/// Drawn here: the Deck's body, screen and sticks.
pub const PAD_STEAM_DECK: Icon = Icon(
    "M4.2 6.6h15.6a3.2 3.2 0 0 1 3.2 3.2v4.4a3.2 3.2 0 0 1 -3.2 3.2h-15.6a3.2 3.2 0 0 1 -3.2 -3.2v-4.4a3.2 3.2 0 0 1 3.2 -3.2ZM7.8 8.6h8.4a0.8 0.8 0 0 1 0.8 0.8v5.2a0.8 0.8 0 0 1 -0.8 0.8h-8.4a0.8 0.8 0 0 1 -0.8 -0.8v-5.2a0.8 0.8 0 0 1 0.8 -0.8ZM3 9.6a1.1 1.1 0 1 0 2.2 0a1.1 1.1 0 1 0 -2.2 0ZM18.8 9.6a1.1 1.1 0 1 0 2.2 0a1.1 1.1 0 1 0 -2.2 0Z",
);
/// Drawn here: a keyboard with a key row and a space bar.
pub const KEYBOARD: Icon = Icon(
    "M4.2 5.5h15.6a2.2 2.2 0 0 1 2.2 2.2v8.6a2.2 2.2 0 0 1 -2.2 2.2h-15.6a2.2 2.2 0 0 1 -2.2 -2.2v-8.6a2.2 2.2 0 0 1 2.2 -2.2ZM8 14.5L16 14.5M6 10L6.01 10M10 10L10.01 10M14 10L14.01 10M18 10L18.01 10",
);
/// Drawn here: a TV remote with its D-pad ring.
pub const REMOTE: Icon = Icon(
    "M11.5 0.5h1a4.5 4.5 0 0 1 4.5 4.5v14a4.5 4.5 0 0 1 -4.5 4.5h-1a4.5 4.5 0 0 1 -4.5 -4.5v-14a4.5 4.5 0 0 1 4.5 -4.5ZM9.2 6.8a2.8 2.8 0 1 0 5.6 0a2.8 2.8 0 1 0 -5.6 0ZM12 13.5L12.01 13.5",
);

/// Every device mark with its sheet label.
#[cfg(test)]
pub(crate) const DEVICE_MARKS: [(&str, Icon); 13] = [
    ("Xbox 360", PAD_XBOX_360),
    ("Xbox One", PAD_XBOX_ONE),
    ("Xbox Elite", PAD_XBOX_ELITE),
    ("DualShock 4", PAD_DUALSHOCK_4),
    ("DualSense", PAD_DUALSENSE),
    ("DualSense Edge", PAD_DUALSENSE_EDGE),
    ("Switch Pro", PAD_SWITCH_PRO),
    ("Steam Controller", PAD_STEAM_CONTROLLER),
    ("Steam Controller 2", PAD_STEAM_CONTROLLER_2),
    ("SC2 Puck", PAD_STEAM_PUCK),
    ("Steam Deck", PAD_STEAM_DECK),
    ("Keyboard", KEYBOARD),
    ("Remote", REMOTE),
];

pub fn by_name(name: &str) -> Option<Icon> {
    pf_client_core::lucide::path(name).map(Icon)
}

/// Centre `(x, y)`, 24-unit box scaled to `box_px`. Stroke 2 is Lucide's
/// native weight, so it scales with the box.
pub fn draw_icon(canvas: &Canvas, icon: Icon, x: f32, y: f32, box_px: f32, color: Color4f) {
    draw_icon_weight(canvas, icon, x, y, box_px, 2.0 * box_px / 24.0, color);
}

/// [`draw_icon`] with the stroke in pixels, so one weight holds at every box size.
pub fn draw_icon_weight(
    canvas: &Canvas,
    icon: Icon,
    x: f32,
    y: f32,
    box_px: f32,
    stroke_px: f32,
    color: Color4f,
) {
    let Some(path) = parse_path::from_svg(icon.0) else {
        return;
    };
    let f = box_px / 24.0;
    let mut p = stroke(color, stroke_px / f);
    p.set_stroke_cap(PaintCap::Round);
    p.set_stroke_join(PaintJoin::Round);
    canvas.save();
    canvas.translate((x - 12.0 * f, y - 12.0 * f));
    canvas.scale((f, f));
    canvas.draw_path(&path, &p);
    canvas.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tight bounds, not `bounds()`: an arc's conic control points sit outside
    /// the curve they draw, so the loose box flags a correct circle. Walk the
    /// whole shared table — GTK strokes the same strings and has no parser.
    #[test]
    fn every_icon_parses_and_fits_its_box() {
        for (name, data, _glyph) in pf_client_core::lucide::ALL {
            let path =
                parse_path::from_svg(data).unwrap_or_else(|| panic!("{name} does not parse"));
            let b = path.compute_tight_bounds();
            assert!(
                b.left >= -0.5 && b.top >= -0.5 && b.right <= 24.5 && b.bottom <= 24.5,
                "{name} leaves the box: {b:?}"
            );
        }
    }

    #[test]
    fn every_device_mark_parses_and_fits_its_box() {
        for (name, icon) in DEVICE_MARKS {
            let path =
                parse_path::from_svg(icon.0).unwrap_or_else(|| panic!("{name} does not parse"));
            let b = path.compute_tight_bounds();
            assert!(
                b.left >= 0.0 && b.top >= 0.0 && b.right <= 24.0 && b.bottom <= 24.0,
                "{name} leaves the box: {b:?}"
            );
        }
    }

    #[test]
    fn the_consoles_icons_come_from_the_shared_table() {
        assert_eq!(PLUS.0, pf_client_core::lucide::path("plus").unwrap());
        assert_eq!(
            by_name("gamepad-2").unwrap().0,
            pf_client_core::lucide::path("gamepad-2").unwrap()
        );
        assert!(by_name("no-such-icon").is_none());
    }
}
