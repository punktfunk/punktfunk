//! Scroll injection on-glass probe (see Cargo.toml). Run inside the target user session:
//!
//! ```sh
//! WAYLAND_DISPLAY=wayland-0 XDG_CURRENT_DESKTOP=GNOME \
//!   DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus \
//!   scroll-probe [--script swipe,lift,wheel,legacy] [--raw]
//! ```
//!
//! Per segment it prints what went in (wire events, distance) beside what the window got on
//! the vertical axis: the `axis_source` seen, Σ`axis`, Σ`value120`, stops, and cadence.
//! `PUNKTFUNK_INPUT_BACKEND` picks the injector like it does for the host.
//!
//! `--listen N` injects nothing and records a real client for N seconds. `--inject-at WxH`
//! opens no window: the script lands on whatever app sits at the centre of a desktop that
//! size, e.g. `gtk-scroll-logger.py` beside this file. `--script ticks` is a wheel turned
//! into a trackpad; it runs only when named.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("scroll-probe is Linux-only (Wayland compositors)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{bail, Context, Result};
    use punktfunk_core::input::scroll::{ScrollEvent, ScrollPhase, ScrollSource, SCROLL_SCALE};
    use punktfunk_core::input::{InputEvent, InputKind, SCROLL_FLAG_PRECISE};
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    /// 8 ms ≈ 120 Hz, a trackpad's report rate.
    const FRAME_MS: u64 = 8;

    #[derive(Debug)]
    enum Got {
        Source(String),
        Axis(f64),
        Value120(i32),
        Discrete(i32),
        Stop,
        Enter,
        Leave,
    }

    #[derive(Default)]
    struct App {
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        wm: Option<xdg_wm_base::XdgWmBase>,
        surface: Option<wl_surface::WlSurface>,
        pointer: Option<wl_pointer::WlPointer>,
        size: (i32, i32),
        configured: bool,
        entered: Arc<AtomicBool>,
        /// Vertical-axis pointer events as they were dispatched.
        got: Vec<(Instant, Got)>,
    }

    #[derive(Clone)]
    enum Step {
        Mark(&'static str),
        Send(InputEvent),
        Wait(u64),
    }

    fn scroll(source: ScrollSource, phase: ScrollPhase, delta: f64) -> InputEvent {
        ScrollEvent {
            source,
            phase,
            axis: 0,
            delta: (delta * SCROLL_SCALE) as i32,
            axis_units: false,
        }
        .to_event()
    }

    fn legacy(x: i32, precise: bool) -> InputEvent {
        InputEvent {
            kind: InputKind::MouseScroll,
            _pad: [0; 3],
            code: 0,
            x,
            y: 0,
            flags: if precise { SCROLL_FLAG_PRECISE } else { 0 },
        }
    }

    fn nudge(dx: i32) -> InputEvent {
        InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: dx,
            y: 0,
            flags: 0,
        }
    }

    fn script(wanted: &str) -> Vec<Step> {
        use ScrollPhase::*;
        use ScrollSource::*;
        let want = |name| wanted.split(',').any(|w| w == name || w == "all");
        let mut s = Vec::new();
        let tick = |s: &mut Vec<Step>, e| s.extend([Step::Send(e), Step::Wait(FRAME_MS)]);
        // Each segment starts the way a hand does: the pointer moves, then it scrolls.
        let mark = |s: &mut Vec<Step>, name| {
            s.extend([
                Step::Mark(name),
                Step::Send(nudge(1)),
                Step::Wait(20),
                Step::Send(nudge(-1)),
                Step::Wait(100),
            ])
        };
        if want("swipe") {
            mark(&mut s, "finger swipe + momentum");
            tick(&mut s, scroll(Finger, Begin, 2.0));
            for _ in 0..20 {
                tick(&mut s, scroll(Finger, Update, 6.0));
            }
            tick(&mut s, scroll(Finger, End, 0.0));
            let mut v = 6.0;
            tick(&mut s, scroll(Finger, MomentumBegin, v));
            for _ in 0..40 {
                v *= 0.93;
                tick(&mut s, scroll(Finger, Momentum, v));
            }
            tick(&mut s, scroll(Finger, MomentumEnd, 0.0));
            s.push(Step::Wait(600));
        }
        // A wheel turned into a trackpad (Mac Mouse Fix): every tick is a short tracked
        // gesture, a lift, and a momentum tail the next tick cuts short.
        if wanted.split(',').any(|w| w == "ticks") {
            mark(&mut s, "wheel-as-trackpad ticks");
            for tick_no in 0..8 {
                tick(&mut s, scroll(Finger, Begin, 1.5));
                for k in 0..9 {
                    tick(&mut s, scroll(Finger, Update, 2.0 + 0.5 * f64::from(k)));
                }
                tick(&mut s, scroll(Finger, End, 0.0));
                let mut v = 6.0;
                tick(&mut s, scroll(Finger, MomentumBegin, v));
                let tail = if tick_no == 7 { 60 } else { 3 };
                for _ in 0..tail {
                    v *= 0.94;
                    tick(&mut s, scroll(Finger, Momentum, v));
                }
                tick(&mut s, scroll(Finger, MomentumEnd, 0.0));
            }
            s.push(Step::Wait(1500));
        }
        if want("lift") {
            mark(&mut s, "finger lift, no momentum");
            tick(&mut s, scroll(Finger, Begin, 2.0));
            for _ in 0..15 {
                tick(&mut s, scroll(Finger, Update, 4.0));
            }
            tick(&mut s, scroll(Finger, End, 0.0));
            s.push(Step::Wait(600));
        }
        if want("wheel") {
            mark(&mut s, "wheel, 5 detents");
            for _ in 0..5 {
                s.extend([Step::Send(scroll(Wheel, None, 120.0)), Step::Wait(80)]);
            }
            s.push(Step::Wait(600));
            mark(&mut s, "hi-res wheel, 2 detents in 16");
            for _ in 0..16 {
                s.extend([Step::Send(scroll(Wheel, None, 15.0)), Step::Wait(16)]);
            }
            s.push(Step::Wait(600));
        }
        if want("legacy") {
            mark(&mut s, "legacy precise, 20 × 5 DIP");
            for _ in 0..20 {
                tick(&mut s, legacy(60, true));
            }
            s.push(Step::Wait(600));
            mark(&mut s, "legacy wheel, 3 detents");
            for _ in 0..3 {
                s.extend([Step::Send(legacy(120, false)), Step::Wait(80)]);
            }
            s.push(Step::Wait(600));
        }
        s
    }

    /// Wire events and distance a segment sent: DIP for surfaces, v120 for wheels. A legacy
    /// precise `x` is `x / 12` DIP.
    fn sent(steps: &[Step]) -> (usize, f64, &'static str) {
        let (mut n, mut sum, mut unit) = (0, 0.0, "");
        for step in steps {
            let Step::Send(e) = step else { continue };
            if !matches!(e.kind, InputKind::Scroll | InputKind::MouseScroll) {
                continue;
            }
            let (d, wheel) = match ScrollEvent::from_event(e) {
                Some(se) => (f64::from(se.delta) / SCROLL_SCALE, se.is_wheel()),
                None if e.flags & SCROLL_FLAG_PRECISE != 0 => (f64::from(e.x) / 12.0, false),
                None => (f64::from(e.x), true),
            };
            n += 1;
            sum += d;
            unit = if wheel { "v120" } else { "DIP" };
        }
        (n, sum, unit)
    }

    fn summarize(name: &str, steps: &[Step], got: &[&(Instant, Got)]) {
        let (n_sent, sum_sent, unit) = sent(steps);
        let mut sources = BTreeSet::new();
        let (mut axis, mut v120, mut discrete, mut stops) = (0.0, 0, 0, 0);
        let mut times = Vec::new();
        let mut stop_lag = None;
        for (t, g) in got {
            match g {
                Got::Source(s) => {
                    sources.insert(s.clone());
                }
                Got::Axis(v) => {
                    axis += v;
                    times.push(*t);
                }
                Got::Value120(v) => v120 += v,
                Got::Discrete(v) => discrete += v,
                Got::Stop => {
                    stops += 1;
                    stop_lag = times.last().map(|l| t.duration_since(*l));
                }
                Got::Enter | Got::Leave => {}
            }
        }
        let span = match (times.first(), times.last()) {
            (Some(a), Some(b)) => b.duration_since(*a),
            _ => Duration::ZERO,
        };
        let max_gap = times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .max()
            .unwrap_or_default();
        let rate = if span.is_zero() {
            0.0
        } else {
            (times.len() - 1) as f64 / span.as_secs_f64()
        };
        println!("── {name}");
        if n_sent > 0 {
            println!("   sent  {n_sent} events, Σ {sum_sent:.1} {unit}");
        }
        println!(
            "   got   {} axis events, source {:?}, Σaxis {axis:.1}, Σvalue120 {v120}, Σdiscrete {discrete}",
            times.len(),
            sources
        );
        println!(
            "         {stops} stop(s){}, {rate:.0} events/s over {} ms, longest gap {} ms",
            stop_lag.map_or(String::new(), |l| format!(
                " {} ms after the last axis",
                l.as_millis()
            )),
            span.as_millis(),
            max_gap.as_millis()
        );
    }

    pub fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .init();
        let args: Vec<String> = std::env::args().collect();
        let arg = |name: &str| {
            args.iter()
                .skip_while(|a| a.as_str() != name)
                .nth(1)
                .cloned()
        };
        let wanted = arg("--script").unwrap_or_else(|| "all".into());
        // Listen only: a real client scrolls over the window, nothing is injected.
        let listen: Option<u64> = arg("--listen").and_then(|s| s.parse().ok());
        let raw = args.iter().any(|a| a == "--raw");

        // Inject only: no window of our own, the script lands on whatever app sits at
        // the centre of a `WxH` desktop.
        if let Some(at) = arg("--inject-at") {
            let (w, h) = at
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?)))
                .context("--inject-at WxH")?;
            let svc = pf_inject::InjectorService::start();
            let tx = svc.sender();
            let centre = InputEvent {
                kind: InputKind::MouseMoveAbs,
                _pad: [0; 3],
                code: 0,
                x: w / 2,
                y: h / 2,
                flags: ((w as u32) << 16) | h as u32,
            };
            // libei drops events until its devices resume.
            for _ in 0..12 {
                let _ = tx.send(centre);
                std::thread::sleep(Duration::from_millis(100));
            }
            for step in script(&wanted) {
                match step {
                    Step::Mark(name) => println!("── injecting {name}"),
                    Step::Send(e) => {
                        let _ = tx.send(e);
                    }
                    Step::Wait(ms) => std::thread::sleep(Duration::from_millis(ms)),
                }
            }
            std::thread::sleep(Duration::from_millis(500));
            return Ok(());
        }

        let conn = Connection::connect_to_env().context("connect to the Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut app = App::default();
        queue.roundtrip(&mut app)?;
        let (Some(comp), Some(wm)) = (app.compositor.clone(), app.wm.clone()) else {
            bail!("the compositor lacks wl_compositor or xdg_wm_base");
        };
        let surface = comp.create_surface(&qh, ());
        let top = wm.get_xdg_surface(&surface, &qh, ()).get_toplevel(&qh, ());
        top.set_title("punktfunk scroll-probe".into());
        top.set_fullscreen(None);
        surface.commit();
        app.surface = Some(surface);
        while !app.configured {
            queue.blocking_dispatch(&mut app)?;
        }
        let (w, h) = app.size;
        println!(
            "scroll-probe: window {w}x{h}, backend {:?}",
            pf_inject::default_backend()
        );

        let done = Arc::new(AtomicBool::new(false));
        let (marks_tx, marks_rx) = std::sync::mpsc::channel::<(usize, Instant)>();
        let steps = if listen.is_some() {
            Vec::new()
        } else {
            script(&wanted)
        };
        let player = if let Some(secs) = listen {
            println!("scroll-probe: listening {secs} s — scroll over the window");
            let done = done.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(secs));
                done.store(true, Ordering::Relaxed);
            })
        } else {
            let svc = pf_inject::InjectorService::start();
            let tx = svc.sender();
            let entered = app.entered.clone();
            let done = done.clone();
            let plan = steps.clone();
            std::thread::spawn(move || {
                // Park the pointer on the window until it enters; libei drops events until
                // its devices resume.
                let centre = InputEvent {
                    kind: InputKind::MouseMoveAbs,
                    _pad: [0; 3],
                    code: 0,
                    x: w / 2,
                    y: h / 2,
                    flags: ((w as u32) << 16) | h as u32,
                };
                let deadline = Instant::now() + Duration::from_secs(8);
                while !entered.load(Ordering::Relaxed) && Instant::now() < deadline {
                    let _ = tx.send(centre);
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::thread::sleep(Duration::from_millis(300));
                for (i, step) in plan.into_iter().enumerate() {
                    match step {
                        Step::Mark(_) => {
                            let _ = marks_tx.send((i, Instant::now()));
                        }
                        Step::Send(e) => {
                            let _ = tx.send(e);
                        }
                        Step::Wait(ms) => std::thread::sleep(Duration::from_millis(ms)),
                    }
                }
                done.store(true, Ordering::Relaxed);
            })
        };

        let mut idle_since: Option<Instant> = None;
        loop {
            queue.dispatch_pending(&mut app)?;
            conn.flush()?;
            if let Some(guard) = queue.prepare_read() {
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: 50_000_000,
                };
                if poll(&mut fds, Some(&timeout))? > 0 {
                    guard.read()?;
                }
            }
            if done.load(Ordering::Relaxed) {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(300) {
                    break;
                }
            }
        }
        let _ = player.join();
        if !app.entered.load(Ordering::Relaxed) {
            println!("scroll-probe: the pointer never entered the window — nothing to measure");
        }
        if raw {
            for (t, g) in &app.got {
                println!(
                    "{:>8.1} ms  {g:?}",
                    t.duration_since(app.got[0].0).as_secs_f64() * 1e3
                );
            }
        }
        if listen.is_some() {
            // A gesture ends where the window heard nothing for 400 ms.
            let mut gesture: Vec<&(Instant, Got)> = Vec::new();
            let mut k = 0;
            for g in &app.got {
                if gesture
                    .last()
                    .is_some_and(|l| g.0.duration_since(l.0) > Duration::from_millis(400))
                {
                    k += 1;
                    summarize(&format!("gesture {k}"), &[], &gesture);
                    gesture.clear();
                }
                gesture.push(g);
            }
            if !gesture.is_empty() {
                summarize(&format!("gesture {}", k + 1), &[], &gesture);
            }
            return Ok(());
        }
        let marks: Vec<(usize, Instant)> = marks_rx.try_iter().collect();
        for (k, (i, at)) in marks.iter().enumerate() {
            let end = marks.get(k + 1).map(|m| m.0).unwrap_or(steps.len());
            let until = marks.get(k + 1).map(|m| m.1);
            let Step::Mark(name) = steps[*i] else {
                continue;
            };
            let got: Vec<&(Instant, Got)> = app
                .got
                .iter()
                .filter(|(t, _)| t >= at && until.is_none_or(|u| *t < u))
                .collect();
            summarize(name, &steps[*i..end], &got);
        }
        Ok(())
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for App {
        fn event(
            app: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            else {
                return;
            };
            match interface.as_str() {
                "wl_compositor" => {
                    app.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(registry.bind(name, version.min(2), qh, ())),
                // v8 brings axis_value120; below that the probe reads axis_discrete.
                "wl_seat" => {
                    let _: wl_seat::WlSeat = registry.bind(name, version.min(8), qh, ());
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for App {
        fn event(
            app: &mut Self,
            seat: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            // A compositor re-sends capabilities when a device comes or goes; a second
            // wl_pointer would hear every event twice.
            if let wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(caps),
            } = event
            {
                if caps.contains(wl_seat::Capability::Pointer) && app.pointer.is_none() {
                    app.pointer = Some(seat.get_pointer(qh, ()));
                }
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for App {
        fn event(
            app: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let vertical = |a: &WEnum<wl_pointer::Axis>| {
                matches!(a, WEnum::Value(wl_pointer::Axis::VerticalScroll))
            };
            let got = match event {
                wl_pointer::Event::Enter { .. } => {
                    app.entered.store(true, Ordering::Relaxed);
                    Some(Got::Enter)
                }
                wl_pointer::Event::Leave { .. } => Some(Got::Leave),
                wl_pointer::Event::AxisSource { axis_source } => {
                    Some(Got::Source(format!("{axis_source:?}")))
                }
                wl_pointer::Event::Axis { axis, value, .. } if vertical(&axis) => {
                    Some(Got::Axis(value))
                }
                wl_pointer::Event::AxisValue120 { axis, value120 } if vertical(&axis) => {
                    Some(Got::Value120(value120))
                }
                wl_pointer::Event::AxisDiscrete { axis, discrete } if vertical(&axis) => {
                    Some(Got::Discrete(discrete))
                }
                wl_pointer::Event::AxisStop { axis, .. } if vertical(&axis) => Some(Got::Stop),
                _ => None,
            };
            if let Some(g) = got {
                app.got.push((Instant::now(), g));
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
        fn event(
            _: &mut Self,
            wm: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                wm.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
        fn event(
            app: &mut Self,
            _: &xdg_toplevel::XdgToplevel,
            event: xdg_toplevel::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_toplevel::Event::Configure { width, height, .. } = event {
                if width > 0 && height > 0 {
                    app.size = (width, height);
                }
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for App {
        fn event(
            app: &mut Self,
            xs: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let xdg_surface::Event::Configure { serial } = event else {
                return;
            };
            xs.ack_configure(serial);
            if app.size == (0, 0) {
                app.size = (1280, 800);
            }
            if let (Some(shm), Some(surface)) = (&app.shm, &app.surface) {
                if let Ok(buffer) = blank_buffer(shm, app.size, qh) {
                    surface.attach(Some(&buffer), 0, 0);
                    surface.damage_buffer(0, 0, app.size.0, app.size.1);
                }
                surface.commit();
            }
            app.configured = true;
        }
    }

    /// A black XRGB buffer backed by an unlinked tmpfs file.
    fn blank_buffer(
        shm: &wl_shm::WlShm,
        (w, h): (i32, i32),
        qh: &QueueHandle<App>,
    ) -> Result<wl_buffer::WlBuffer> {
        let dir = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR")?;
        let path = std::path::Path::new(&dir).join(format!("scroll-probe-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .context("create the shm file")?;
        std::fs::remove_file(&path)?;
        let stride = w * 4;
        file.set_len((stride * h) as u64)?;
        let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), stride * h, qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        Ok(buffer)
    }

    delegate_noop!(App: ignore wl_compositor::WlCompositor);
    delegate_noop!(App: ignore wl_shm::WlShm);
    delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(App: ignore wl_buffer::WlBuffer);
    delegate_noop!(App: ignore wl_surface::WlSurface);
}
