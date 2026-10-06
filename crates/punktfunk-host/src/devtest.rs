//! CLI arms that exercise a host subsystem without a streaming client.
//!
//! Linux: UHID DualSense / Switch Pro, libei/wlr input, pad-sink and usbip audio,
//! per-monitor mirror, libei absolute-input ladder. Windows: UMDF DualSense-family,
//! Steam Deck spike, pad-audio endpoints. Each fn is the full `punktfunk-host`
//! subcommand; `main.rs` only forwards. Flags live on the fn.

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;

/// The value after `name` in `args`, e.g. `--seconds 30`.
#[cfg(target_os = "linux")]
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .skip_while(|a| *a != name)
        .nth(1)
        .map(String::as_str)
}

/// [`flag`] parsed, or `default` when absent or malformed.
#[cfg(target_os = "linux")]
fn flag_or<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    flag(args, name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Capture a pad's split_quad mix for `secs`, printing chunks and per-pair peaks each second.
///
/// ch0/1 are the speaker, ch2/3 the coils. A remix or a UAC channel-order slip zeros one pair
/// while a global peak looks fine, so the pairs are metered apart.
#[cfg(target_os = "linux")]
fn meter_split_quad(
    cap: &mut dyn crate::audio::AudioCapturer,
    secs: u64,
    what: &str,
) -> Result<()> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut chunks, mut samples) = (0u64, 0u64);
    let (mut peak_spk, mut peak_coil) = (0f32, 0f32);
    let mut last_report = Instant::now();
    while Instant::now() < deadline {
        let c = cap.next_chunk().with_context(|| what.to_owned())?;
        if !c.is_empty() {
            chunks += 1;
            samples += c.len() as u64;
            for f in c.chunks_exact(4) {
                peak_spk = peak_spk.max(f[0].abs()).max(f[1].abs());
                peak_coil = peak_coil.max(f[2].abs()).max(f[3].abs());
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "  chunks={chunks} samples={samples} (~{:.1}ms of 4ch audio) \
                 peak_speaker={peak_spk:.4} peak_coils={peak_coil:.4}",
                samples as f64 / (4.0 * 48.0)
            );
            (chunks, samples, peak_spk, peak_coil) = (0, 0, 0.0, 0.0);
        }
    }
    Ok(())
}

/// Scripted stylus through [`PenTracker`](punktfunk_core::quic::PenTracker) → the "Punktfunk Pen"
/// uinput tablet. No client.
///
/// Hover in, tip down, sine stroke with pressure ramp + tilt, tip up, hover out.
/// Watch a pressure brush, or `sudo libinput debug-events` (`TABLET_TOOL_PROXIMITY`
/// / `TIP` / `AXIS`). `design/pen-tablet-input.md`.
#[cfg(target_os = "linux")]
pub fn pen_test() -> Result<()> {
    use punktfunk_core::quic::{
        PenBatch, PenSample, PenTracker, PenTransition, PEN_IN_RANGE, PEN_TOUCHING,
    };
    use std::time::Duration;

    let mut dev = crate::inject::pen::VirtualPen::create()?;
    let mut tracker = PenTracker::default();
    let mut out: Vec<PenTransition> = Vec::new();
    // 2 s: compositor enumerates the new evdev node; events before that are dropped.
    std::thread::sleep(Duration::from_secs(2));

    let mut seq = 0u16;
    let mut send = |tracker: &mut PenTracker, out: &mut Vec<PenTransition>, s: PenSample| {
        out.clear();
        tracker.apply(&PenBatch::new(seq, &[s]), out);
        seq = seq.wrapping_add(1);
        dev.apply_batch(out);
    };

    tracing::info!("pen-test: hover in, then a 3 s pressure-ramped sine stroke");
    let hover = |x: f32| PenSample {
        state: PEN_IN_RANGE,
        x,
        y: 0.5,
        distance: 300,
        ..Default::default()
    };
    for i in 0..20 {
        send(&mut tracker, &mut out, hover(0.05 + i as f32 * 0.005));
        std::thread::sleep(Duration::from_millis(10));
    }
    const STEPS: u32 = 360;
    for i in 0..=STEPS {
        let t = i as f32 / STEPS as f32;
        send(
            &mut tracker,
            &mut out,
            PenSample {
                state: PEN_IN_RANGE | PEN_TOUCHING,
                x: 0.15 + 0.7 * t,
                y: 0.5 + 0.2 * (t * std::f32::consts::TAU * 2.0).sin(),
                // 6553..65535 (10 %→100 %): a pressure brush must visibly widen.
                pressure: (6553.0 + 58982.0 * t) as u16,
                distance: 0,
                tilt_deg: 25 + (20.0 * t) as u8,
                azimuth_deg: ((90.0 + 180.0 * t) as u16) % 360,
                roll_deg: ((360.0 * t) as u16) % 360,
                ..Default::default()
            },
        );
        std::thread::sleep(Duration::from_millis(8));
    }
    for i in 0..10 {
        send(&mut tracker, &mut out, hover(0.85 + i as f32 * 0.005));
        std::thread::sleep(Duration::from_millis(10));
    }
    send(&mut tracker, &mut out, PenSample::default()); // state 0 = out of range
    tracing::info!("pen-test: done (stroke drawn, pen out of range) — device destroyed on exit");
    Ok(())
}

/// Scripted mouse + keyboard through the session input backend (libei / wlr). No client.
#[cfg(target_os = "linux")]
pub fn input_test() -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;

    let backend = crate::inject::default_backend();
    tracing::info!(?backend, "input-test: opening injector");
    let mut inj = crate::inject::open(backend)?;
    // 4 s: libei portal/EIS session + device resume; events before that are dropped.
    std::thread::sleep(Duration::from_secs(4));

    let ev = |kind, code, x, y| InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags: 0,
    };
    // `PUNKTFUNK_INPUT_TEST_ABS=WxH`: MouseMoveAbs (touch → abs). `xdotool getmouselocation` should jump.
    if let Ok(dims) = std::env::var("PUNKTFUNK_INPUT_TEST_ABS") {
        let (w, h) = dims
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .unwrap_or((1280, 800));
        let flags = (w << 16) | (h & 0xffff);
        let pts = [
            (100, 100),
            (w as i32 - 100, 100),
            (w as i32 - 100, h as i32 - 100),
            (100, h as i32 - 100),
            (w as i32 / 2, h as i32 / 2),
        ];
        tracing::info!(w, h, "input-test: ABS mode — corners + center, 1s apart");
        for (x, y) in pts {
            let mut e = ev(InputKind::MouseMoveAbs, 0, x, y);
            e.flags = flags;
            if let Err(err) = inj.inject(&e) {
                tracing::warn!(error = %format!("{err:#}"), "input-test: abs inject failed");
            }
            tracing::info!(x, y, "input-test: abs move emitted");
            std::thread::sleep(Duration::from_secs(1));
        }
        tracing::info!("input-test: done (abs)");
        return Ok(());
    }
    tracing::info!(
        "input-test: injecting a mouse square + 'A'/click taps for ~8s (watch wev / focused app)"
    );
    for i in 0..160u32 {
        let (dx, dy) = match (i / 10) % 4 {
            0 => (12, 0),
            1 => (0, 12),
            2 => (-12, 0),
            _ => (0, -12),
        };
        if let Err(e) = inj.inject(&ev(InputKind::MouseMove, 0, dx, dy)) {
            tracing::warn!(error = %format!("{e:#}"), "input-test: inject failed");
        }
        if i % 20 == 0 {
            let _ = inj.inject(&ev(InputKind::KeyDown, 0x41, 0, 0)); // 'A'
            let _ = inj.inject(&ev(InputKind::KeyUp, 0x41, 0, 0));
            let _ = inj.inject(&ev(InputKind::MouseButtonDown, 1, 0, 0)); // left click
            let _ = inj.inject(&ev(InputKind::MouseButtonUp, 1, 0, 0));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!("input-test: done");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn input_test() -> Result<()> {
    anyhow::bail!("input-test requires Linux")
}

/// What decides HDR on this box: the monitor's colour mode, gamescope's PQ capture and
/// knob, the encoder's 10-bit profiles, and the verdict for each plane.
#[cfg(target_os = "linux")]
pub fn hdr_probe() -> Result<()> {
    let monitor_hdr = pf_capture::gnome_hdr_monitor_active();
    let hevc10 = crate::encode::can_encode_10bit(crate::encode::Codec::H265);
    let av110 = crate::encode::can_encode_10bit(crate::encode::Codec::Av1);
    let gs_binary_hdr = pf_vdisplay::gamescope_hdr_available(None);
    let gs_knob = pf_host_config::config().gamescope_hdr;
    let compositor = crate::vdisplay::detect().ok();
    println!("monitor in BT.2100 (HDR) colour mode: {monitor_hdr}");
    println!("gamescope offers 10-bit PQ capture:   {gs_binary_hdr}");
    println!("PUNKTFUNK_GAMESCOPE_HDR:              {gs_knob}");
    // In-node cursor lets the session take the zero-CSC encode source; otherwise a
    // full-frame blend. Invisible until you compare two streams, so print it here.
    println!(
        "gamescope paints the cursor in-node:  {}",
        pf_vdisplay::gamescope_composites_cursor(None)
    );
    println!("encoder Main10 (HEVC): {hevc10}");
    println!("encoder 10-bit (AV1):  {av110}");
    println!(
        "native-plane HDR on the resolved compositor ({}): {}",
        compositor.map_or("none".to_string(), |c| format!("{c:?}")),
        crate::capture::capturer_supports_hdr_for(compositor, None)
    );
    println!(
        "GameStream HDR capable (PUNKTFUNK_10BIT + a capable source + encoder): {}",
        crate::gamestream::host_hdr_capable()
    );
    Ok(())
}

/// Connector names `PUNKTFUNK_CAPTURE_MONITOR` takes — available before the mgmt API is up.
#[cfg(target_os = "linux")]
pub fn list_monitors() -> Result<()> {
    let compositor = crate::vdisplay::detect()?;
    let monitors = crate::vdisplay::monitors::list(compositor)
        .with_context(|| format!("enumerate monitors on {compositor:?}"))?;
    if monitors.is_empty() {
        println!("{compositor:?}: no monitors");
        return Ok(());
    }
    let pinned = crate::vdisplay::capture_monitor();
    println!("{compositor:?}:");
    for m in &monitors {
        let mut tags = Vec::new();
        if m.primary {
            tags.push("primary");
        }
        if !m.enabled {
            tags.push("disabled");
        }
        if m.managed {
            tags.push("punktfunk virtual display");
        }
        if pinned
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(&m.connector))
        {
            tags.push("PINNED");
        }
        println!(
            "  {:<12} {:>13} at +{},+{}  scale {}  {}{}",
            m.connector,
            m.mode_label(),
            m.x,
            m.y,
            m.scale,
            m.description,
            if tags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", tags.join(", "))
            }
        );
    }
    Ok(())
}

/// Virtual DualSense via UHID: Cross, left-stick sweep, print kernel HID output. No session.
///
/// `evtest`, `/dev/input/by-id/*Punktfunk*`, `wpctl status`. `--edge` is 054C:0DF2 and
/// cycles the four back paddles (`BTN_TRIGGER_HAPPY1..4` on kernel ≥ 7.2; older kernels:
/// bind + hidraw byte 10).
#[cfg(target_os = "linux")]
pub fn dualsense_test(args: &[String]) -> Result<()> {
    use crate::inject::dualsense::{DsUhidIdentity, DualSensePad};
    use crate::inject::dualsense_proto::{edge_paddle_bits, DsState};
    let secs: u64 = flag_or(args, "--seconds", 20);
    let edge = args.iter().any(|a| a == "--edge");
    let (identity, label) = if edge {
        (DsUhidIdentity::dualsense_edge(), "DualSense Edge")
    } else {
        (DsUhidIdentity::dualsense(), "DualSense")
    };
    use std::time::{Duration, Instant};
    let mut pad = DualSensePad::open(0, &identity)
        .with_context(|| format!("create virtual {label} via /dev/uhid"))?;
    // 800 ms: hid-playstation GET_REPORT init; input nodes appear after that.
    let init = Instant::now() + Duration::from_millis(800);
    while Instant::now() < init {
        pad.service(0);
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "virtual {label} created — check `evtest`, `ls /dev/input/by-id/*Punktfunk*`, \
         `ls /sys/class/leds/`. Cycling Cross + sweeping LS for {secs}s."
    );
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut i, mut last_write) = (0i32, Instant::now());
    while Instant::now() < deadline {
        let fb = pad.service(0);
        if let Some((low, high)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high}");
        }
        for o in fb.hidout {
            println!("  hid output from kernel/game: {o:?}");
        }
        if last_write.elapsed() >= Duration::from_millis(300) {
            last_write = Instant::now();
            i += 1;
            let mut buttons = if i % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                0
            };
            if edge {
                // One paddle per beat so all four Edge slots show in evtest.
                buttons |= punktfunk_core::input::gamepad::BTN_PADDLE1 << (i % 4);
            }
            let lx = (((i % 64) - 32) * 1024) as i16;
            let mut st = DsState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
            if edge {
                st.buttons[2] |= edge_paddle_bits(buttons);
            }
            pad.write_state(&st).context("write report")?;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    println!("dualsense-test: done");
    Ok(())
}

/// Mint a DualSense-shaped PipeWire graph (`audio::pad_sink`) and capture the mix. No client.
///
/// Three nodes: mono `Speaker__sink`, positioned-quad `SpeakerHaptic__sink`, hidden AUX
/// parent. `pactl list sinks`; `pw-play --target <node.name>`. `--pad N`, `--edge`,
/// `--seconds N` (default 30).
#[cfg(target_os = "linux")]
pub fn pad_sink_test(args: &[String]) -> Result<()> {
    let secs: u64 = flag_or(args, "--seconds", 30);
    let pad: u8 = flag_or(args, "--pad", 0);
    let edge = args.iter().any(|a| a == "--edge");
    let mut cap = crate::audio::pad_sink::PadSinkCapturer::open(pad, edge)
        .context("mint pad-audio sink (is PipeWire running in this session?)")?;
    println!(
        "pad nodes minted (the split a real DualSense presents):\n  \
         speaker sink  = {}    (mono — GE-Proton's is_dualsense_speaker_sink target)\n  \
         haptic sink   = {}    (4ch POSITIONED FL,FR,RL,RR — the public quad a real pad shows)\n  \
         parent        = {}    (4ch AUX0..AUX3, hidden — what GE opens as pipewire:NODE=…)\n  \
         inspect: pactl list sinks | grep -A25 Speaker\n  \
         drive the coils via the POSITIONED sink (what a real pad's writers use):\n    \
         pw-play --target '{}' --channel-map 'front-left,front-right,rear-left,rear-right' <48k-file>\n  \
         drive the coils via the AUX parent (GE's own leg):\n    \
         pw-play --target '{}' --channel-map 'AUX0,AUX1,AUX2,AUX3' <48k-file>\n  \
         (a POSITIONED wav aimed at the AUX PARENT still folds into the speaker pair — that is \
         why the positioned sink exists)\nCapturing for {secs}s…",
        cap.node_name,
        cap.haptic_name,
        if cap.split_name.is_empty() {
            "(suppressed)"
        } else {
            cap.split_name.as_str()
        },
        cap.haptic_name,
        cap.split_name,
    );
    meter_split_quad(&mut cap, secs, "pad sink capture")?;
    println!("pad-sink-test: done");
    Ok(())
}

/// usbip DualSense: real USB + UAC card, capture from the isochronous endpoint. No client.
///
/// Proves what UHID cannot: `vhci_hcd` + `hid-playstation` bind, `snd-usb-audio` ALSA card
/// (GE-Proton `snd_card_next`; a minted PipeWire node is never that), USB parent so wine
/// walks HID → `usb_device` and gets a non-null ContainerId, then split_quad samples.
/// Ignores `PUNKTFUNK_DUALSENSE_USBIP` — this command is the opt-in. `--pad N`, `--seconds N`.
#[cfg(target_os = "linux")]
pub fn pad_usbip_test(args: &[String]) -> Result<()> {
    use std::time::Duration;
    let secs: u64 = flag_or(args, "--seconds", 30);
    let pad: u8 = flag_or(args, "--pad", 0);

    let _pad = pf_inject::dualsense_usbip::DualSenseUsbip::open(pad).context(
        "attach the usbip DualSense (is vhci_hcd loaded, and is \
         /sys/devices/platform/vhci_hcd.0/attach writable by the `punktfunk` group?)",
    )?;
    // 1.5 s: vhci enumerate + hid/snd bind + PipeWire; report before that looks like a miss.
    std::thread::sleep(Duration::from_millis(1500));

    match pf_inject::dualsense_usbip::find_usb_topology() {
        Some(t) => println!(
            "usb device attached:\n  \
             sysfs         = {}\n  \
             busnum/devnum = {}/{}   (with the vendor/product pair, these are exactly the fields \
             wine packs into the ContainerId — the pad's HID device and its audio sink must both \
             resolve to THIS node)\n  \
             check the sink agrees:\n    \
             pactl list sinks | grep -E 'Name:|sysfs'\n  \
             (`sysfs.path` on the sink is what winepulse prefixes with /sys and walks up; with a \
             real card PipeWire fills it in itself)",
            t.sysfs_path.display(),
            t.busnum,
            t.devnum,
        ),
        None => println!(
            "⚠ no 054c:0ce6 usb device found under vhci_hcd — the attach reported success but the \
             kernel did not enumerate it. Check `dmesg | tail -40`."
        ),
    }
    match std::fs::read_to_string("/proc/asound/cards") {
        Ok(cards) if cards.contains("DualSense") => println!(
            "alsa card present (GE-Proton's snd_card_next scan can see this):\n{}",
            cards.trim_end()
        ),
        Ok(cards) => println!(
            "⚠ no DualSense ALSA card — snd-usb-audio did NOT bind the audio function, so \
             GE-Proton's raw-ALSA haptic leg stays blind. /proc/asound/cards:\n{}\n  \
             check `dmesg | grep -i 'usb\\|snd' | tail -30`",
            cards.trim_end()
        ),
        Err(e) => println!("⚠ could not read /proc/asound/cards: {e}"),
    }
    println!(
        "drive it (either route converges on the pad's isochronous endpoint):\n  \
         via PipeWire:  pw-play --target <the pad's SpeakerHaptic sink> \
         --channel-map 'front-left,front-right,rear-left,rear-right' <48k-file>\n  \
         raw ALSA:      aplay -D plughw:CARD=Controller -f S16_LE -r 48000 -c 4 <48k-file>\n\
         Capturing for {secs}s…"
    );

    let mut cap = crate::audio::pad_usb::PadUsbCapturer::open(pad)
        .context("claim the usbip pad's audio stream")?;
    meter_split_quad(&mut cap, secs, "usb pad capture")?;
    println!("pad-usbip-test: done");
    Ok(())
}

/// Virtual Switch Pro via UHID: hid-nintendo probe, then A/B + left-stick sweep. No session.
///
/// `evtest`, `dmesg | grep nintendo`, SDL "Nintendo Switch Pro Controller". A/B are
/// positionally swapped.
#[cfg(target_os = "linux")]
pub fn switchpro_test(args: &[String]) -> Result<()> {
    use crate::inject::switch_pro::SwitchPad;
    use crate::inject::switch_proto::SwitchState;
    let secs: u64 = flag_or(args, "--seconds", 20);
    use std::time::{Duration, Instant};
    let mut pad =
        SwitchPad::pro(0).context("create virtual Switch Pro Controller via /dev/uhid")?;
    // 2.5 s: every hid-nintendo probe step blocks until the reply; stream 0x30 like hardware.
    println!("virtual Switch Pro created — servicing the hid-nintendo probe…");
    let init = Instant::now() + Duration::from_millis(2500);
    let mut hb = Instant::now();
    while Instant::now() < init {
        let fb = pad.service(0);
        for o in fb.hidout {
            println!("  probe feedback: {o:?}");
        }
        if hb.elapsed() >= Duration::from_millis(15) {
            hb = Instant::now();
            let _ = pad.write_state(&SwitchState::neutral());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("probe window over — cycling buttons + stick for {secs}s (check evtest)");
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut i, mut last_write) = (0i32, Instant::now());
    while Instant::now() < deadline {
        let fb = pad.service(0);
        // Switch Pro has no trigger motors; `PadFeedback` still carries four rumble levels.
        if let Some((low, high, lt, rt)) = fb.rumble {
            println!("  rumble from kernel/game: low={low} high={high} lt={lt} rt={rt}");
        }
        for o in fb.hidout {
            println!("  hid output from kernel/game: {o:?}");
        }
        // 15 ms: real Pro report rate; also feeds hid-nintendo's post-probe rate limiter.
        if last_write.elapsed() >= Duration::from_millis(15) {
            last_write = Instant::now();
            i += 1;
            let step = i / 20; // ~300 ms at 15 ms/report
            let buttons = if step % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                punktfunk_core::input::gamepad::BTN_B
            };
            let lx = (((i % 64) - 32) * 1024) as i16;
            let st = SwitchState::from_gamepad(buttons, lx, 0, 0, 0, 0, 0);
            pad.write_state(&st).context("write Switch Pro report")?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("switchpro-test: done");
    Ok(())
}

/// Mirror a named physical monitor and pull frames (`design/per-monitor-portal-capture.md`).
///
/// Same display-backend open as a session (`PUNKTFUNK_CAPTURE_MONITOR` routes to mirror).
/// Proves the compositor accepted a record request for that head and it produces pixels
/// at its own size. `--monitor <CONNECTOR>` (else the pin); `--seconds N`.
#[cfg(target_os = "linux")]
pub fn mirror_test(args: &[String]) -> Result<()> {
    use std::time::{Duration, Instant};
    let secs: u64 = flag_or(args, "--seconds", 5);
    // `--monitor` cannot set `PUNKTFUNK_CAPTURE_MONITOR`: config is snapshotted at
    // startup. Explicit connector → `open_mirror`; unset → pin / production `open`.
    let explicit = flag(args, "--monitor").map(str::to_owned);
    let want = explicit
        .clone()
        .or_else(crate::vdisplay::capture_monitor)
        .context(
            "no monitor named — pass --monitor <CONNECTOR> or set PUNKTFUNK_CAPTURE_MONITOR",
        )?;

    let compositor = crate::vdisplay::detect()?;
    let monitors = crate::vdisplay::monitors::list(compositor)?;
    let target = crate::vdisplay::monitors::resolve(&monitors, &want)?;
    println!(
        "mirror-test: {compositor:?} {} ({}) at +{},+{}",
        target.connector,
        target.mode_label(),
        target.x,
        target.y
    );

    let mut vd = match &explicit {
        Some(connector) => crate::vdisplay::open_mirror(compositor, connector)?,
        None => crate::vdisplay::open(compositor)?,
    };
    // Mirror ignores mode (the panel runs at its owner's). Pass the head's own if the pin drops.
    let mode = crate::vdisplay::Mode {
        width: target.width,
        height: target.height,
        refresh_hz: 60,
    };
    let vout = vd.create(mode).context("open the mirror display")?;
    println!(
        "mirror-test: node_id={} preferred={:?} ownership={:?}",
        vout.node_id, vout.preferred_mode, vout.ownership
    );

    // Default: session GPU/dmabuf. `--cpu` forces mmap — different PipeWire buffer types.
    let gpu = !args.iter().any(|a| a == "--cpu");
    let fmt = pf_frame::OutputFormat::resolve(false, gpu);
    println!(
        "mirror-test: capture path = {}",
        if gpu { "gpu/dmabuf" } else { "cpu/mmap" }
    );
    let mut cap = crate::capture::capture_virtual_output(
        vout,
        crate::capture::VirtualCaptureRequest {
            output: fmt,
            codec: None,
            capture: crate::session_plan::CaptureBackend::resolve(),
            kwin: compositor == crate::vdisplay::Compositor::Kwin,
            gamescope: compositor == crate::vdisplay::Compositor::Gamescope,
            stream_hz: mode.refresh_hz,
        },
    )
    .context("attach a capturer to the mirrored monitor")?;
    cap.set_active(true);

    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut frames, mut first) = (0u32, None);
    let mut idle = 0u32;
    let mut dims = (0u32, 0u32);
    while Instant::now() < deadline {
        match cap.next_frame_within(Duration::from_secs(5)) {
            Ok(f) => {
                if first.is_none() {
                    first = Some(Instant::now());
                    println!(
                        "mirror-test: FIRST FRAME {}x{} {:?}",
                        f.width, f.height, f.format
                    );
                }
                dims = (f.width, f.height);
                frames += 1;
            }
            // Timeout is not fatal: screencast is damage-driven; a static desktop produces
            // nothing for seconds. Wait the full `--seconds`, don't stop on the first gap.
            Err(e) => {
                idle += 1;
                if idle == 1 {
                    println!("mirror-test: (idle — no damage yet: {e:#})");
                }
            }
        }
    }
    match first {
        Some(_) => println!(
            "mirror-test: OK — {frames} frames in {secs}s at {}x{} ({:.1} fps over the whole run, \
             {idle} idle gaps). Compositor capture is damage-driven: a static desktop produces \
             nothing, so judge this by whether frames track what is happening on screen.",
            dims.0,
            dims.1,
            frames as f64 / secs as f64
        ),
        None => {
            anyhow::bail!("no frames arrived in {secs}s — the cast started but produced nothing")
        }
    }
    Ok(())
}

/// Absolute input at a named monitor (`design/per-monitor-portal-capture.md`).
///
/// Two same-size heads: matching a libei region by streamed mode can pick the wrong
/// screen. This uses the compositor's EIS regions and prints the mapped output.
/// `--monitor <CONNECTOR>` (else the pin); `--none` is unanchored A/B. Walks
/// `--width`×`--height` corners. Answer: `libei: absolute input maps into this output`.
#[cfg(target_os = "linux")]
pub fn anchor_test(args: &[String]) -> Result<()> {
    use punktfunk_core::input::{InputEvent, InputKind};
    use std::time::Duration;
    let unanchored = args.iter().any(|a| a == "--none");
    let w: u32 = flag_or(args, "--width", 1920);
    let h: u32 = flag_or(args, "--height", 1080);

    let compositor = crate::vdisplay::detect()?;
    let monitors = crate::vdisplay::monitors::list(compositor)?;
    println!(
        "anchor-test: {compositor:?} has {} monitor(s):",
        monitors.len()
    );
    for m in &monitors {
        println!(
            "  {:<12} {:>13} at +{},+{}",
            m.connector,
            m.mode_label(),
            m.x,
            m.y
        );
    }
    let same_size = monitors.iter().enumerate().any(|(i, a)| {
        monitors
            .iter()
            .skip(i + 1)
            .any(|b| a.width == b.width && a.height == b.height)
    });
    println!(
        "anchor-test: two same-size heads present: {} {}",
        same_size,
        if same_size {
            "— this run exercises the case the ladder exists for"
        } else {
            "— WEAK RIG: size matching would have picked correctly anyway"
        }
    );

    if unanchored {
        crate::inject::set_absolute_anchor(None);
        println!("anchor-test: UNANCHORED (--none) — the size/first rungs decide");
    } else {
        let want = flag(args, "--monitor")
            .map(str::to_owned)
            .or_else(crate::vdisplay::capture_monitor)
            .context("no monitor named — pass --monitor <CONNECTOR>, or --none for the A/B")?;
        let m = crate::vdisplay::monitors::resolve(&monitors, &want)?;
        crate::inject::set_absolute_anchor(Some(crate::inject::AbsoluteAnchor {
            origin: Some((m.x, m.y)),
            mapping_id: None,
        }));
        println!(
            "anchor-test: anchored at {} +{},+{} ({})",
            m.connector,
            m.x,
            m.y,
            m.mode_label()
        );
    }

    let backend = crate::inject::default_backend();
    if backend != crate::inject::Backend::Libei {
        // WlrVirtual (Sway) would green-pass with no region log. Need a compositor that speaks EI.
        anyhow::bail!(
            "input backend is {backend:?}, not libei — the absolute-region ladder only exists on \
             the libei backend; set PUNKTFUNK_INPUT_BACKEND=libei"
        );
    }
    let mut inj = crate::inject::open(backend)?;
    // 4 s: libei portal/EIS + device resume; events before that drop, and resume publishes regions.
    std::thread::sleep(Duration::from_secs(4));

    let flags = (w << 16) | (h & 0xffff);
    let pts = [
        (w as i32 / 2, h as i32 / 2),
        (60, 60),
        (w as i32 - 60, 60),
        (w as i32 - 60, h as i32 - 60),
        (60, h as i32 - 60),
        (w as i32 / 2, h as i32 / 2),
    ];
    println!("anchor-test: walking {w}x{h} — centre, four corners, centre (1s apart)");
    for (x, y) in pts {
        let e = InputEvent {
            kind: InputKind::MouseMoveAbs,
            _pad: [0; 3],
            code: 0,
            x,
            y,
            flags,
        };
        if let Err(err) = inj.inject(&e) {
            tracing::warn!(error = %format!("{err:#}"), "anchor-test: inject failed");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!(
        "anchor-test: done — read the `libei: absolute input maps into this output` line above \
         for the region that was chosen"
    );
    Ok(())
}
