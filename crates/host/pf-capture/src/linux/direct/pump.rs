//! The direct capture's frame loop on the capture thread: the shell around [`FrameLoop`].
//!
//! Each turn samples the quit flag, the session and the free list, then flushes, polls and
//! dispatches, and does what the model answers. The compositor answers each armed frame once,
//! `ready` or `failed`, and the model arms the next only after that answer, so one frame is
//! out at a time and each buffer leaves the loop once.

use super::frame_loop::{Action, Event, Fault, FrameLoop};
use super::protocol::CaptureFrame;
use super::session::{drain, wait_readable};
use super::Opened;
use crate::linux::pipewire::realtime_minus_monotonic_ns;
use crate::linux::{FrameSlot, StreamShared};
use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest one poll waits. A quit writes no wakeup, so the loop samples it at least
/// this often; an idle desktop that delivers nothing only polls again.
const POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// The frame loop: arm a capture whenever none is outstanding and a buffer is free, and hand
/// each ready one to `slot`. Ends on `quit` or a stopped session; a failed frame, a resize
/// or a re-lit head ends it with an error, and the capture rebuilds.
pub(super) fn pump(
    o: Opened,
    slot: &FrameSlot,
    wake: &SyncSender<()>,
    shared: &StreamShared,
    quit: &AtomicBool,
    header_pts: bool,
) -> Result<()> {
    let mut sh = Shell {
        o,
        slot,
        wake,
        shared,
        header_pts,
        frame: None,
        delivered: 0,
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
    };
    let mut model = FrameLoop::Idle;
    let mut repaired = Instant::now();
    loop {
        // A returned buffer wakes the poll below, so a free one arms the moment the encoder
        // lets go.
        let top = if quit.load(Ordering::Relaxed) {
            Some(Event::Quit)
        } else if sh.o.st.stopped {
            Some(Event::Stopped)
        } else if model.idle() {
            sh.o.free.take().map(Event::BufferFree)
        } else {
            None
        };
        if let Some(event) = top {
            if !sh.act(model.step(event))? {
                break;
            }
        }
        if repaired.elapsed() >= Duration::from_secs(30) {
            sh.rt_minus_mono_ns = realtime_minus_monotonic_ns();
            repaired = Instant::now();
        }
        sh.o.conn.flush().context("wayland flush")?;
        wait_readable(&sh.o.conn, &sh.o.wake_r, POLL_SLICE)?;
        drain(&sh.o.wake_r);
        sh.o.queue
            .dispatch_pending(&mut sh.o.st)
            .context("dispatch")?;
        let events = std::mem::take(&mut sh.o.st.events);
        if !sh.act(model.dispatched(events))? {
            break;
        }
    }
    sh.o.session.destroy();
    sh.o.source.destroy();
    Ok(())
}

/// What the loop's actions work on.
struct Shell<'a> {
    o: Opened,
    slot: &'a FrameSlot,
    wake: &'a SyncSender<()>,
    shared: &'a StreamShared,
    header_pts: bool,
    /// The armed frame's proxy; the model holds its buffer.
    frame: Option<CaptureFrame>,
    delivered: u64,
    /// Clocks drift by microseconds over a long session, so the loop re-pairs this on the
    /// portal path's provenance cadence.
    rt_minus_mono_ns: i64,
}

impl Shell<'_> {
    /// Does `actions` in order. `false` once the loop stops.
    fn act(&mut self, actions: Vec<Action>) -> Result<bool> {
        for action in actions {
            match action {
                Action::Arm(idx) => {
                    let (w, h) = self.o.size;
                    let f = self.o.session.create_frame(&self.o.qh, ());
                    f.attach_buffer(&self.o.buffers[idx]);
                    f.damage_buffer(0, 0, w as i32, h as i32);
                    f.capture();
                    self.o.st.presented_ns = None;
                    self.frame = Some(f);
                }
                Action::Deliver { idx, presented } => self.deliver(idx, presented)?,
                Action::Return(idx) => {
                    if let Some(f) = self.frame.take() {
                        f.destroy();
                    }
                    self.o.free.put(idx);
                }
                Action::Fail(fault) => {
                    return Err(match fault {
                        Fault::Compositor(reason) => {
                            anyhow!("compositor failed the capture frame (reason {reason})")
                        }
                        Fault::Resized => {
                            anyhow!("capture source changed size — rebuilding the capture")
                        }
                        Fault::ColourChanged => {
                            anyhow!("output colour description changed — rebuilding the capture")
                        }
                    })
                }
                Action::Stop => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Stamps buffer `idx`, which the compositor just filled, then delivers it to the slot.
    fn deliver(&mut self, idx: usize, presented: Option<u64>) -> Result<()> {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        // `presentation_time` is CLOCK_MONOTONIC and the wire speaks realtime; `wire_pts`
        // rebases it, or falls back to the delivery stamp, as the portal path does for
        // `SPA_META_Header`. The stamp comes before `deliver`: its fence wait and import are
        // not part of the frame's age.
        let delivery_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let presented_ns = presented.filter(|_| self.header_pts);
        let pts_ns = crate::pts_provenance::wire_pts(
            presented_ns.map(|p| p as i64),
            delivery_ns,
            self.rt_minus_mono_ns,
        )
        .pts_ns;
        let captured = self.o.deliver(idx, pts_ns, self.shared)?;
        if let Ok(mut s) = self.slot.lock() {
            *s = Some(captured);
        }
        let _ = self.wake.try_send(());
        self.delivered += 1;
        if self.delivered == 1 {
            tracing::info!("direct wayland capture: first frame delivered");
        }
        Ok(())
    }
}
