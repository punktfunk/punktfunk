//! Session-transition latency trace (`design/first-frame-and-resize-latency.md`).
//!
//! One [`Trace`] per bring-up (`hello → first_packet`) or mid-stream resize
//! (`reconfigure_received → pipeline_rebuilt`) stamps millisecond stages across the
//! handshake, encode, and send threads. Completing the transition emits one `info!`
//! line and writes the total into a [`crate::session_status`] slot
//! (`time_to_first_frame_ms` / `last_resize_ms`).
//!
//! Stages are only those the session layer can see. Display-manager activation and
//! settle waits log their own deltas and correlate by wall clock.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One transition. [`finish`] emits the summary once; later calls no-op, so an
/// abandoned trace stays silent.
pub(crate) struct Trace {
    kind: &'static str,
    origin: Instant,
    stages: Mutex<Vec<(&'static str, u32)>>,
    finished: AtomicBool,
    /// Completed total for [`crate::session_status`]; 0 until [`finish`].
    total_ms: Arc<AtomicU32>,
}

impl Trace {
    pub(crate) fn start(kind: &'static str, total_ms: Arc<AtomicU32>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            origin: Instant::now(),
            stages: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
            total_ms,
        })
    }

    pub(crate) fn total_slot(&self) -> Arc<AtomicU32> {
        self.total_ms.clone()
    }

    /// First occurrence only: a retried build re-crosses stamp points. No-op after
    /// [`finish`] so steady-state paths that share a stamp stay free.
    pub(crate) fn mark(&self, stage: &'static str) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        let ms = self.origin.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut stages = self.stages.lock().unwrap();
        if stages.iter().any(|(s, _)| *s == stage) {
            return;
        }
        stages.push((stage, ms));
    }

    /// Stamp the last stage and emit the summary (first call only). Stores
    /// `total.max(1)` so a 0 ms finish is distinct from "not finished".
    pub(crate) fn finish(&self, stage: &'static str) {
        if self.finished.swap(true, Ordering::Relaxed) {
            return;
        }
        let total = self.origin.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let mut stages = self.stages.lock().unwrap();
        stages.push((stage, total));
        let line = stages
            .iter()
            .map(|(s, ms)| format!("{s}+{ms}"))
            .collect::<Vec<_>>()
            .join(" ");
        drop(stages);
        self.total_ms.store(total.max(1), Ordering::Relaxed);
        tracing::info!(
            kind = self.kind,
            total_ms = total,
            stages = %line,
            "session-transition trace"
        );
    }
}
