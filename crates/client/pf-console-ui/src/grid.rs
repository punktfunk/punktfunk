//! Cursor and grid arithmetic the library screens share, ported from the GTK launcher.
//!
//! [`GridShape`] is the layout both the cursor maths and the renderer read.

pub const JUMP: i32 = 5;

/// `clamp` lands jumps on the ends; a plain step refuses to leave them.
#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    Moved(i32),
    Boundary,
}

pub fn step_cursor(cursor: i32, len: usize, delta: i32, clamp: bool) -> StepResult {
    if len == 0 {
        return StepResult::Boundary;
    }
    let max = len as i32 - 1;
    let target = if clamp {
        (cursor + delta).clamp(0, max)
    } else {
        cursor + delta
    };
    if target == cursor || target < 0 || target > max {
        StepResult::Boundary
    } else {
        StepResult::Moved(target)
    }
}
/// Grid cell: same 2:3 as the poster at ~⅔ size, so three rows plus a readable detail band at 800-tall.
pub const GRID_W: f64 = 150.0;
pub const GRID_H: f64 = 225.0;
pub const GRID_GAP: f64 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDir {
    Left,
    Right,
    Up,
    Down,
    PageBack,
    PageForward,
}

/// Shoulder jump, rows (≈ one screen).
pub const GRID_PAGE_ROWS: i32 = 3;

/// Layout both cursor math and the renderer read.
///
/// The launcher prefix occupies its own rows; the games section restarts at column 0.
/// A uniform `index % cols` grid only agrees when `launchers` is a multiple of `cols`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridShape {
    /// Cells per row, from the last frame actually drawn — not derived twice from two widths.
    pub cols: usize,
    /// Filtered count; the cursor indexes this.
    pub len: usize,
    /// Where the games section starts, or 0 when the field is one continuous run.
    pub split: usize,
}

impl GridShape {
    /// `launchers` is the leading run. Split only when both halves exist; otherwise a plain grid.
    pub fn new(len: usize, cols: usize, launchers: usize) -> GridShape {
        let split = if launchers > 0 && launchers < len {
            launchers
        } else {
            0
        };
        GridShape { cols, len, split }
    }

    /// First row of the games section; ignore when `split == 0`.
    pub fn split_row(&self) -> usize {
        self.split.div_ceil(self.cols.max(1))
    }

    pub fn cell_of(&self, i: usize) -> (usize, usize) {
        let cols = self.cols.max(1);
        if self.split > 0 && i >= self.split {
            let j = i - self.split;
            (self.split_row() + j / cols, j % cols)
        } else {
            (i / cols, i % cols)
        }
    }

    pub fn rows(&self) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 {
            self.split_row() + (self.len - self.split).div_ceil(cols)
        } else {
            self.len.div_ceil(cols)
        }
    }

    pub fn row_start(&self, row: usize) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 && row >= self.split_row() {
            self.split + (row - self.split_row()) * cols
        } else {
            row * cols
        }
    }

    /// Cells this row holds. The last launcher row ends at `split`; the last field row at `len`.
    pub fn row_len(&self, row: usize) -> usize {
        let start = self.row_start(row);
        let end = if self.split > 0 && row + 1 == self.split_row() {
            self.split
        } else {
            self.len
        };
        end.saturating_sub(start).min(self.cols.max(1))
    }
}

/// Grid cursor against the shape the renderer is drawing.
///
/// Horizontal: walk the row, refuse at that row's true ends (no wrap). Vertical and page:
/// change row only, carrying `col_hint` clamped into the target row. Only leaving the
/// grid is a boundary. A short row is a layout accident — Down clamps onto the last title.
/// `col_hint` is the column last chosen ([`grid_col_hint`]), so a two-wide launcher row
/// is reversible.
pub fn grid_step(cursor: i32, shape: GridShape, col_hint: usize, dir: GridDir) -> StepResult {
    if shape.len == 0 || shape.cols == 0 {
        return StepResult::Boundary;
    }
    // Outside the field: library shortened. Nearest real cell, so the next press heals it.
    let (row, col) = shape.cell_of((cursor.max(0) as usize).min(shape.len - 1));
    let moved = |i: usize| {
        if i as i32 == cursor {
            StepResult::Boundary
        } else {
            StepResult::Moved(i as i32)
        }
    };
    match dir {
        GridDir::Left => {
            if col == 0 {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col - 1)
            }
        }
        GridDir::Right => {
            if col + 1 >= shape.row_len(row) {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col + 1)
            }
        }
        GridDir::Up | GridDir::Down | GridDir::PageBack | GridDir::PageForward => {
            let (d, paging) = match dir {
                GridDir::Up => (-1, false),
                GridDir::Down => (1, false),
                GridDir::PageBack => (-GRID_PAGE_ROWS, true),
                _ => (GRID_PAGE_ROWS, true),
            };
            let target = (row as i32 + d).clamp(0, shape.rows() as i32 - 1) as usize;
            if target == row {
                // Step at the edge refuses. Page is clamped `step_cursor`: land on this row's end.
                if !paging {
                    return StepResult::Boundary;
                }
                let c = if d > 0 { shape.row_len(row) - 1 } else { 0 };
                return moved(shape.row_start(row) + c);
            }
            let c = col_hint.min(shape.row_len(target) - 1);
            moved(shape.row_start(target) + c)
        }
    }
}

/// Remembered column after a move: a horizontal step chooses it; a vertical step only borrows it.
pub fn grid_col_hint(shape: GridShape, prev: usize, dir: GridDir, landed: i32) -> usize {
    match dir {
        GridDir::Left | GridDir::Right => shape.cell_of(landed.max(0) as usize).1,
        _ => prev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 5, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(4, 5, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(2, 5, 1, false), StepResult::Moved(3));
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
    }

    /// Launcher-less, prefix, partial launcher row, degenerate two-cell, and single-column fields.
    const SHAPES: [(usize, usize, usize); 9] = [
        (11, 4, 0),
        (40, 5, 0),
        (30, 7, 2),
        (20, 4, 6),
        (4, 4, 2),
        (9, 3, 3),
        (7, 3, 7),
        (1, 3, 1),
        (13, 1, 2),
    ];

    /// Rows tile `0..len` once: `cell_of` and `row_start` agree; no index in two rows or in none.
    #[test]
    fn grid_rows_tile_the_field_exactly_once() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            let mut next = 0usize;
            for row in 0..s.rows() {
                let n = s.row_len(row);
                assert!((1..=cols).contains(&n), "{s:?} row {row} holds {n} cells");
                for col in 0..n {
                    let i = s.row_start(row) + col;
                    assert_eq!(i, next, "{s:?} row {row} does not follow the one above");
                    assert_eq!(s.cell_of(i), (row, col), "{s:?} disagrees about index {i}");
                    next += 1;
                }
            }
            assert_eq!(next, len, "{s:?} left cells in no row at all");
        }
    }

    /// Horizontal step refuses at the row's true end, not at `cols` (partial rows, launcher prefix).
    #[test]
    fn grid_horizontal_moves_walk_the_row_and_refuse_its_true_ends() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, col) = s.cell_of(i);
                let want = |first: bool, to: i32| {
                    if first {
                        StepResult::Boundary
                    } else {
                        StepResult::Moved(to)
                    }
                };
                let i = i as i32;
                // Hint is the column you would return to, not the one a horizontal step walks out of.
                for hint in 0..cols {
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Left),
                        want(col == 0, i - 1),
                        "{s:?} Left from {i}"
                    );
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Right),
                        want(col + 1 == s.row_len(row), i + 1),
                        "{s:?} Right from {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn grid_vertical_moves_change_row_and_carry_the_column() {
        const VERTICAL: [(GridDir, i32); 4] = [
            (GridDir::Up, -1),
            (GridDir::Down, 1),
            (GridDir::PageBack, -GRID_PAGE_ROWS),
            (GridDir::PageForward, GRID_PAGE_ROWS),
        ];
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, _) = s.cell_of(i);
                for hint in 0..cols {
                    for (dir, d) in VERTICAL {
                        let want_row = (row as i32 + d).clamp(0, s.rows() as i32 - 1) as usize;
                        let what = format!("{s:?} {dir:?} from {i} with hint {hint}");
                        match grid_step(i as i32, s, hint, dir) {
                            StepResult::Moved(to) => {
                                let (r, c) = s.cell_of(to as usize);
                                assert_eq!(r, want_row, "{what} landed in row {r}");
                                if r != row {
                                    assert_eq!(c, hint.min(s.row_len(r) - 1), "{what} column");
                                }
                            }
                            // Field edges refuse. A page already at the edge travels the current row.
                            StepResult::Boundary => assert_eq!(want_row, row, "{what} refused"),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_row_is_reachable_by_stepping() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                for (dir, end) in [(GridDir::Up, 0), (GridDir::Down, s.rows() - 1)] {
                    let mut cursor = i as i32;
                    for _ in 0..=s.rows() {
                        match grid_step(cursor, s, 0, dir) {
                            StepResult::Moved(to) => cursor = to,
                            StepResult::Boundary => break,
                        }
                    }
                    let (row, _) = s.cell_of(cursor as usize);
                    assert_eq!(row, end, "{s:?} {dir:?} from {i} stalled in row {row}");
                }
            }
        }
    }

    /// Two launchers on seven columns: alone on row 0, games restart at column 0.
    #[test]
    fn the_launcher_row_sits_squarely_above_the_games() {
        let s = GridShape::new(30, 7, 2);
        assert_eq!(s.rows(), 5);
        assert_eq!((s.row_len(0), s.row_len(1)), (2, 7));
        // Down from a launcher lands on the cover under it, not five columns right.
        assert_eq!(grid_step(0, s, 0, GridDir::Down), StepResult::Moved(2));
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(3));
        // Up out of the games band leaves it, rather than sliding along it.
        assert_eq!(grid_step(2, s, 0, GridDir::Up), StepResult::Moved(0));
        assert_eq!(grid_step(3, s, 1, GridDir::Up), StepResult::Moved(1));
        assert_eq!(grid_step(6, s, 4, GridDir::Up), StepResult::Moved(1));
        // Games row true ends are 2 and 8 — 6 and 7 are mid-row.
        assert_eq!(grid_step(6, s, 4, GridDir::Right), StepResult::Moved(7));
        assert_eq!(grid_step(7, s, 5, GridDir::Left), StepResult::Moved(6));
        assert_eq!(grid_step(8, s, 6, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 0, GridDir::Left), StepResult::Boundary);
    }

    #[test]
    fn a_crossing_returns_to_the_column_it_started_from() {
        use GridDir::{Down, Right, Up};
        let s = GridShape::new(30, 7, 2);
        // Mirrors `LibraryScreen::grid_move`: step, then `grid_col_hint`.
        let walk = |start: i32, dirs: &[GridDir]| {
            let (mut cursor, mut hint) = (start, s.cell_of(start.max(0) as usize).1);
            for &dir in dirs {
                if let StepResult::Moved(to) = grid_step(cursor, s, hint, dir) {
                    hint = grid_col_hint(s, hint, dir, to);
                    cursor = to;
                }
            }
            cursor
        };
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right]), 6);
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up]), 1);
        // A vertical move never spends the hint.
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up, Down]), 6);
        assert_eq!(
            walk(0, &[Down, Right, Right, Right, Right, Up, Down, Up, Down]),
            6
        );
    }

    #[test]
    fn grid_pages_by_rows_and_lands_on_the_ends() {
        let s = GridShape::new(40, 5, 0);
        assert_eq!(
            grid_step(0, s, 0, GridDir::PageForward),
            StepResult::Moved(15)
        );
        // Page past the end lands on the end (clamped `step_cursor`), it does not refuse.
        assert_eq!(
            grid_step(35, s, 0, GridDir::PageForward),
            StepResult::Moved(39)
        );
        assert_eq!(
            grid_step(39, s, 4, GridDir::PageForward),
            StepResult::Boundary
        );
        assert_eq!(grid_step(3, s, 3, GridDir::PageBack), StepResult::Moved(0));
        assert_eq!(grid_step(0, s, 0, GridDir::PageBack), StepResult::Boundary);
    }

    #[test]
    fn grid_rows_refuse_at_their_ends_but_the_tail_row_clamps() {
        // 11 items, 4 columns: rows of 4, 4, 3.
        let s = GridShape::new(11, 4, 0);
        assert_eq!(grid_step(1, s, 1, GridDir::Right), StepResult::Moved(2));
        assert_eq!(grid_step(2, s, 2, GridDir::Left), StepResult::Moved(1));
        // At a row's ends: refused, not wrapped onto the neighbouring row.
        assert_eq!(grid_step(3, s, 3, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(4, s, 0, GridDir::Left), StepResult::Boundary);
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(5));
        // Down into the short tail row clamps to the last item — column 3 does not exist there.
        assert_eq!(grid_step(7, s, 3, GridDir::Down), StepResult::Moved(10));
        assert_eq!(grid_step(10, s, 3, GridDir::Down), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 2, GridDir::Up), StepResult::Boundary);
        assert_eq!(grid_step(6, s, 2, GridDir::Up), StepResult::Moved(2));
    }

    #[test]
    fn grid_step_is_safe_on_a_degenerate_grid() {
        let empty = GridShape::new(0, 4, 0);
        assert_eq!(grid_step(0, empty, 0, GridDir::Right), StepResult::Boundary);
        let colless = GridShape::new(5, 0, 0);
        assert_eq!(
            grid_step(0, colless, 0, GridDir::Right),
            StepResult::Boundary
        );
        // One column: left/right are always refused, up/down still walk.
        let thin = GridShape::new(5, 1, 0);
        assert_eq!(grid_step(1, thin, 0, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(1, thin, 0, GridDir::Down), StepResult::Moved(2));
        // A cursor the library outgrew reads as the nearest real cell; the next press heals it.
        let s = GridShape::new(6, 3, 2);
        assert_eq!(grid_step(99, s, 0, GridDir::Up), StepResult::Moved(2));
        assert_eq!(grid_step(-4, s, 0, GridDir::Right), StepResult::Moved(1));
    }

    #[test]
    fn jump_clamps_onto_the_ends() {
        assert_eq!(step_cursor(1, 5, -JUMP, true), StepResult::Moved(0));
        assert_eq!(step_cursor(3, 5, JUMP, true), StepResult::Moved(4));
        assert_eq!(step_cursor(0, 5, -JUMP, true), StepResult::Boundary);
    }
}
