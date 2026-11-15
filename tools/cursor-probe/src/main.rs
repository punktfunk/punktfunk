//! Cursor-as-metadata on-glass probe (see Cargo.toml). Run inside the target user session:
//!
//! ```sh
//! DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus cursor-probe [--seconds 20] [--embedded] [--gpu]
//! ```
//!
//! `--embedded` A/Bs the pre-channel path (compositor embeds the pointer, no metadata expected);
//! `--gpu` takes the zero-copy dmabuf negotiation a real session uses instead of the CPU mmap path.

// A diagnostic that drives the same PipeWire/dmabuf negotiation the host does, but through safe
// wrappers only. Free to lock down, and worth locking down: a probe is exactly the kind of tool
// where "just deref it quickly to see what's there" is tempting.
#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cursor-probe is Linux-only (compositor virtual outputs)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result};
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const W: u32 = 1280;
    const H: u32 = 800;

    pub fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        let args: Vec<String> = std::env::args().collect();
        let arg = |name: &str| {
            args.iter()
                .skip_while(|a| a.as_str() != name)
                .nth(1)
                .cloned()
        };
        let secs: u64 = arg("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
        let embedded = args.iter().any(|a| a == "--embedded");
        let gpu = args.iter().any(|a| a == "--gpu");

        let compositor = pf_vdisplay::detect()?;
        println!("cursor-probe: compositor = {compositor:?}");
        let mut vd = pf_vdisplay::open(compositor)?;
        // The cursor-channel session's request: out-of-band cursor (Mutter/KWin metadata mode).
        vd.set_hw_cursor(!embedded);
        println!(
            "cursor-probe: virtual output with {} cursor",
            if embedded {
                "EMBEDDED (compositor paints it)"
            } else {
                "METADATA (out-of-band)"
            }
        );
        let vout = vd
            .create(pf_vdisplay::Mode {
                width: W,
                height: H,
                refresh_hz: 60,
                refresh_mhz: 0,
            })
            .context("create the virtual output")?;
        println!("cursor-probe: node_id = {}", vout.node_id);

        // Encode-backend facts don't matter to the metadata question; all-off = CPU-friendly.
        let producer = match compositor {
            pf_vdisplay::Compositor::Kwin => pf_capture::Producer::Kwin,
            pf_vdisplay::Compositor::Gamescope => pf_capture::Producer::Gamescope,
            _ => pf_capture::Producer::Other,
        };
        let mut cap = pf_capture::open_virtual_output(
            vout.remote_fd,
            vout.node_id,
            vout.preferred_mode,
            vout.keepalive,
            pf_capture::VirtualOutputOpts {
                allow_zerocopy: gpu,
                expect_exact_dims: vout.expect_exact_dims,
                producer,
                ..Default::default()
            },
            || pf_capture::ZeroCopyPolicy {
                backend_is_gpu: gpu,
                ..Default::default()
            },
        )
        .context("attach the PipeWire capturer")?;
        if let Some(feed) = cap.feed() {
            feed.set_active(true);
        }
        // A cursor-channel session in capture mode: the HOST composites, so the capturer must
        // keep surfacing the overlay (this is a no-op on the Linux portal capturer, but state it
        // the way the encode loop does).
        if let Some(cursor) = cap.cursor() {
            cursor.set_forward(false);
        }

        // Wiggle the pointer over the virtual output through the production injector (libei on
        // GNOME/KWin). The region ladder maps the absolute events onto the output by size match —
        // W×H is deliberately distinctive.
        let backend = pf_inject::default_backend();
        println!("cursor-probe: input backend = {backend:?} (4 s device-resume wait)");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_inj = stop.clone();
        // `dyn InputInjector` isn't Send — build it on the thread that drives it.
        let injector = std::thread::spawn(move || {
            let mut inj = match pf_inject::open(backend) {
                Ok(i) => i,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "cursor-probe: injector open failed");
                    return;
                }
            };
            std::thread::sleep(Duration::from_secs(4));
            let flags = (W << 16) | (H & 0xffff);
            let (cx, cy, r) = ((W / 2) as f64, (H / 2) as f64, 200.0f64);
            let mut t = 0.0f64;
            let started = Instant::now();
            let mut announced_rel = false;
            while !stop_inj.load(Ordering::Relaxed) {
                // Phase 1 (8 s): ABSOLUTE wiggle — parks the pointer on the virtual output and
                // proves the metadata path. Phase 2: RELATIVE circles — what a capture-mode
                // (pointer-lock) client sends; watch whether the live meta position keeps moving.
                let e = if started.elapsed() < Duration::from_secs(8) {
                    InputEvent {
                        kind: InputKind::MouseMoveAbs,
                        _pad: [0; 3],
                        code: 0,
                        x: (cx + r * t.cos()) as i32,
                        y: (cy + r * t.sin()) as i32,
                        flags,
                    }
                } else {
                    if !announced_rel {
                        announced_rel = true;
                        println!("cursor-probe: switching to RELATIVE motion (capture-mode model)");
                    }
                    InputEvent {
                        kind: InputKind::MouseMove,
                        _pad: [0; 3],
                        code: 0,
                        x: (12.0 * t.cos()) as i32,
                        y: (12.0 * t.sin()) as i32,
                        flags: 0,
                    }
                };
                if let Err(err) = inj.inject(&e) {
                    tracing::warn!(error = %format!("{err:#}"), "cursor-probe: inject failed");
                }
                t += 0.15;
                std::thread::sleep(Duration::from_millis(25));
            }
        });

        let dump = args.iter().any(|a| a == "--dump");
        let dump_dir = format!(
            "/tmp/pf-probe-frames-{}",
            if embedded { "embedded" } else { "metadata" }
        );
        if dump {
            let _ = std::fs::remove_dir_all(&dump_dir);
            let _ = std::fs::create_dir_all(&dump_dir);
        }
        let mut prev: Vec<u8> = Vec::new();
        let mut changed = 0u64;
        let mut dumped = 0u32;

        let deadline = Instant::now() + Duration::from_secs(secs);
        let (mut frames, mut with_overlay) = (0u64, 0u64);
        let mut last_report = Instant::now();
        let mut live_seen = false;
        while Instant::now() < deadline {
            // Damage-driven source: timeouts (gaps) are normal, hence the missing error arm and
            // the provisional budget, whose expiry convicts no offer.
            let budget = pf_capture::FirstFrame::Provisional(Duration::from_millis(500));
            if let Ok(f) = cap.next_frame(budget) {
                frames += 1;
                if let pf_frame::FramePayload::Cpu(data) = &f.payload {
                    if !prev.is_empty()
                        && prev.len() == data.len()
                        && prev.as_slice() != data.as_slice()
                    {
                        changed += 1;
                    }
                    prev.clear();
                    prev.extend_from_slice(data);
                    if dump && frames % 15 == 1 && dumped < 16 {
                        dumped += 1;
                        let path = format!("{dump_dir}/frame-{frames:05}.ppm");
                        match dump_ppm(&path, f.width, f.height, f.format, data) {
                            Ok(()) => println!("cursor-probe: dumped {path}"),
                            Err(e) => eprintln!("cursor-probe: dump failed: {e:#}"),
                        }
                    }
                }
                if let Some(c) = &f.cursor {
                    with_overlay += 1;
                    if with_overlay == 1 {
                        println!(
                            "cursor-probe: FIRST frame-attached overlay: pos=({}, {}) \
                             {}x{} serial={} visible={}",
                            c.x, c.y, c.w, c.h, c.serial, c.visible
                        );
                    }
                }
            }
            if last_report.elapsed() >= Duration::from_secs(2) {
                last_report = Instant::now();
                let live = cap.cursor().and_then(|c| c.overlay());
                if live.is_some() {
                    live_seen = true;
                }
                match live {
                    Some(c) => println!(
                        "cursor-probe: frames={frames} with_overlay={with_overlay} live: \
                         pos=({}, {}) {}x{} serial={} visible={}",
                        c.x, c.y, c.w, c.h, c.serial, c.visible
                    ),
                    None => println!(
                        "cursor-probe: frames={frames} with_overlay={with_overlay} live: NONE \
                         (no SPA_META_Cursor bitmap yet)"
                    ),
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = injector.join();

        println!("---");
        println!(
            "cursor-probe: content-changed frames: {changed}/{frames} (a moving embedded \
             cursor over a static desktop should change nearly every frame)"
        );
        if embedded {
            println!(
                "cursor-probe: EMBEDDED run — no metadata expected; check the dumped frames. \
                 frames={frames}"
            );
        } else if live_seen || with_overlay > 0 {
            println!(
                "cursor-probe: VERDICT: metadata cursor DELIVERED ({with_overlay}/{frames} \
                 frames carried it; live overlay seen: {live_seen})"
            );
        } else {
            println!(
                "cursor-probe: VERDICT: NO cursor metadata after {secs}s of pointer motion over \
                 the virtual output ({frames} frames) — the compositor is not delivering \
                 SPA_META_Cursor on this stream"
            );
        }
        Ok(())
    }

    /// Write a packed 4-bpp frame as a PPM, swapping B/R for the BGR layouts. Enough fidelity
    /// to answer "is the pointer in the pixels".
    fn dump_ppm(
        path: &str,
        w: u32,
        h: u32,
        format: pf_frame::PixelFormat,
        data: &[u8],
    ) -> Result<()> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        write!(out, "P6\n{w} {h}\n255\n")?;
        let (ri, gi, bi) = match format {
            pf_frame::PixelFormat::Rgbx | pf_frame::PixelFormat::Rgba => (0usize, 1usize, 2usize),
            _ => (2usize, 1usize, 0usize),
        };
        for px in data.chunks_exact(4).take((w as usize) * (h as usize)) {
            out.write_all(&[px[ri], px[gi], px[bi]])?;
        }
        Ok(())
    }
}
