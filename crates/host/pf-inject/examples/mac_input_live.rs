//! Live check of the CGEvent injector against an event sink: drives `pf_inject::open` with wire
//! events and prints each one beside what the sink logs. The sink is `mac_input_sink.swift`
//! beside this file (`swiftc -O -o mac_input_sink mac_input_sink.swift`); its header lists the
//! lines this reads.
//!
//! A run starts once the desk has had no input for 4 s, and stops at 10 s, on foreign input, or
//! as soon as the sink is not both the key window and topmost. It ends with what it holds
//! released and the pointer where it started. Posting needs the Accessibility grant for the app
//! that launches this (the terminal); without it, this asks once and exits.
//!
//! Usage: `mac_input_live <sink binary> <scroll|motion|keys|repeat|buttons|pen|touch>...`, in
//! one run. `pen` drives the stylus the host's pen plane drives, through `PenTracker`.

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_input_live runs on macOS");
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{
        CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType,
        CGWarpMouseCursorPosition,
    };
    use pf_inject::{Backend, InputInjector};
    use punktfunk_core::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    const LIMIT: Duration = Duration::from_secs(10);

    type Rect = (f64, f64, f64, f64);

    pub fn main() {
        let mut args = std::env::args().skip(1);
        let (Some(sink), scenarios) = (args.next(), args.collect::<Vec<_>>()) else {
            eprintln!(
                "usage: mac_input_live <sink binary> <scroll|motion|keys|repeat|buttons|pen|touch>..."
            );
            std::process::exit(2);
        };
        if !pf_inject::post_event_access() {
            let granted = pf_inject::request_post_event_access();
            println!("accessibility: missing; asked once, granted now: {granted}");
            return;
        }
        if !wait_for_idle() {
            println!("abort: the desk never went 4 s without input");
            return;
        }
        let mut child = Command::new(&sink)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the sink");
        let ok = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<String>();
        let stdout = child.stdout.take().expect("sink stdout");
        let reader_ok = ok.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                println!("  sink {line}");
                if line.contains("STATE ") || line.contains("READY ") {
                    let good = line.contains("key=1") && line.contains("front=1");
                    reader_ok.store(good, Ordering::SeqCst);
                }
                if line.contains("FOREIGN ") {
                    reader_ok.store(false, Ordering::SeqCst);
                }
                let _ = tx.send(line);
            }
        });
        let until = Instant::now() + Duration::from_secs(5);
        let ready = loop {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(line) if line.contains("READY ") => break Some(line),
                Ok(_) => continue,
                Err(_) => break None,
            }
        };
        let Some(ready) = ready else {
            println!("abort: the sink never became ready");
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let field = |name: &str| {
            let key = format!("{name}=");
            ready
                .split_whitespace()
                .find_map(|t| t.strip_prefix(key.as_str()).map(str::to_owned))
                .unwrap_or_default()
        };
        let display: u32 = field("display").parse().unwrap_or(0);
        let nums: Vec<f64> = field("scroll")
            .split(',')
            .filter_map(|v| v.parse().ok())
            .collect();
        let [sx, sy, sw, sh] = nums[..] else {
            println!("abort: no scroll frame in {ready}");
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let b = CGDisplayBounds(display);
        let bounds = (b.origin.x, b.origin.y, b.size.width, b.size.height);
        let home = cursor();
        println!("start: cursor {home:?}, display {display} bounds {bounds:?}");
        pf_inject::set_stream_display(Some(display));
        let inj = pf_inject::open(Backend::CgEvent).expect("open the CGEvent injector");
        let mut run = Run {
            inj,
            ok,
            start: Instant::now(),
            bounds,
            aborted: false,
        };
        for scenario in &scenarios {
            match scenario.as_str() {
                "scroll" => scroll_run(&mut run, (sx, sy, sw, sh)),
                "motion" => motion_run(&mut run, (sx, sy, sw, sh)),
                "keys" => keys_run(&mut run),
                "repeat" => repeat_run(&mut run),
                "buttons" => buttons_run(&mut run, (sx, sy, sw, sh)),
                "pen" => pen_run(&mut run, (sx, sy, sw, sh)),
                "touch" => touch_run(&mut run, (sx, sy, sw, sh)),
                other => println!("unknown scenario {other}"),
            }
        }
        let posted = run.start.elapsed();
        drop(run);
        let _ = CGWarpMouseCursorPosition(CGPoint::new(home.0, home.1));
        let caps = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState)
            .contains(CGEventFlags::MaskAlphaShift);
        println!(
            "end: posted for {posted:?}, cursor back at {:?}, caps lock {caps}",
            cursor()
        );
        std::thread::sleep(Duration::from_millis(300));
        drop(child.stdin.take());
        let _ = child.wait();
        let _ = reader.join();
    }

    /// Whether the desk goes 4 s without hardware input within the next 60 s.
    fn wait_for_idle() -> bool {
        let any_input = CGEventType(u32::MAX);
        let until = Instant::now() + Duration::from_secs(60);
        while Instant::now() < until {
            let idle = CGEventSource::seconds_since_last_event_type(
                CGEventSourceStateID::HIDSystemState,
                any_input,
            );
            if idle >= 4.0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn cursor() -> (f64, f64) {
        let at = CGEvent::new(None).map_or(CGPoint::default(), |e| CGEvent::location(Some(&e)));
        (at.x, at.y)
    }

    fn ev(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y,
            flags,
        }
    }

    fn wheel(source: ScrollSource, phase: ScrollPhase, axis: u32, delta: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis,
            delta: (delta * SCROLL_SCALE) as i32,
            axis_units: false,
        }
        .to_event()
    }

    fn centre(r: Rect) -> (f64, f64) {
        ((r.0 + r.2 / 2.0).round(), (r.1 + r.3 / 2.0).round())
    }

    struct Run {
        inj: Box<dyn InputInjector>,
        ok: Arc<AtomicBool>,
        start: Instant,
        bounds: Rect,
        aborted: bool,
    }

    impl Run {
        /// Inject `e` if the run may still post; `loud` prints it.
        fn post(&mut self, e: InputEvent, loud: bool) {
            if self.aborted {
                return;
            }
            if self.start.elapsed() > LIMIT || !self.ok.load(Ordering::SeqCst) {
                println!("abort: time is up, or the sink is not key and topmost");
                self.aborted = true;
                return;
            }
            if loud {
                println!(
                    "send {:?} code={:#x} x={} y={} flags={:#x}",
                    e.kind, e.code, e.x, e.y, e.flags
                );
            }
            if let Err(err) = self.inj.inject(&e) {
                println!("abort: inject {err:#}");
                self.aborted = true;
            }
        }

        fn send(&mut self, e: InputEvent) {
            self.post(e, true);
        }

        /// Sleep, running the injector's deadline as the service does.
        fn wait(&mut self, ms: u64) {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
                if self.inj.deadline().is_some_and(|d| Instant::now() >= d) {
                    println!("send on_deadline");
                    let _ = self.inj.on_deadline();
                }
            }
        }

        /// An absolute move to global point `p`, in the stream display's own extent.
        fn abs(&mut self, (x, y): (f64, f64)) {
            let (bx, by, bw, bh) = self.bounds;
            let flags = ((bw as u32) << 16) | bh as u32;
            let e = ev(
                InputKind::MouseMoveAbs,
                0,
                (x - bx).round() as i32,
                (y - by).round() as i32,
                flags,
            );
            self.send(e);
        }

        fn step(&mut self, name: &str) {
            println!("step {name}");
        }

        /// Whether the run may still post, as [`Self::post`] checks.
        fn may_post(&mut self) -> bool {
            if !self.aborted && (self.start.elapsed() > LIMIT || !self.ok.load(Ordering::SeqCst)) {
                println!("abort: time is up, or the sink is not key and topmost");
                self.aborted = true;
            }
            !self.aborted
        }

        /// A touch at global point `p`, in the stream display's own extent.
        fn touch(&mut self, kind: InputKind, id: u32, (x, y): (f64, f64)) {
            let (bx, by, bw, bh) = self.bounds;
            let flags = ((bw as u32) << 16) | bh as u32;
            let e = ev(
                kind,
                id,
                (x - bx).round() as i32,
                (y - by).round() as i32,
                flags,
            );
            self.send(e);
        }

        /// A finger from `a` to `b` in `n` moves `gap_ms` apart.
        fn drag(&mut self, id: u32, a: (f64, f64), b: (f64, f64), n: u32, gap_ms: u64) {
            for i in 1..=n {
                let k = f64::from(i) / f64::from(n);
                self.wait(gap_ms);
                let p = (a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k);
                self.touch(InputKind::TouchMove, id, p);
            }
        }

        fn key(&mut self, vk: u32, down: bool) {
            let kind = if down {
                InputKind::KeyDown
            } else {
                InputKind::KeyUp
            };
            self.send(ev(kind, vk, 0, 0, 0));
        }

        fn tap(&mut self, vk: u32) {
            self.key(vk, true);
            self.wait(30);
            self.key(vk, false);
        }

        fn button(&mut self, code: u32, down: bool) {
            let kind = if down {
                InputKind::MouseButtonDown
            } else {
                InputKind::MouseButtonUp
            };
            self.send(ev(kind, code, 0, 0, 0));
        }

        /// `n` relative moves of `(dx, dy)`, `gap_ms` apart, printed as one line.
        fn burst(&mut self, n: u32, (dx, dy): (i32, i32), gap_ms: u64) {
            println!("send {n} x MouseMove ({dx},{dy}) every {gap_ms} ms");
            for _ in 0..n {
                self.post(ev(InputKind::MouseMove, 0, dx, dy, 0), false);
                if gap_ms > 0 {
                    std::thread::sleep(Duration::from_millis(gap_ms));
                }
            }
        }

        fn expect_cursor(&mut self, want: (f64, f64)) {
            self.wait(150);
            println!("check cursor {:?}, want {want:?}", cursor());
        }
    }

    fn scroll_run(r: &mut Run, view: Rect) {
        use ScrollPhase::*;
        use ScrollSource::{Finger, Wheel};
        r.abs(centre(view));
        r.wait(150);
        for (name, axis, v120) in [
            ("wheel up", 0, 120.0),
            ("wheel down", 0, -120.0),
            ("wheel right", 1, 120.0),
            ("wheel left", 1, -120.0),
        ] {
            r.step(name);
            for _ in 0..3 {
                r.send(wheel(Wheel, None, axis, v120));
                r.wait(100);
            }
        }
        for (name, axis, dip) in [("finger up", 0, 6.0), ("finger right", 1, 6.0)] {
            r.step(name);
            r.send(wheel(Finger, Begin, axis, dip));
            for _ in 0..9 {
                r.wait(16);
                r.send(wheel(Finger, Update, axis, dip));
            }
            r.send(wheel(Finger, End, axis, 0.0));
            r.wait(150);
        }
        r.step("finger down, then momentum");
        r.send(wheel(Finger, Begin, 0, -6.0));
        for _ in 0..5 {
            r.wait(16);
            r.send(wheel(Finger, Update, 0, -6.0));
        }
        r.send(wheel(Finger, End, 0, 0.0));
        r.wait(16);
        r.send(wheel(Finger, MomentumBegin, 0, -5.0));
        for d in [-4.0, -3.0, -2.0, -1.0] {
            r.wait(16);
            r.send(wheel(Finger, Momentum, 0, d));
        }
        r.send(wheel(Finger, MomentumEnd, 0, 0.0));
        r.wait(150);
    }

    fn motion_run(r: &mut Run, view: Rect) {
        let (cx, cy) = centre(view);
        r.step("absolute to the scroll view's centre");
        r.abs((cx, cy));
        r.expect_cursor((cx, cy));
        r.step("100 relative moves 1 ms apart");
        r.burst(100, (3, 0), 1);
        r.expect_cursor((cx + 300.0, cy));
        r.burst(100, (-3, 1), 1);
        r.expect_cursor((cx, cy + 100.0));
        r.step("50 relative moves back to back");
        r.burst(50, (0, -2), 0);
        r.expect_cursor((cx, cy));
        let (x0, y0, x1, y1) = (
            view.0 + 40.0,
            view.1 + 40.0,
            view.0 + view.2 - 40.0,
            view.1 + view.3 - 40.0,
        );
        for p in [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (cx, cy)] {
            r.step("absolute");
            r.abs(p);
            r.expect_cursor(p);
        }
    }

    const VK_A: u32 = 0x41;
    const VK_B: u32 = 0x42;
    const VK_C: u32 = 0x43;
    const VK_D: u32 = 0x44;
    const VK_SPACE: u32 = 0x20;
    const VK_CAPITAL: u32 = 0x14;
    const VK_LSHIFT: u32 = 0xA0;
    const VK_RSHIFT: u32 = 0xA1;

    fn keys_run(r: &mut Run) {
        r.step("shift+a");
        r.key(VK_LSHIFT, true);
        r.tap(VK_A);
        r.key(VK_LSHIFT, false);
        r.step("b with four client repeats");
        r.key(VK_B, true);
        for _ in 0..4 {
            r.wait(40);
            r.key(VK_B, true);
        }
        r.wait(40);
        r.key(VK_B, false);
        r.step("c held 500 ms with no repeats");
        r.key(VK_C, true);
        r.wait(500);
        r.key(VK_C, false);
        r.tap(VK_SPACE);
        r.step("committed text");
        for ch in ['é', '日', '😀'] {
            r.send(ev(InputKind::TextInput, u32::from(ch), 0, 0, 0));
            r.wait(30);
        }
        r.step("left and right shift");
        r.key(VK_LSHIFT, true);
        r.key(VK_RSHIFT, true);
        r.key(VK_RSHIFT, false);
        r.key(VK_LSHIFT, false);
        // Taps of different modifiers, far enough apart not to read as a double press.
        for (name, vk) in [
            ("left control", 0xA2),
            ("right control", 0xA3),
            ("left option", 0xA4),
            ("right option", 0xA5),
            ("left command", 0x5B),
            ("right command", 0x5C),
        ] {
            r.wait(300);
            r.step(name);
            r.tap(vk);
        }
        r.wait(300);
        r.step("caps lock on, d, caps lock off, d");
        r.tap(VK_CAPITAL);
        r.tap(VK_D);
        r.tap(VK_CAPITAL);
        r.tap(VK_D);
        r.wait(100);
    }

    /// Held keys with the client's repeats: a number key, which press-and-hold may swallow, and
    /// Backspace, which it never does.
    fn repeat_run(r: &mut Run) {
        for (name, vk) in [("1", 0x31), ("2345 then backspace", 0x08)] {
            r.step(&format!("{name} with three client repeats"));
            if vk == 0x08 {
                for vk in [0x32, 0x33, 0x34, 0x35] {
                    r.tap(vk);
                }
            }
            r.key(vk, true);
            for _ in 0..3 {
                r.wait(40);
                r.key(vk, true);
            }
            r.wait(40);
            r.key(vk, false);
            r.wait(100);
        }
    }

    fn buttons_run(r: &mut Run, view: Rect) {
        let c = centre(view);
        r.abs(c);
        r.wait(150);
        r.step("double-click");
        for _ in 0..2 {
            r.button(1, true);
            r.wait(40);
            r.button(1, false);
            r.wait(60);
        }
        r.wait(600);
        for (name, code, d) in [
            ("left drag", 1, (5, 0)),
            ("right drag", 3, (0, 5)),
            ("middle drag", 2, (-5, 0)),
        ] {
            r.step(name);
            r.button(code, true);
            for _ in 0..10 {
                r.wait(15);
                r.send(ev(InputKind::MouseMove, 0, d.0, d.1, 0));
            }
            r.button(code, false);
            r.wait(100);
        }
        r.step("back and forward buttons");
        for code in [4, 5] {
            r.button(code, true);
            r.wait(30);
            r.button(code, false);
        }
        r.abs(c);
        r.wait(100);
    }

    fn pen_run(r: &mut Run, view: Rect) {
        use punktfunk_core::quic::{
            PenBatch, PenSample, PenTool, PenTracker, PEN_BARREL1, PEN_IN_RANGE, PEN_TOUCHING,
        };
        let mut pen = pf_inject::pen::VirtualPen::create().expect("create the pen");
        let mut tracker = PenTracker::default();
        let mut seq = 0u16;
        let (bx, by, bw, bh) = r.bounds;
        let (cx, cy) = centre(view);
        let mut send = |r: &mut Run, s: PenSample| {
            if !r.may_post() {
                return;
            }
            let mut t = Vec::new();
            tracker.apply(&PenBatch::new(seq, &[s]), &mut t);
            seq = seq.wrapping_add(1);
            println!(
                "send pen state={:#x} tool={:?} at={:.3},{:.3} p={} tilt={} az={} roll={} -> {t:?}",
                s.state, s.tool, s.x, s.y, s.pressure, s.tilt_deg, s.azimuth_deg, s.roll_deg
            );
            pen.apply_batch(&t);
            r.wait(16);
        };
        let at = |dx: f64, dy: f64| (((cx + dx - bx) / bw) as f32, ((cy + dy - by) / bh) as f32);
        let sample = |state, (x, y): (f32, f32), pressure| PenSample {
            state,
            x,
            y,
            pressure,
            ..Default::default()
        };
        let (hover, touch) = (PEN_IN_RANGE, PEN_IN_RANGE | PEN_TOUCHING);
        r.step("hover");
        for i in 0..5 {
            send(r, sample(hover, at(f64::from(i) * 10.0, 0.0), 0));
        }
        r.step("stroke with rising pressure");
        for i in 0..8u16 {
            let p = 8000 + i * 7000;
            send(r, sample(touch, at(40.0 + f64::from(i) * 10.0, 0.0), p));
        }
        send(r, sample(hover, at(110.0, 0.0), 0));
        r.step("tilt east 30, north 45, roll 90");
        for (tilt_deg, azimuth_deg) in [(30, 90), (45, 0)] {
            send(
                r,
                PenSample {
                    tilt_deg,
                    azimuth_deg,
                    roll_deg: 90,
                    ..sample(touch, at(0.0, 30.0), 30000)
                },
            );
        }
        send(r, sample(hover, at(0.0, 30.0), 0));
        r.step("barrel button");
        send(r, sample(hover | PEN_BARREL1, at(0.0, 60.0), 0));
        send(r, sample(touch | PEN_BARREL1, at(0.0, 60.0), 30000));
        send(r, sample(touch, at(10.0, 60.0), 30000));
        send(r, sample(hover, at(10.0, 60.0), 0));
        r.step("eraser");
        for i in 0..3 {
            let s = PenSample {
                tool: PenTool::Eraser,
                ..sample(touch, at(f64::from(i) * 10.0, 90.0), 40000)
            };
            send(r, s);
        }
        r.step("out of range");
        send(r, PenSample::default());
        drop(pen);
        r.wait(100);
    }

    fn touch_run(r: &mut Run, view: Rect) {
        use InputKind::{TouchDown, TouchMove, TouchUp};
        let c = centre(view);
        let off = |dx: f64, dy: f64| (c.0 + dx, c.1 + dy);
        r.step("tap");
        r.touch(TouchDown, 1, c);
        r.wait(60);
        r.touch(TouchUp, 1, c);
        r.wait(400);
        for (name, d) in [
            ("pan down", (0.0, 120.0)),
            ("pan up", (0.0, -120.0)),
            ("pan right", (120.0, 0.0)),
            ("pan left", (-120.0, 0.0)),
        ] {
            r.step(name);
            r.touch(TouchDown, 2, c);
            r.drag(2, c, off(d.0, d.1), 8, 30);
            r.wait(200);
            r.touch(TouchUp, 2, off(d.0, d.1));
            r.wait(150);
        }
        r.step("flick up, then momentum");
        r.touch(TouchDown, 3, off(0.0, 100.0));
        r.drag(3, off(0.0, 100.0), off(0.0, -60.0), 6, 10);
        r.touch(TouchUp, 3, off(0.0, -60.0));
        r.wait(1500);
        r.step("long press, right drag");
        r.touch(TouchDown, 4, c);
        r.wait(650);
        r.drag(4, c, off(40.0, 0.0), 4, 20);
        r.touch(TouchUp, 4, off(40.0, 0.0));
        r.wait(200);
        for (name, a, b) in [
            ("pinch out", (-50.0, 50.0), (-120.0, 120.0)),
            ("pinch in", (-120.0, 120.0), (-50.0, 50.0)),
        ] {
            r.step(name);
            r.touch(TouchDown, 5, off(a.0, 0.0));
            r.touch(TouchDown, 6, off(a.1, 0.0));
            for i in 1..=6 {
                let k = f64::from(i) / 6.0;
                r.wait(20);
                r.touch(TouchMove, 5, off(a.0 + (b.0 - a.0) * k, 0.0));
                r.touch(TouchMove, 6, off(a.1 + (b.1 - a.1) * k, 0.0));
            }
            r.touch(TouchUp, 5, off(b.0, 0.0));
            r.touch(TouchUp, 6, off(b.1, 0.0));
            r.wait(200);
        }
        r.step("rotate counter-clockwise");
        r.touch(TouchDown, 7, off(-80.0, 0.0));
        r.touch(TouchDown, 8, off(80.0, 0.0));
        for i in 1..=6 {
            let a = f64::from(i) * 5.0_f64.to_radians();
            r.wait(20);
            r.touch(TouchMove, 7, off(-80.0 * a.cos(), 80.0 * a.sin()));
            r.touch(TouchMove, 8, off(80.0 * a.cos(), -80.0 * a.sin()));
        }
        r.touch(TouchUp, 7, c);
        r.touch(TouchUp, 8, c);
        r.wait(200);
    }
}
