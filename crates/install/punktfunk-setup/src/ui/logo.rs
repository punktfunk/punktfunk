//! The lens mark, rasterised from the favicon geometry rather than embedded.
//!
//! `web/public/favicon.svg` is two equal circles; their intersection is the lens.
//! Each frame is a point-in-circle test over the union's bounding box. Scale by
//! changing `MARK_COLS` / `MARK_ROWS`.
//!
//! A pixel is one whole cell, painted as a background fill, not a block glyph —
//! see `render`. Cells are not square, so neither is the grid. No anti-aliasing:
//! the terminal background is unknown, so there is nothing safe to blend toward.
//!
//! Skip rules: `design/installer-v2.md` D7.

use crate::ui::theme::{Caps, Colors, Layer, Rgb};

/// One cell per pixel. Columns and rows differ because cells are not square.
///
/// A terminal cell is ~2.2:1. Sampling the square mark into 20×9 cancels that:
/// the shape stays round across the 2.0–2.4 range, worst case about 10%.
/// Below 16×7 the lens thins to a line; below 14×6 the circles collapse.
pub const MARK_COLS: usize = 20;
pub const MARK_ROWS: usize = 9;

pub const MARK_TEXT_ROWS: usize = MARK_ROWS;

const LIGHT: Rgb = Rgb(0xa7, 0x9f, 0xf8);
const DEEP: Rgb = Rgb(0x6c, 0x5b, 0xf3);
const LENS: Rgb = Rgb(0xd2, 0xc9, 0xfb);

/// Circle centres and radius in the favicon's 1000×1000 viewBox.
const R: f32 = 194.41;
const LIGHT_C: (f32, f32) = (403.037, 597.262);
const DEEP_C: (f32, f32) = (597.808, 402.853);

/// Start gap in viewBox units. ~one radius, so at t=0 each circle is mostly off-frame.
const SPREAD: f32 = 190.0;

/// 15 frames at 40 ms ≈ 600 ms: long enough to read as motion, short enough not to stall install.
pub const FRAMES: usize = 15;
pub const FRAME_MS: u64 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intro {
    Animated,
    Static,
    Plain,
}

/// Skip ladder (`design/installer-v2.md` D7). `--yes` keeps the mark but never
/// animates: an unattended transcript must not carry 15 repaints.
pub fn intro_level(caps: &Caps, yes: bool) -> Intro {
    // +2 is the indent. A terminal as wide as the mark would wrap the last two
    // columns and break the frame's repaint arithmetic.
    if !caps.tty || caps.colors < Colors::Ansi256 || usize::from(caps.width) < MARK_COLS + 2 {
        return Intro::Plain;
    }
    if yes {
        Intro::Static
    } else {
        Intro::Animated
    }
}

fn bbox() -> (f32, f32, f32, f32) {
    let x0 = (LIGHT_C.0 - R).min(DEEP_C.0 - R);
    let y0 = (LIGHT_C.1 - R).min(DEEP_C.1 - R);
    let x1 = (LIGHT_C.0 + R).max(DEEP_C.0 + R);
    let y1 = (LIGHT_C.1 + R).max(DEEP_C.1 + R);
    (x0, y0, x1 - x0, y1 - y0)
}

fn diagonal() -> (f32, f32) {
    let (dx, dy) = (DEEP_C.0 - LIGHT_C.0, DEEP_C.1 - LIGHT_C.1);
    let len = dx.hypot(dy);
    (dx / len, dy / len)
}

/// Left circle is host, right is client. Unselected halves mute rather than drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub host: bool,
    pub client: bool,
}

impl Default for Parts {
    fn default() -> Self {
        Parts {
            host: true,
            client: true,
        }
    }
}

/// Desaturate toward the colour's own luminance. Do not dim: the terminal
/// background is unknown, so darker reads as muted on dark and as emphasis on light.
fn muted(c: Rgb) -> Rgb {
    const DESAT: f32 = 0.8;
    let luma = 0.299 * f32::from(c.0) + 0.587 * f32::from(c.1) + 0.114 * f32::from(c.2);
    let mix = |v: u8| (f32::from(v) + (luma - f32::from(v)) * DESAT).round() as u8;
    Rgb(mix(c.0), mix(c.1), mix(c.2))
}

pub fn frame(t: f32) -> Vec<Vec<Option<Rgb>>> {
    frame_parts(t, Parts::default())
}

/// Pixel grid at `t` (0 = apart, 1 = at rest). `None` is uncovered.
///
/// Draw order matches the SVG: deep over light, lens over both. The lens is
/// lit only when both halves are installed.
pub fn frame_parts(t: f32, parts: Parts) -> Vec<Vec<Option<Rgb>>> {
    let light_c = if parts.host { LIGHT } else { muted(LIGHT) };
    let deep_c = if parts.client { DEEP } else { muted(DEEP) };
    let lens_c = if parts.host && parts.client {
        LENS
    } else {
        muted(LENS)
    };
    // Ease-out cubic: most of the travel happens early so the circles settle.
    let eased = 1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3);
    let gap = SPREAD * (1.0 - eased);
    let (dx, dy) = diagonal();
    let light = (LIGHT_C.0 - dx * gap, LIGHT_C.1 - dy * gap);
    let deep = (DEEP_C.0 + dx * gap, DEEP_C.1 + dy * gap);

    let (x0, y0, w, h) = bbox();
    let inside = |c: (f32, f32), x: f32, y: f32| (x - c.0).hypot(y - c.1) <= R;
    (0..MARK_ROWS)
        .map(|row| {
            let y = y0 + (row as f32 + 0.5) * h / MARK_ROWS as f32;
            (0..MARK_COLS)
                .map(|col| {
                    let x = x0 + (col as f32 + 0.5) * w / MARK_COLS as f32;
                    match (inside(light, x, y), inside(deep, x, y)) {
                        (true, true) => Some(lens_c),
                        (_, true) => Some(deep_c),
                        (true, _) => Some(light_c),
                        _ => None,
                    }
                })
                .collect()
        })
        .collect()
}

/// One cell per pixel, painted as a background fill.
///
/// Do not use half-block glyphs (`▀`). A glyph only covers its font box and
/// leaves a hairline between rows. A background fill covers the whole cell.
pub fn render(grid: &[Vec<Option<Rgb>>], caps: &Caps, indent: usize) -> String {
    let mut out = String::new();
    for row in grid {
        out.push_str(&" ".repeat(indent));
        for cell in row {
            // Uncovered cells must restore the default background or they smear the previous colour.
            match cell {
                Some(c) => out.push_str(&caps.paint(*c, Layer::Bg)),
                None => out.push_str(&caps.clear(Layer::Bg)),
            }
            out.push(' ');
        }
        out.push_str(&caps.reset());
        out.push('\n');
    }
    out
}

pub fn still(caps: &Caps, indent: usize, parts: Parts) -> String {
    render(&frame_parts(1.0, parts), caps, indent)
}

pub fn ascii(grid: &[Vec<Option<Rgb>>]) -> String {
    let mut out = String::new();
    for row in grid {
        for cell in row {
            out.push(match *cell {
                None => '.',
                Some(LENS) => '#',
                Some(DEEP) => 'o',
                _ => '+',
            });
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::Env;

    fn caps(colors: Colors, width: u16, tty: bool) -> Caps {
        Caps { tty, colors, width }
    }

    #[test]
    fn the_skip_ladder_matches_d7() {
        let good = caps(Colors::Truecolor, 80, true);
        assert_eq!(intro_level(&good, false), Intro::Animated);
        assert_eq!(
            intro_level(&good, true),
            Intro::Static,
            "--yes keeps the mark, not the motion"
        );
        assert_eq!(
            intro_level(&caps(Colors::Truecolor, 80, false), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::Ansi16, 80, true), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::None, 80, true), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::Truecolor, 12, true), false),
            Intro::Plain
        );
    }

    #[test]
    fn a_no_color_terminal_never_reaches_the_mark() {
        let env = Env::of(&[("NO_COLOR", "1"), ("TERM", "xterm-256color")]);
        let caps = Caps::detect(&env, true, 120);
        assert_eq!(intro_level(&caps, false), Intro::Plain);
    }

    #[test]
    fn the_settled_mark_is_the_expected_pixel_grid() {
        let art = ascii(&frame(1.0));
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mark.txt");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &art).expect("write mark golden");
            return;
        }
        let want = std::fs::read_to_string(&path).expect("mark golden — UPDATE_GOLDEN=1");
        assert_eq!(art, want);
    }

    #[test]
    fn the_mark_is_square_and_the_grid_is_the_declared_size() {
        let grid = frame(1.0);
        assert_eq!(grid.len(), MARK_ROWS);
        assert!(grid.iter().all(|row| row.len() == MARK_COLS));
    }

    #[test]
    fn the_lens_lights_up_as_the_circles_meet() {
        let lens_pixels = |t: f32| {
            frame(t)
                .iter()
                .flatten()
                .filter(|c| **c == Some(LENS))
                .count()
        };
        assert_eq!(
            lens_pixels(0.0),
            0,
            "fully apart, there is no overlap to light"
        );
        assert!(lens_pixels(1.0) > 0, "at rest the lens is the whole point");
        // Non-decreasing, not strictly increasing: one cell per pixel plateaus between frames.
        let mut prev = 0;
        for step in 0..=10 {
            let now = lens_pixels(step as f32 / 10.0);
            assert!(now >= prev, "the lens shrank as the circles came together");
            prev = now;
        }
    }

    #[test]
    fn an_edge_cell_leaves_the_terminal_background_alone() {
        let caps = caps(Colors::Truecolor, 80, true);
        let text = render(&frame(1.0), &caps, 2);
        assert!(
            text.contains("\x1b[49m"),
            "no cell returned the background to default"
        );
        assert!(
            text.lines().all(|l| l.starts_with("  ")),
            "the indent is not applied"
        );
    }

    #[derive(Debug, PartialEq)]
    struct Cell {
        ch: char,
        fg: Option<Rgb>,
        bg: Option<Rgb>,
    }

    fn parse_cells(line: &str) -> Vec<Cell> {
        let rgb = |seq: &str| {
            let parts: Vec<u8> = seq
                .trim_start_matches("[38;2;")
                .trim_start_matches("[48;2;")
                .trim_end_matches('m')
                .split(';')
                // `::<u8>` is required: Windows pulls extra `FromIterator` impls and inference fails.
                .filter_map(|v| v.parse::<u8>().ok())
                .collect();
            Rgb(parts[0], parts[1], parts[2])
        };
        let (mut fg, mut bg) = (None, None);
        let mut cells = Vec::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                let mut seq = String::new();
                for c in chars.by_ref() {
                    seq.push(c);
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                match seq.as_str() {
                    s if s.starts_with("[38;2;") => fg = Some(rgb(s)),
                    s if s.starts_with("[48;2;") => bg = Some(rgb(s)),
                    "[39m" => fg = None,
                    "[49m" => bg = None,
                    "[0m" => {
                        fg = None;
                        bg = None;
                    }
                    _ => {}
                }
                continue;
            }
            cells.push(Cell { ch: c, fg, bg });
        }
        cells
    }

    #[test]
    fn every_cell_paints_exactly_what_the_grid_says() {
        let caps = caps(Colors::Truecolor, 80, true);
        const INDENT: usize = 2;
        for t in [0.0, 0.3, 0.7, 1.0] {
            let grid = frame(t);
            let text = render(&grid, &caps, INDENT);
            for (row, line) in text.lines().enumerate() {
                let cells = parse_cells(line);
                for pad in &cells[..INDENT] {
                    assert_eq!(pad.ch, ' ');
                    assert_eq!(pad.bg, None, "the indent carried a background");
                }
                for (col, cell) in cells[INDENT..].iter().enumerate() {
                    let what = format!("t={t} row {row} col {col}");
                    assert_eq!(cell.ch, ' ', "{what}: every cell is a fill, never a glyph");
                    assert_eq!(cell.bg, grid[row][col], "{what}");
                }
            }
        }
    }

    #[test]
    fn the_mark_renders_round_on_a_real_cell() {
        let grid = frame(1.0);
        let (mut min_r, mut max_r) = (usize::MAX, 0usize);
        let (mut min_c, mut max_c) = (usize::MAX, 0usize);
        for (r, row) in grid.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if cell.is_some() {
                    min_r = min_r.min(r);
                    max_r = max_r.max(r);
                    min_c = min_c.min(c);
                    max_c = max_c.max(c);
                }
            }
        }
        let cols = (max_c - min_c + 1) as f32;
        let rows = (max_r - min_r + 1) as f32;
        for cell_aspect in [2.0f32, 2.18, 2.4] {
            let ratio = rows * cell_aspect / cols;
            assert!(
                (ratio - 1.0).abs() < 0.12,
                "on a {cell_aspect}:1 cell the mark renders {ratio:.2}x taller than wide"
            );
        }
    }

    #[test]
    fn muting_greys_a_colour_without_darkening_it() {
        let luma =
            |c: Rgb| 0.299 * f32::from(c.0) + 0.587 * f32::from(c.1) + 0.114 * f32::from(c.2);
        let spread = |c: Rgb| i32::from(c.0.max(c.1).max(c.2)) - i32::from(c.0.min(c.1).min(c.2));
        for c in [LIGHT, DEEP, LENS] {
            let m = muted(c);
            assert!(
                (luma(m) - luma(c)).abs() < 2.0,
                "{c:?} changed brightness when muted"
            );
            assert!(
                spread(m) * 3 < spread(c),
                "{c:?} kept its colour when muted"
            );
        }
    }

    #[test]
    fn only_the_unselected_half_of_the_mark_is_muted() {
        let both = frame(1.0);
        let find = |want: Rgb| {
            both.iter()
                .enumerate()
                .find_map(|(r, row)| row.iter().position(|c| *c == Some(want)).map(|c| (r, c)))
                .expect("the settled mark carries every colour")
        };
        let (lr, lc) = find(LIGHT);
        let (dr, dc) = find(DEEP);
        let (nr, nc) = find(LENS);

        let host = frame_parts(
            1.0,
            Parts {
                host: true,
                client: false,
            },
        );
        assert_eq!(host[lr][lc], Some(LIGHT), "the host circle must stay lit");
        assert_eq!(
            host[dr][dc],
            Some(muted(DEEP)),
            "the client circle must mute"
        );
        assert_eq!(
            host[nr][nc],
            Some(muted(LENS)),
            "nothing meets with one half installed"
        );

        let client = frame_parts(
            1.0,
            Parts {
                host: false,
                client: true,
            },
        );
        assert_eq!(client[lr][lc], Some(muted(LIGHT)));
        assert_eq!(client[dr][dc], Some(DEEP));

        let all = frame_parts(
            1.0,
            Parts {
                host: true,
                client: true,
            },
        );
        assert_eq!(all[nr][nc], Some(LENS), "both installed lights the lens");
    }

    #[test]
    fn the_mark_occupies_one_text_row_per_pixel_row() {
        let caps = caps(Colors::Truecolor, 80, true);
        assert_eq!(
            still(&caps, 0, Parts::default()).lines().count(),
            MARK_TEXT_ROWS
        );
    }
}
