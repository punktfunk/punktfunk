//! Bounded Wayland event loop for the in-process protocol clients (KWin
//! screencast, output management, window list, panel DPMS).
//!
//! `blocking_dispatch` and `roundtrip` cannot be interrupted and have no
//! ceiling, so a compositor that accepts the connection and then stops serving
//! would pin the calling thread. [`pump_until`] polls the fd in [`POLL_MS`]
//! slices instead.

use anyhow::{Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::{Connection, Dispatch, EventQueue};

/// Poll slice: the granularity at which `stop` and the deadline are observed.
const POLL_MS: i32 = 100;

pub(crate) enum Pumped {
    Done,
    /// `stop` was set while we waited.
    Stopped,
    Expired,
}

/// State that records the highest `wl_display.sync` serial answered.
pub(crate) trait SyncDone {
    fn sync_done(&self) -> u32;
}

/// Dispatch, poll the connection fd, read, until `done`, `stop` or `deadline`.
/// `deadline: None` waits for `done` or `stop` alone. A dispatch or flush error
/// is the connection failing.
pub(crate) fn pump_until<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Option<Instant>,
    stop: Option<&AtomicBool>,
    done: impl Fn(&S) -> bool,
) -> Result<Pumped> {
    if done(state) {
        return Ok(Pumped::Done);
    }
    loop {
        queue.dispatch_pending(state).context("dispatch_pending")?;
        if done(state) {
            return Ok(Pumped::Done);
        }
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            return Ok(Pumped::Stopped);
        }
        let timeout = match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(Pumped::Expired);
                }
                (remaining.as_millis() as i64).clamp(0, i64::from(POLL_MS)) as i32
            }
            None => POLL_MS,
        };
        conn.flush().context("wayland flush")?;
        let Some(guard) = conn.prepare_read() else {
            continue; // events already queued — the loop dispatches them
        };
        let mut pfd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `&mut pfd` is one live, initialized `libc::pollfd` on the stack and the count is 1,
        // so `poll` reads `fd`/`events` and writes only `revents` within it. `pfd.fd` is the
        // connection's fd, valid while `conn` and the `prepare_read` guard live across the call.
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = guard.read();
        } // else: timeout or signal — drop the guard, re-check `stop` and the deadline
    }
}

/// A `wl_display.sync` barrier: [`Pumped::Done`] once every event sent before
/// it has been dispatched. `serial` must be unique per connection.
pub(crate) fn sync_barrier<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    serial: u32,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> Result<Pumped>
where
    S: SyncDone + Dispatch<WlCallback, u32> + 'static,
{
    let _cb = conn.display().sync(&queue.handle(), serial);
    pump_until(conn, queue, state, Some(deadline), stop, |st| {
        st.sync_done() >= serial
    })
}
