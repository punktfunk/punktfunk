//! Request-driven paint pacing for a lazy-driver producer (Mutter ≥ 49 virtual monitors).

use super::consume::PTS_REPORT_EVERY;
use pipewire as pw;
use pw::spa;

/// Heartbeat period: a cycle out this long is lost and a driver that never started one
/// gets its first (Mutter's first paint follows a request only once a cycle has run).
pub(super) const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(250);

/// The least spacing between two paints: one wire interval, at `stream_hz` when the host
/// named it. Otherwise the configured multiplier is undone from the output's refresh — a
/// driven monitor never ticks on its own, so a multiplied refresh only inflates the mode
/// its clients see.
pub(super) fn wire_interval(
    preferred: Option<(u32, u32, u32)>,
    stream_hz: u32,
) -> std::time::Duration {
    let hz = if stream_hz > 0 {
        stream_hz
    } else {
        let hz = preferred.map(|(_, _, hz)| hz).unwrap_or(60).max(1);
        (hz / pf_host_config::config().vdisplay_hz_mult.max(1)).max(1)
    };
    std::time::Duration::from_nanos(1_000_000_000 / u64::from(hz))
}

/// One-shot loop timer the pacer re-arms from any of its entry points. Both pointers belong
/// to the loop thread and outlive the pacer's listeners.
#[derive(Clone, Copy)]
pub(super) struct RawTimer {
    pub(super) utils: *mut spa::sys::spa_loop_utils,
    pub(super) source: *mut spa::sys::spa_source,
}

impl RawTimer {
    fn arm(&self, after: std::time::Duration) {
        let value = spa::sys::timespec {
            tv_sec: after.as_secs() as _,
            tv_nsec: after.subsec_nanos() as _,
        };
        let interval = spa::sys::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `utils` is the loop's utils interface and `source` a live timer source of
        // that loop; `update_timer` reads the two timespecs for the duration of the call.
        unsafe {
            // The macro names the sys crate by this alias.
            use spa::sys as spa_sys;
            let mut iface = (*self.utils).iface;
            spa::spa_interface_call_method!(
                &mut iface as *mut spa::sys::spa_interface,
                spa::sys::spa_loop_utils_methods,
                update_timer,
                self.source,
                &value as *const _ as *mut _,
                &interval as *const _ as *mut _,
                false
            );
        }
    }
}

/// Request-driven paint pacing for a lazy driver.
///
/// The producer's frame clock is passive: it paints only inside a graph cycle this stream
/// starts, and asks for one (RequestProcess) when a client committed, a frame callback is
/// owed, or the pointer moved. Each request is answered with a cycle at once, or on the cap
/// timer when the last *paint* is less than a wire interval old. The cap counts paints, not
/// cycles: a cycle that only serves a frame callback or the cursor delivers no frame, and a
/// commit right behind it must not wait an interval for it. The producer's node is sync in
/// the graph, so its paint runs inside the cycle and this stream wakes with the frame the
/// same cycle. Every field is a `Cell`: `trigger_done` can land inside `schedule` when a
/// trigger has to close a cycle the producer never finished.
pub(super) struct Pacer {
    pub(super) stream: std::cell::Cell<*mut pw::sys::pw_stream>,
    interval: std::time::Duration,
    /// Streaming and driving: the only state a trigger starts a cycle in.
    live: std::cell::Cell<bool>,
    pub(super) timer: std::cell::Cell<Option<RawTimer>>,
    /// A request came since the last trigger.
    pending: std::cell::Cell<bool>,
    /// The cycle out now, if any: its trigger time.
    in_flight: std::cell::Cell<Option<std::time::Instant>>,
    last_trigger: std::cell::Cell<Option<std::time::Instant>>,
    /// Trigger time of the last cycle that delivered a frame: the cap's anchor.
    last_paint: std::cell::Cell<Option<std::time::Instant>>,
    requests: std::cell::Cell<u64>,
    triggers: std::cell::Cell<u64>,
    /// Requests that waited on the cap timer.
    deferred: std::cell::Cell<u64>,
    reported: std::cell::Cell<Option<std::time::Instant>>,
}

/// A cycle out longer than this is lost (the producer stalled or the stream re-linked).
const LOST_CYCLE: std::time::Duration = std::time::Duration::from_millis(100);

impl Pacer {
    pub(super) fn new(interval: std::time::Duration) -> std::rc::Rc<Pacer> {
        std::rc::Rc::new(Pacer {
            stream: std::cell::Cell::new(std::ptr::null_mut()),
            interval,
            live: std::cell::Cell::new(false),
            timer: std::cell::Cell::new(None),
            pending: std::cell::Cell::new(false),
            in_flight: std::cell::Cell::new(None),
            last_trigger: std::cell::Cell::new(None),
            last_paint: std::cell::Cell::new(None),
            requests: std::cell::Cell::new(0),
            triggers: std::cell::Cell::new(0),
            deferred: std::cell::Cell::new(0),
            reported: std::cell::Cell::new(None),
        })
    }

    fn on_request(&self) {
        self.requests.set(self.requests.get() + 1);
        self.pending.set(true);
        self.schedule();
    }

    /// Every edge into Streaming-and-driving starts a cycle at once: the producer's request
    /// gate stays latched across a renegotiation, and a trigger sent while paused is lost.
    pub(super) fn on_streaming(&self, live: bool) {
        self.live.set(live);
        self.in_flight.set(None);
        if live {
            self.pending.set(true);
            self.last_paint.set(None);
            self.schedule();
        }
    }

    fn on_done(&self) {
        self.in_flight.set(None);
        self.schedule();
    }

    /// A cycle delivered a frame (called from `.process`, before it is consumed). The
    /// anchor is that cycle's trigger, not the arrival: the next trigger then lands one
    /// interval after it and the paint period is the interval, not interval plus cycle.
    pub(super) fn on_paint(&self) {
        self.last_paint.set(Some(
            self.in_flight.get().unwrap_or_else(std::time::Instant::now),
        ));
    }

    /// Start a cycle if one is wanted and allowed; else arm the cap timer for the moment
    /// it is. Never two cycles out at once.
    pub(super) fn schedule(&self) {
        if !self.live.get() || !self.pending.get() || self.in_flight.get().is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(next) = self.last_paint.get().map(|t| t + self.interval) {
            if now < next {
                if let Some(timer) = self.timer.get() {
                    timer.arm(next - now);
                }
                self.deferred.set(self.deferred.get() + 1);
                return;
            }
        }
        self.pending.set(false);
        self.trigger(now);
    }

    fn trigger(&self, now: std::time::Instant) {
        // SAFETY: `stream` is this thread's live stream; the listeners that reach the pacer
        // are removed before it drops.
        let res = unsafe { pw::sys::pw_stream_trigger_process(self.stream.get()) };
        if res < 0 {
            // Not started (paused, unlinked): the request stays pending for the next edge.
            self.pending.set(true);
            return;
        }
        self.in_flight.set(Some(now));
        self.last_trigger.set(Some(now));
        self.triggers.set(self.triggers.get() + 1);
    }

    /// Every [`HEARTBEAT`]: retry a lost cycle (its request was never served), serve a
    /// request whose cap timer was missed, and report the tally.
    pub(super) fn heartbeat(&self) {
        let now = std::time::Instant::now();
        if self.in_flight.get().is_some_and(|t| now - t > LOST_CYCLE) {
            self.in_flight.set(None);
            self.pending.set(true);
        }
        self.schedule();
        if self
            .reported
            .get()
            .is_none_or(|t| now - t >= PTS_REPORT_EVERY)
        {
            if self.reported.get().is_some() {
                tracing::info!(
                    requests = self.requests.get(),
                    triggers = self.triggers.get(),
                    deferred = self.deferred.get(),
                    interval_us = self.interval.as_micros() as u64,
                    "lazy capture pacer: producer requests answered with cycles (deferred = held \
                     to the wire interval)"
                );
            }
            self.reported.set(Some(now));
        }
    }
}

/// The stream events pipewire-rs 0.9 has no builder for: RequestProcess and `trigger_done`.
/// Both callbacks run on the loop thread. Dropping removes the hook, before the pacer.
pub(super) struct RequestListener {
    hook: Box<spa::sys::spa_hook>,
    _events: Box<pw::sys::pw_stream_events>,
    _pacer: std::rc::Rc<Pacer>,
}

impl RequestListener {
    pub(super) fn attach(
        stream: &pw::stream::Stream,
        pacer: std::rc::Rc<Pacer>,
    ) -> RequestListener {
        unsafe extern "C" fn on_command(
            data: *mut std::ffi::c_void,
            command: *const spa::sys::spa_command,
        ) {
            // SAFETY: `data` is the `Rc<Pacer>` this listener holds; `command` is the live pod
            // PipeWire passes for the callback.
            unsafe {
                let body = (*command).body.body;
                if body.type_ == spa::sys::SPA_TYPE_COMMAND_Node
                    && body.id == spa::sys::SPA_NODE_COMMAND_RequestProcess
                {
                    (*(data as *const Pacer)).on_request();
                }
            }
        }
        unsafe extern "C" fn on_trigger_done(data: *mut std::ffi::c_void) {
            // SAFETY: as above.
            unsafe { (*(data as *const Pacer)).on_done() }
        }
        // SAFETY: zeroed is the C initialiser for both structs; every unset event stays
        // `None` and the hook's list link is filled in by `pw_stream_add_listener`.
        let (mut events, mut hook): (Box<pw::sys::pw_stream_events>, Box<spa::sys::spa_hook>) =
            unsafe { (Box::new(std::mem::zeroed()), Box::new(std::mem::zeroed())) };
        events.version = pw::sys::PW_VERSION_STREAM_EVENTS;
        events.command = Some(on_command);
        events.trigger_done = Some(on_trigger_done);
        // SAFETY: the hook and events boxes live as long as this listener, which is removed
        // in `Drop` before either is freed; `data` stays valid while `_pacer` is held.
        unsafe {
            pw::sys::pw_stream_add_listener(
                stream.as_raw_ptr(),
                &mut *hook,
                &*events,
                std::rc::Rc::as_ptr(&pacer) as *mut std::ffi::c_void,
            );
        }
        RequestListener {
            hook,
            _events: events,
            _pacer: pacer,
        }
    }
}

impl Drop for RequestListener {
    fn drop(&mut self) {
        // SAFETY: the hook was added by `attach` and not removed since.
        unsafe { spa::sys::spa_hook_remove(&mut *self.hook) }
    }
}
