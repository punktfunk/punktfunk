//! One session's stylus: [`PenTracker`] diffs batches into transitions on a lazily
//! created [`VirtualPen`]. No ink, no device; the tablet dies with the session.
//! Both planes feed it; the stroke timeout is the native plane's own.

use crate::inject::pen::VirtualPen;
use punktfunk_core::quic::{PenBatch, PenTracker, PenTransition};

#[derive(Default)]
pub(crate) struct PenSink {
    tracker: PenTracker,
    dev: Option<VirtualPen>,
    /// Create failed once: never retry at 240 Hz. The tracker still consumes batches
    /// so its state stays coherent.
    create_failed: bool,
    /// Reused transition buffer (a batch yields a few).
    out: Vec<PenTransition>,
}

impl PenSink {
    pub(crate) fn apply(&mut self, batch: &PenBatch) {
        if self.dev.is_none() && !self.create_failed {
            match VirtualPen::create() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    // The pen cap was advertised from the same probe; permissions can
                    // still change between then and first ink.
                    self.create_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "pen: virtual tablet creation failed — dropping pen input this session"
                    );
                }
            }
        }
        self.out.clear();
        self.tracker.apply(batch, &mut self.out);
        self.flush();
    }

    /// Lift buttons, then tip, then proximity.
    pub(crate) fn force_release(&mut self) {
        self.out.clear();
        self.tracker.force_release(&mut self.out);
        self.flush();
    }

    pub(crate) fn active(&self) -> bool {
        self.tracker.is_active()
    }

    fn flush(&mut self) {
        if let Some(dev) = self.dev.as_mut() {
            dev.apply_batch(&self.out);
        }
    }
}
