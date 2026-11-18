//! Virtual mouse ("Punktfunk Mouse"): a uinput relative mouse. libinput reads it as it reads a
//! mouse plugged into the host, so the compositor's pointer speed, acceleration profile,
//! button and wheel settings apply to a client's mouse. Compositor protocols take finished
//! pointer deltas and apply none of them.
//!
//! Reports carry their capture time: libinput measures a mouse's speed from the time between
//! reports, and moves that arrive bunched otherwise read as a faster hand. A kernel before 6.5
//! stamps arrival. The name and `1209:504d` stay fixed for compositor rules.
//!
//! [`layered`] puts the mouse in front of a compositor injector. It takes what a mouse says:
//! moves that carry a mouse's own counts ([`InputEvent::mouse_counts_time`]), buttons, and a
//! wheel's turns. Finished pointer distances, absolute moves, a finger's scroll, keys and
//! touches stay with the compositor's protocol. Nothing reaches the mouse until the compositor
//! has said it reads uinput devices ([`crate::uinput_reach`]); gamescope and a seat never do.

use crate::plan::{mouse_op, MouseOp};
use crate::scroll::WheelAxis;
use crate::uinput_abi::{
    InputId, ReportClock, UinputDevice, EV_KEY, EV_REL, EV_SYN, SYN_REPORT, UI_SET_EVBIT,
    UI_SET_KEYBIT, UI_SET_RELBIT,
};
use crate::{Backend, InputInjector};
use anyhow::Result;
use punktfunk_core::input::InputEvent;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
/// `BTN_LEFT..=BTN_EXTRA`: every button `gs_button_to_evdev` names.
const BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x114;

/// A uinput mouse. It lives as long as the injector that made it.
pub struct VirtualMouse {
    dev: UinputDevice,
    clock: ReportClock,
    /// Vertical, horizontal.
    wheel: [WheelAxis; 2],
}

impl VirtualMouse {
    pub fn create() -> Result<VirtualMouse> {
        let dev = UinputDevice::open()?;
        dev.set_bits(UI_SET_EVBIT, "UI_SET_EVBIT", &[EV_KEY, EV_REL])?;
        dev.set_bits(UI_SET_KEYBIT, "UI_SET_KEYBIT", &BUTTONS.collect::<Vec<_>>())?;
        let axes = [
            REL_X,
            REL_Y,
            REL_WHEEL,
            REL_HWHEEL,
            REL_WHEEL_HI_RES,
            REL_HWHEEL_HI_RES,
        ];
        dev.set_bits(UI_SET_RELBIT, "UI_SET_RELBIT", &axes)?;
        // BUS_VIRTUAL: libinput turns a touchpad off beside a USB or Bluetooth mouse where the
        // host says so, and this one is there with no client.
        let id = InputId {
            bustype: 0x0006,
            vendor: 0x1209,
            product: 0x504d, // "PM"
            version: 1,
        };
        dev.create(id, b"Punktfunk Mouse", 0)?;
        tracing::info!("virtual mouse created (Punktfunk Mouse, uinput)");
        Ok(VirtualMouse {
            dev,
            clock: ReportClock::default(),
            wheel: Default::default(),
        })
    }

    /// The time of a report made now: `scan_time` on the host's clock where the client gave
    /// one, else now. Never earlier than the report before.
    fn time(&mut self, scan_time: Option<u16>) -> u64 {
        let now = Instant::now();
        let now_us = crate::monotonic_us();
        match scan_time {
            Some(scan) => self.clock.stamp(scan, now, now_us),
            None => self.clock.next(now_us),
        }
    }

    /// One report of `dx`, `dy` counts, captured at `scan_time` (100 µs units, wrapping).
    pub fn motion(&mut self, dx: i32, dy: i32, scan_time: Option<u16>) {
        if dx == 0 && dy == 0 {
            return;
        }
        let t = self.time(scan_time);
        for (code, v) in [(REL_X, dx), (REL_Y, dy)] {
            if v != 0 {
                self.dev.emit_at(t, EV_REL, code, v);
            }
        }
        self.dev.emit_at(t, EV_SYN, SYN_REPORT, 0);
    }

    /// `evdev` is a `BTN_*` code out of [`BUTTONS`]; the kernel drops any other.
    pub fn button(&mut self, evdev: u16, down: bool) {
        let t = self.time(None);
        self.dev.emit_at(t, EV_KEY, evdev, down.into());
        self.dev.emit_at(t, EV_SYN, SYN_REPORT, 0);
    }

    /// A wheel turn of `q8`, Q24.8 in 120ths of a detent, positive up and right.
    pub fn wheel(&mut self, horizontal: bool, q8: i32) {
        let Some((hi, detents)) = self.wheel[usize::from(horizontal)].turn(q8) else {
            return;
        };
        let (hi_code, code) = if horizontal {
            (REL_HWHEEL_HI_RES, REL_HWHEEL)
        } else {
            (REL_WHEEL_HI_RES, REL_WHEEL)
        };
        let t = self.time(None);
        if detents != 0 {
            self.dev.emit_at(t, EV_REL, code, detents);
        }
        self.dev.emit_at(t, EV_REL, hi_code, hi);
        self.dev.emit_at(t, EV_SYN, SYN_REPORT, 0);
    }
}

const UINPUT: &str = "/dev/uinput";

/// The compositor's answer, as the probe thread leaves it.
const ASKING: u8 = 0;
const READS: u8 = 1;
const READS_NOT: u8 = 2;

/// After the compositor names the probe tablet, before the mouse takes input: the mouse was
/// made first, and libinput adds devices in order.
const SETTLE: Duration = Duration::from_millis(200);

/// `inner` with a [`VirtualMouse`] in front, where this host can have one: `/dev/uinput`
/// opens, the node is not a seat's proxy, and the backend is not gamescope, which gives a
/// plugged-in mouse the same unaccelerated motion as its EIS. `PUNKTFUNK_MOUSE=0` keeps the
/// mouse off; `1` uses it without asking the compositor, for one that reads uinput devices
/// and cannot say so.
pub(crate) fn layered(inner: Box<dyn InputInjector>, backend: Backend) -> Box<dyn InputInjector> {
    let knob = pf_host_config::env_on("PUNKTFUNK_MOUSE");
    let wanted = backend != Backend::GamescopeEi
        && knob != Some(false)
        && crate::uapi::open_nonblock(UINPUT).is_ok()
        && !crate::pad_broker::proxied(std::path::Path::new(UINPUT));
    if !wanted {
        return inner;
    }
    let mouse = match VirtualMouse::create() {
        Ok(mouse) => mouse,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "virtual mouse not created");
            return inner;
        }
    };
    let reads = Arc::new(AtomicU8::new(ASKING));
    let answer = reads.clone();
    let asked = std::thread::Builder::new()
        .name("punktfunk-uinput-reach".into())
        .spawn(move || {
            let said = knob == Some(true) || crate::uinput_reach::reaches();
            if said {
                std::thread::sleep(SETTLE);
            }
            answer.store(if said { READS } else { READS_NOT }, Ordering::Release);
        });
    if asked.is_err() {
        return inner;
    }
    Box::new(MouseLayer {
        inner,
        mouse: Some(mouse),
        reads,
        held: 0,
    })
}

/// A compositor injector with the mouse in front of it.
struct MouseLayer {
    inner: Box<dyn InputInjector>,
    /// Dropped once the compositor says it reads no uinput device.
    mouse: Option<VirtualMouse>,
    reads: Arc<AtomicU8>,
    /// Wire buttons the mouse holds down, by bit: their releases are its own.
    held: u8,
}

impl MouseLayer {
    /// Put `ev` on the mouse. `false` leaves it to the compositor's protocol.
    fn take(&mut self, ev: &InputEvent) -> bool {
        let reads = self.reads.load(Ordering::Acquire);
        if reads == READS_NOT {
            self.mouse = None;
        }
        let Some(mouse) = self.mouse.as_mut() else {
            return false;
        };
        match mouse_op(ev, reads == READS, &mut self.held) {
            Some(MouseOp::Motion { dx, dy, scan }) => mouse.motion(dx, dy, Some(scan)),
            Some(MouseOp::Button { evdev, down }) => mouse.button(evdev, down),
            Some(MouseOp::Wheel { horizontal, q8 }) => mouse.wheel(horizontal, q8),
            None => return false,
        }
        true
    }
}

impl InputInjector for MouseLayer {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        if self.take(event) {
            return Ok(());
        }
        self.inner.inject(event)
    }

    fn deadline(&self) -> Option<Instant> {
        self.inner.deadline()
    }

    fn on_deadline(&mut self) -> Result<()> {
        self.inner.on_deadline()
    }

    fn motion_reports(&self) -> usize {
        self.inner.motion_reports()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::InputKind;
    use std::thread::sleep;

    /// Sleeps most of the way, then spins: arrivals 0.2 ms apart need better than `sleep`.
    fn wait_until(deadline: Instant) {
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if left > Duration::from_micros(300) {
                sleep(left - Duration::from_micros(200));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    fn scan(us: u64) -> u16 {
        punktfunk_core::quic::touchpad_scan_time(us)
    }

    /// A mouse on a real device, each step named on stderr. Attach in the first five seconds:
    /// `libinput debug-events --device /dev/input/eventN`.
    struct Player {
        mouse: VirtualMouse,
        t0: Instant,
    }

    impl Player {
        fn new() -> Player {
            let mouse = VirtualMouse::create().expect("create the uinput mouse");
            let name = mouse.dev.sysname().unwrap_or_default();
            eprintln!("created {name}; attach now");
            sleep(Duration::from_secs(5));
            Player {
                mouse,
                t0: Instant::now(),
            }
        }

        fn step(&self, name: &str) {
            sleep(Duration::from_millis(1200));
            eprintln!("{:8.3} s  {name}", self.t0.elapsed().as_secs_f32());
        }

        /// `reports` of `counts` each, one per `period_us`, delivered `group` at a time `gap_us`
        /// apart: a group arrives when its last report was made. `timed` stamps capture time.
        fn stroke(
            &mut self,
            reports: u64,
            counts: i32,
            period_us: u64,
            (group, gap_us): (u64, u64),
            timed: bool,
        ) {
            let (start, base) = (Instant::now(), self.t0.elapsed().as_micros() as u64);
            for i in 0..reports {
                let last = ((i / group + 1) * group - 1).min(reports - 1);
                let arrival = last * period_us + (i % group) * gap_us;
                wait_until(start + Duration::from_micros(arrival));
                let at = timed.then(|| scan(base + i * period_us));
                self.mouse.motion(counts, 0, at);
            }
        }
    }

    const EVEN: (u64, u64) = (1, 0);

    /// What a plugged-in mouse gets: the same 2000 counts as a slow, a medium and a fast
    /// stroke, at 1000 Hz and at 125 Hz. libinput prints the pointer's travel and the counts.
    #[test]
    #[ignore = "needs /dev/uinput; plays strokes for libinput debug-events"]
    fn a_real_mouse_plays_strokes_at_three_speeds() {
        let mut p = Player::new();
        for (label, reports, counts, period) in [
            ("1000 Hz, 1 count a report (1000 counts/s)", 2000, 1, 1000),
            ("1000 Hz, 4 counts a report (4000 counts/s)", 500, 4, 1000),
            (
                "1000 Hz, 16 counts a report (16000 counts/s)",
                125,
                16,
                1000,
            ),
            ("125 Hz, 8 counts a report (1000 counts/s)", 250, 8, 8000),
            ("125 Hz, 32 counts a report (4000 counts/s)", 63, 32, 8000),
            (
                "125 Hz, 128 counts a report (16000 counts/s)",
                16,
                128,
                8000,
            ),
        ] {
            p.step(label);
            p.stroke(reports, counts, period, EVEN, true);
        }
        sleep(Duration::from_secs(1));
    }

    /// One-count reports at 300 and at 800 counts/s, below and on the slope of libinput's
    /// default curve, delivered evenly, in pairs, in bursts of four and of sixteen, first
    /// stamped at arrival and then with capture time. libinput should move the pointer equally
    /// far however a stroke arrived.
    #[test]
    #[ignore = "needs /dev/uinput; plays bunched strokes for libinput debug-events"]
    fn a_real_mouse_keeps_bunched_strokes_whole() {
        let mut p = Player::new();
        for (rate, period, reports) in [(300, 3333, 450), (800, 1250, 1200)] {
            for timed in [false, true] {
                for (label, bunch) in [
                    ("evenly", EVEN),
                    ("in pairs 0.2 ms apart", (2, 200)),
                    ("in bursts of four", (4, 0)),
                    ("in bursts of sixteen", (16, 0)),
                ] {
                    let stamp = if timed { "capture" } else { "arrival" };
                    p.step(&format!("{rate} counts/s, {stamp} time, {label}"));
                    p.stroke(reports, 1, period, bunch, timed);
                }
            }
        }
        sleep(Duration::from_secs(1));
    }

    /// For a client under the pointer on a real desktop: the pointer goes to the top-left
    /// corner, 600 px in, then 300 counts right and left at three speeds. The client shows how
    /// far each stroke moved it under the desktop's own mouse settings.
    #[test]
    #[ignore = "needs /dev/uinput and a desktop session; moves its pointer"]
    fn a_real_mouse_strokes_back_and_forth() {
        let mut p = Player::new();
        p.step("to the corner, then 600 counts in");
        p.mouse.motion(-8000, -8000, None);
        sleep(Duration::from_millis(300));
        for _ in 0..600 {
            p.mouse.motion(1, 1, None);
            sleep(Duration::from_millis(4));
        }
        for (rate, period) in [(300, 3333), (1000, 1000), (4000, 250)] {
            for (way, counts) in [("right", 1), ("left", -1)] {
                p.step(&format!("300 counts {way} at {rate} counts/s"));
                p.stroke(300, counts, period, EVEN, true);
            }
        }
        sleep(Duration::from_secs(1));
    }

    fn event(kind: InputKind, code: u32, x: i32, flags: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags,
        }
    }

    /// The production injector for this session's compositor, fed what a session sends. A
    /// Wayland client under the pointer shows where each stroke went: the compositor's own
    /// speed and curve on a mouse's counts where it reads uinput, one pixel a unit everywhere
    /// else.
    #[test]
    #[ignore = "needs /dev/uinput and a compositor"]
    fn a_real_compositor_gets_the_mouse_where_it_reads_uinput() {
        use punktfunk_core::input::mouse_counts_flags;
        let mut inj = crate::open(crate::default_backend()).expect("open the injector");
        let t0 = Instant::now();
        let step = |name: &str| {
            sleep(Duration::from_millis(1200));
            eprintln!("{:8.3} s  {name}", t0.elapsed().as_secs_f32());
        };
        let centre = event(InputKind::MouseMoveAbs, 0, 960, (1920 << 16) | 1080);
        inj.inject(&InputEvent { y: 540, ..centre }).unwrap();
        sleep(Duration::from_secs(3));

        let stroke = |inj: &mut Box<dyn InputInjector>, dx: i32, period_us: u64, counts| {
            let start = Instant::now();
            for i in 0..300u64 {
                wait_until(start + Duration::from_micros(i * period_us));
                let flags = if counts {
                    mouse_counts_flags(t0.elapsed().as_micros() as u64)
                } else {
                    0
                };
                inj.inject(&event(InputKind::MouseMove, 0, dx, flags))
                    .unwrap();
            }
        };
        step("finished distance, 300 units right");
        stroke(&mut inj, 1, 1000, false);
        step("finished distance, 300 units left");
        stroke(&mut inj, -1, 1000, false);
        step("a mouse's counts, 300 right at 300 counts/s");
        stroke(&mut inj, 1, 3333, true);
        step("a mouse's counts, 300 left at 300 counts/s");
        stroke(&mut inj, -1, 3333, true);
        step("a mouse's counts, 300 right at 1000 counts/s");
        stroke(&mut inj, 1, 1000, true);
        step("a mouse's counts, 300 left at 4000 counts/s");
        stroke(&mut inj, -1, 250, true);

        step("left click, one wheel detent down, a finger's scroll");
        for kind in [InputKind::MouseButtonDown, InputKind::MouseButtonUp] {
            inj.inject(&event(kind, 1, 0, 0)).unwrap();
            sleep(Duration::from_millis(50));
        }
        inj.inject(&event(InputKind::MouseScroll, 0, -120, 0))
            .unwrap();
        sleep(Duration::from_millis(100));
        // Source finger (2), phase update (2): stays on the compositor's protocol.
        inj.inject(&event(InputKind::Scroll, 0, -20 * 256, 2 | (2 << 8)))
            .unwrap();
        sleep(Duration::from_secs(1));
    }

    #[test]
    #[ignore = "needs /dev/uinput; plays buttons and wheels for libinput debug-events"]
    fn a_real_mouse_clicks_and_turns_its_wheels() {
        let mut p = Player::new();
        p.step("left, right, middle, side, extra");
        for evdev in BUTTONS {
            p.mouse.button(evdev, true);
            sleep(Duration::from_millis(40));
            p.mouse.button(evdev, false);
            sleep(Duration::from_millis(40));
        }
        p.step("one detent up, one down");
        p.mouse.wheel(false, 120 * 256);
        sleep(Duration::from_millis(100));
        p.mouse.wheel(false, -120 * 256);
        p.step("four quarter detents up");
        for _ in 0..4 {
            p.mouse.wheel(false, 30 * 256);
            sleep(Duration::from_millis(20));
        }
        p.step("one detent right");
        p.mouse.wheel(true, 120 * 256);
        sleep(Duration::from_secs(1));
    }
}
