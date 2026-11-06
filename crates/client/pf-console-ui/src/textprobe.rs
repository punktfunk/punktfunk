//! Test-only record of the text runs the kit draws, for the pseudo-language gate
//! (`shell/tests/pseudo.rs`). `Fonts::draw` and the paragraph path report here; nothing is kept
//! outside a [`begin`] .. [`end`] window.

use skia_safe::Canvas;
use std::cell::{Cell, RefCell};

/// One run as it was drawn, in the canvas's local space.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    /// The string handed to Skia: a run cut short ends in `…`.
    pub text: String,
    /// Left and right edge of the ink.
    pub x0: f32,
    pub x1: f32,
    /// Top and bottom of the ink.
    pub y0: f32,
    pub y1: f32,
    /// The width the caller gave the run, when it gave one.
    pub box_w: Option<f32>,
    /// The canvas clip's left and right edge: what clips the run when the caller gave no box.
    pub clip: Option<(f32, f32)>,
    /// Which pass drew it: [`layer`] starts a new one.
    pub layer: u32,
}

thread_local! {
    static RUNS: RefCell<Option<Vec<Run>>> = const { RefCell::new(None) };
    static LAYER: Cell<u32> = const { Cell::new(0) };
    static BARE: Cell<bool> = const { Cell::new(false) };
}

/// Skips the backdrop on this thread. Its SkSL runs on the CPU here, a frame each, and none of
/// what it draws is text.
pub(crate) fn set_bare(on: bool) {
    BARE.with(|b| b.set(on));
}

pub(crate) fn bare() -> bool {
    BARE.with(Cell::get)
}

/// Starts recording on this thread.
pub(crate) fn begin() {
    LAYER.with(|l| l.set(0));
    RUNS.with(|r| *r.borrow_mut() = Some(Vec::new()));
}

/// Stops recording and returns what was drawn since [`begin`].
pub(crate) fn end() -> Vec<Run> {
    RUNS.with(|r| r.borrow_mut().take().unwrap_or_default())
}

/// What is drawn from here on sits over what came before: a modal over the screen under it.
/// Runs of different layers may overlap.
pub(crate) fn layer() {
    LAYER.with(|l| l.set(l.get() + 1));
}

/// A run of `width` from `x` between the rows `y`.
pub(crate) fn run(
    canvas: &Canvas,
    text: &str,
    (x, width): (f32, f32),
    (y0, y1): (f32, f32),
    box_w: Option<f32>,
) {
    RUNS.with(|r| {
        if let Some(runs) = r.borrow_mut().as_mut() {
            let clip = canvas.local_clip_bounds().map(|c| (c.left, c.right));
            runs.push(Run {
                text: text.to_owned(),
                x0: x,
                x1: x + width,
                y0,
                y1,
                box_w,
                clip,
                layer: LAYER.with(Cell::get),
            });
        }
    });
}
