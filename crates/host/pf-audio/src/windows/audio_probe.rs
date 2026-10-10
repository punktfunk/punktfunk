//! Devtest for the Windows audio substrate: mint Steam Streaming endpoints, prove a
//! tone crosses render→capture or WASAPI loopback, restore parked defaults, then
//! tear the probe nodes down.
//!
//! `ssm` / `sink` / `sss-primary` / `mint` / `plan` / `micpitch` / `micpins` /
//! `cleanup`. Nodes stamp `PunktfunkAudioProbe=1` under `Device Parameters`
//! because DeviceDesc dies at INF install; `cleanup` and
//! [`devnode_cleanup`](super::devnode_cleanup) match that marker, not the name.
//!
//! Evidence: `design/windows-audio-endpoints-and-vbcable.md`.

use super::devnode_api as da;
use super::pad_endpoint as pe;
use super::{audio_control, SAMPLE_RATE};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

/// `Device Parameters` stamp. `cleanup` and
/// [`devnode_cleanup`](super::devnode_cleanup) match this so a probe node cannot
/// outlive the product.
pub const PROBE_MARKER: &str = "PunktfunkAudioProbe";
/// Device Manager name until the INF install overwrites it.
const PROBE_DESC: &str = "Punktfunk Audio Probe";
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);
/// Tone renders at 0.5; autoconvert may attenuate. Above this is signal.
const SIGNAL_FLOOR: f32 = 0.05;

pub fn run(args: &[String]) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    let keep = args.iter().any(|a| a == "--keep");
    match args.get(1).map(String::as_str) {
        Some("ssm") => probe_ssm(keep),
        Some("sink") => probe_sink(keep),
        Some("sss-primary") => {
            let secs = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(4u32)
                .clamp(2, 30);
            probe_sss_primary(secs)
        }
        Some("cleanup") => cleanup(),
        // Mint into THIS process so a following `plan` can read the pick.
        Some("mint") => super::minted::devtest_mint(),
        // No default parking. `ensure_blocking` first — a fresh CLI process
        // would race its own worker.
        Some("plan") => {
            super::minted::ensure_blocking();
            let plan = super::audio_control::wire_now_full(false);
            let w = &plan.wiring;
            let show = |ep: &Option<super::wiring_plan::Endpoint>| match ep {
                Some((name, id)) => format!("{name:?} ({id})"),
                None => "-".into(),
            };
            println!("audio-plan: mic_render    = {}", show(&w.mic_render));
            println!("audio-plan: mic_capture   = {}", show(&w.mic_capture));
            println!("audio-plan: loopback      = {}", show(&w.loopback_render));
            println!("audio-plan: last_resort   = {}", w.loopback_last_resort);
            println!("audio-plan: mic_withheld  = {}", w.mic_withheld);
            println!(
                "audio-plan: narrowing     = {}",
                w.loopback_narrowing.as_deref().unwrap_or("-")
            );
            println!(
                "audio-plan: readiness     = {:?}",
                super::wiring_plan::readiness(w)
            );
            Ok(())
        }
        // 440 Hz into the minted mic render, measured on capture. ~440 = honest;
        // ~220 = some link is half the declared rate.
        Some("micpitch") => {
            super::minted::ensure_blocking();
            // `minted_ids` hides the mic pair (raw stereo→mono reads an octave low).
            let Some(m) = super::minted::provisioned() else {
                bail!("nothing minted on this box — run `audio-probe mint` first");
            };
            let (Some(render), Some(capture)) = (m.mic_render.clone(), m.mic_capture.clone())
            else {
                bail!("no minted microphone pair on this box — run `audio-probe mint` first");
            };
            println!("audio-probe micpitch: render={render}");
            println!("audio-probe micpitch: capture={capture}");
            let (peak, hz) = tone_while(&Some(render), 6, 440.0, || record_peak(&capture, 4))??;
            println!("audio-probe micpitch: peak={peak:.4}, 440 Hz read back as {hz:.0} Hz");
            if peak < SIGNAL_FLOOR {
                println!("  VERDICT: no signal crossed the pair — is the mic pump holding it?");
            } else if (hz - 440.0).abs() < 40.0 {
                println!("  VERDICT: pitch-true — the minted pair is innocent; the shift lives elsewhere.");
            } else if (hz - 220.0).abs() < 30.0 {
                println!(
                    "  VERDICT: OCTAVE DOWN — the driver forwards the stereo render stream \
                     into the mono capture raw; the render side must run MONO."
                );
            } else {
                println!("  VERDICT: off-pitch by an unusual ratio — measure again / check rates.");
            }
            Ok(())
        }
        // Driver `IsFormatSupported`, not the endpoint store. Exclusive-mode
        // mono is the escape hatch if shared configs disagree.
        Some("micpins") => {
            super::minted::ensure_blocking();
            let Some(m) = super::minted::provisioned() else {
                bail!("nothing minted on this box — run `audio-probe mint` first");
            };
            let (Some(render), Some(capture)) = (m.mic_render.clone(), m.mic_capture.clone())
            else {
                bail!("no minted microphone pair on this box");
            };
            for (label, id) in [("render", &render), ("capture", &capture)] {
                println!("audio-probe micpins: {label} = {id}");
                let device = pe::open_wasapi_device(id)?;
                let client = device.get_iaudioclient().context("IAudioClient")?;
                for ch in [1usize, 2] {
                    for bits in [16usize, 32] {
                        for rate in [44_100usize, 48_000, 96_000] {
                            let stype = if bits == 16 {
                                SampleType::Int
                            } else {
                                SampleType::Float
                            };
                            let fmt = WaveFormat::new(bits, bits, &stype, rate, ch, None);
                            let mut verdicts = Vec::new();
                            for (mode_label, mode) in [
                                ("excl", wasapi::ShareMode::Exclusive),
                                ("shared", wasapi::ShareMode::Shared),
                            ] {
                                let v = match client.is_supported(&fmt, &mode) {
                                    Ok(None) => "OK",
                                    Ok(Some(_)) => "alt",
                                    Err(_) => "no",
                                };
                                verdicts.push(format!("{mode_label}={v}"));
                            }
                            println!("  {ch}ch {bits:2}bit {rate:5}Hz  {}", verdicts.join(" "));
                        }
                    }
                }
            }
            Ok(())
        }
        _ => bail!(
            "usage: punktfunk-host audio-probe \
             <ssm|sink|sss-primary|mint|plan|micpitch|micpins|cleanup> [--keep]"
        ),
    }
}

fn probe_ssm(keep: bool) -> Result<()> {
    let (hwid, inf) = discover_driver("steamstreamingmicrophone", "SteamStreamingMicrophone.inf")?;
    println!("audio-probe ssm: hwid={hwid} inf={inf}");
    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let inst = da::create_media_devnode(PROBE_DESC, &hwid, write_probe_marker)?;
    println!("audio-probe ssm: created devnode {inst}");
    da::bind_driver(&hwid, &inf)?;

    let render_ep = pe::wait_for_endpoint(&inst, Direction::Render, ENDPOINT_WAIT)?;
    let capture_ep = match pe::wait_for_endpoint(&inst, Direction::Capture, ENDPOINT_WAIT) {
        Ok(ep) => ep,
        Err(e) => {
            println!("audio-probe ssm: render endpoint {render_ep} appeared, but:");
            println!("  {e:#}");
            println!("  VERDICT: FAIL (S3) — the minted SSM instance has NO capture endpoint;");
            println!("  a punktfunk-owned virtual mic cannot come from this driver.");
            restore_defaults(prev_render, prev_capture);
            if !keep {
                remove_devnode(&inst);
            }
            return Ok(());
        }
    };
    println!("audio-probe ssm: render={render_ep}");
    println!("audio-probe ssm: capture={capture_ep}");
    report_mix_format("render", &render_ep);

    // Both ends must stay open; the driver only moves audio while they do.
    let (peak, hz) = tone_while(&Some(render_ep.clone()), 5, 440.0, || {
        record_peak(&capture_ep, 3)
    })??;
    println!(
        "audio-probe ssm: capture peak over 3s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz"
    );
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: PASS (S3) — the minted Steam Streaming Microphone instance carries \
             audio render→capture; a punktfunk-owned virtual mic needs no VB-Cable where \
             Steam is installed."
        );
    } else {
        println!(
            "  VERDICT: FAIL (S3) — both endpoints minted but no audio crossed the pair \
             (peak {peak:.4} ≤ {SIGNAL_FLOOR}); the drop-VB-Cable decision reverts to \
             cable-for-mic-only."
        );
    }

    restore_defaults(prev_render, prev_capture);
    if keep {
        println!("audio-probe ssm: --keep — devnode {inst} left in place");
    } else {
        remove_devnode(&inst);
    }
    Ok(())
}

fn probe_sink(keep: bool) -> Result<()> {
    let (hwid, inf) = discover_driver("steamstreamingspeakers", "SteamStreamingSpeakers.inf")?;
    println!("audio-probe sink: hwid={hwid} inf={inf}");
    let prev_render = audio_control::default_render_id();
    let prev_capture = audio_control::default_capture_id();

    let inst = da::create_media_devnode(PROBE_DESC, &hwid, write_probe_marker)?;
    println!("audio-probe sink: created devnode {inst}");
    da::bind_driver(&hwid, &inf)?;
    let ep = pe::wait_for_endpoint(&inst, Direction::Render, ENDPOINT_WAIT)?;
    println!("audio-probe sink: endpoint={ep}");
    report_mix_format("sink", &ep);

    // Tone through the DEFAULT device (`None`), loopback on the parked sink —
    // the product routing, not a direct open of the minted endpoint.
    audio_control::set_default_endpoint(&ep).context("park the default playback on the sink")?;
    let measured = tone_while(&None, 5, 440.0, || loopback_peak(&ep, 3));
    // The operator's devices come back whatever the tone did.
    restore_defaults(prev_render, prev_capture);
    let (peak, hz) = measured??;
    println!(
        "audio-probe sink: loopback peak over 3s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz"
    );
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: PASS (S2) — default-routed audio reaches the minted Speakers instance \
             and its WASAPI loopback carries it; \"Punktfunk Speakers\" can be the canonical \
             client-only sink."
        );
    } else {
        println!(
            "  VERDICT: FAIL (S2) — the minted instance's loopback stayed silent \
             (peak {peak:.4} ≤ {SIGNAL_FLOOR}) despite default routing; the speakers leg of \
             Phase 2 dies and Phase 1 remains the fix."
        );
    }

    if keep {
        println!("audio-probe sink: --keep — devnode {inst} left in place");
    } else {
        remove_devnode(&inst);
    }
    Ok(())
}

fn probe_sss_primary(secs: u32) -> Result<()> {
    // Name-match the primary Speakers; skip leftover `--keep` probe nodes.
    let probes = probe_devnodes()?;
    let en = wasapi::DeviceEnumerator::new().map_err(|e| anyhow!("DeviceEnumerator: {e}"))?;
    let coll = en
        .get_device_collection(&Direction::Render)
        .map_err(|e| anyhow!("render collection: {e}"))?;
    let n = coll.get_nbr_devices().map_err(|e| anyhow!("count: {e}"))?;
    let mut target: Option<(String, String)> = None;
    for i in 0..n {
        let Ok(dev) = coll.get_device_at_index(i) else {
            continue;
        };
        let name = dev.get_friendlyname().unwrap_or_default();
        let id = dev.get_id().unwrap_or_default();
        if name.to_lowercase().contains("steam streaming speakers")
            && !probes
                .iter()
                .any(|(pi, _)| endpoint_of(pi) == Some(id.clone()))
        {
            target = Some((name, id));
            break;
        }
    }
    let Some((name, id)) = target else {
        bail!("no primary Steam Streaming Speakers render endpoint on this box");
    };
    let steam_running = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("steam.exe")
        })
        .unwrap_or(false);
    println!(
        "audio-probe sss-primary: endpoint {name:?} ({id}), steam.exe running: {steam_running}"
    );
    report_mix_format("primary", &id);

    let (peak, hz) = tone_while(&Some(id.clone()), secs + 1, 440.0, || {
        loopback_peak(&id, secs)
    })??;
    println!("audio-probe sss-primary: loopback peak over {secs}s = {peak:.4}, tone 440 Hz read back as {hz:.0} Hz");
    if peak > SIGNAL_FLOOR {
        println!(
            "  VERDICT: the primary SSS loopback CARRIES audio here (steam.exe running: \
             {steam_running}) — the \"validated silent\" verdict does not reproduce in this \
             state; record the state alongside."
        );
    } else {
        println!(
            "  VERDICT: the primary SSS loopback is SILENT (steam.exe running: \
             {steam_running}) — consistent with the wiring plan's last-resort tier."
        );
    }
    Ok(())
}

use super::minted::discover_driver;

/// `mark` callback of [`da::create_media_devnode`]: stamp `PROBE_MARKER`.
fn write_probe_marker(
    set: &da::DevInfoSet,
    did: &mut windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA,
) -> Result<()> {
    da::write_devparam_dword(set, did, PROBE_MARKER, 1)
}

fn probe_devnodes() -> Result<Vec<(String, u32)>> {
    let set = da::media_class_devs()?;
    Ok(set
        .iter()
        .filter_map(|did| {
            let marker = da::read_devparam_dword(&set, &did, PROBE_MARKER)?;
            Some((da::instance_id(&set, &did)?, marker))
        })
        .collect())
}

fn cleanup() -> Result<()> {
    let probes = probe_devnodes()?;
    if probes.is_empty() {
        println!("audio-probe cleanup: nothing to remove");
        return Ok(());
    }
    for (inst, _) in probes {
        remove_devnode(&inst);
    }
    Ok(())
}

/// Same teardown as `pad-endpoint remove`.
fn remove_devnode(inst: &str) {
    match pf_paths::remove_device(inst) {
        Ok(()) => println!("audio-probe: removed devnode {inst}"),
        Err(e) => println!("audio-probe: couldn't remove {inst}: {e:#}"),
    }
}

/// Render id of a probe node, if any — excludes leftovers from the primary match.
fn endpoint_of(inst: &str) -> Option<String> {
    pe::find_endpoint_for_devnode(inst).ok().flatten()
}

fn report_mix_format(label: &str, endpoint_id: &str) {
    match audio_control::mix_format_of(&(label.to_string(), endpoint_id.to_string())) {
        Some(f) => println!(
            "audio-probe: {label} engine mix format = {} Hz, {} ch, {} bits",
            f.rate_hz, f.channels, f.bits
        ),
        None => println!("audio-probe: {label} engine mix format = unknown (probe failed)"),
    }
}

fn tone_while<T>(
    target: &Option<String>,
    tone_secs: u32,
    hz: f32,
    body: impl FnOnce() -> T,
) -> Result<T> {
    let stop = Arc::new(AtomicBool::new(false));
    let (stop_t, target_t) = (stop.clone(), target.clone());
    let join = thread::Builder::new()
        .name("pf-audio-probe-tone".into())
        .spawn(move || render_tone(target_t.as_deref(), tone_secs, hz, &stop_t))
        .context("spawn tone thread")?;
    // Render open takes a beat; without this the measurement window starts silent.
    thread::sleep(Duration::from_millis(500));
    let out = body();
    stop.store(true, Ordering::SeqCst);
    match join.join() {
        Ok(Ok(())) => Ok(out),
        Ok(Err(e)) => Err(e.context("tone render failed")),
        Err(_) => Err(anyhow!("tone thread panicked")),
    }
}

/// Stereo on `target`, or the default render device.
fn render_tone(target: Option<&str>, seconds: u32, hz: f32, stop: &AtomicBool) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA, tone)")?;
    let device = match target {
        Some(id) => pe::open_wasapi_device(id)?,
        None => wasapi::DeviceEnumerator::new()
            .map_err(|e| anyhow!("DeviceEnumerator: {e}"))?
            .get_default_device(&Direction::Render)
            .map_err(|e| anyhow!("default render device: {e}"))?,
    };
    super::pad_capture::play_tone(&device, 2, None, |_| true, seconds, hz, stop)
}

/// Peak |sample| and zero-crossing Hz from capture or loopback. Peaks miss an
/// octave drop (440 Hz reading back as ~220).
///
/// Stereo request, crossings on ch 0. A MONO ask fails `Initialize` with
/// `0x88890008` on SSM even under autoconvert — this stack does not bridge
/// capture channel counts.
fn measure_peak(endpoint_id: &str, seconds: u32, loopback: bool) -> Result<(f32, f32)> {
    let device = pe::open_wasapi_device(endpoint_id)?;
    let mut client = device.get_iaudioclient().context("IAudioClient")?;
    let desired = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
    let (period, _) = client.get_device_period().context("device period")?;
    client
        .initialize_client(
            &desired,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
        )
        .with_context(|| {
            format!(
                "initialize {} client",
                if loopback { "loopback" } else { "record" }
            )
        })?;
    let h_event = client.set_get_eventhandle().context("event handle")?;
    let capture = client.get_audiocaptureclient().context("capture client")?;
    client.start_stream().context("start capture stream")?;

    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds.clamp(1, 60)));
    let mut bytes: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut peak = 0f32;
    let mut frames = 0u64;
    let mut crossings = 0u64;
    let mut prev_positive: Option<bool> = None;
    // Crossings over the SIGNAL span only; leading silence in the denominator
    // reads every tone low.
    let (mut first_signal, mut last_signal): (Option<u64>, Option<u64>) = (None, None);
    while Instant::now() < deadline {
        let _ = h_event.wait_for_event(100);
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_)) => {
                    capture
                        .read_from_device_to_deque(&mut bytes)
                        .context("read capture")?;
                }
                Err(e) => bail!("get_next_packet_size: {e}"),
            }
        }
        let whole = (bytes.len() / 8) * 8;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            for f in raw.chunks_exact(8) {
                let l = f32::from_le_bytes([f[0], f[1], f[2], f[3]]);
                let r = f32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                peak = peak.max(l.abs()).max(r.abs());
                if l.abs() > 0.01 {
                    first_signal.get_or_insert(frames);
                    last_signal = Some(frames);
                    let pos = l > 0.0;
                    if prev_positive.is_some_and(|p| p != pos) {
                        crossings += 1;
                    }
                    prev_positive = Some(pos);
                }
                frames += 1;
            }
        }
    }
    let _ = client.stop_stream();
    let est_hz = match (first_signal, last_signal) {
        (Some(a), Some(b)) if b > a + SAMPLE_RATE as u64 / 10 => {
            crossings as f32 / 2.0 / ((b - a) as f32 / SAMPLE_RATE as f32)
        }
        _ => 0.0,
    };
    println!(
        "audio-probe: {} read {} samples from {endpoint_id} (est {est_hz:.0} Hz)",
        if loopback { "loopback" } else { "record" },
        frames
    );
    Ok((peak, est_hz))
}

fn loopback_peak(endpoint_id: &str, seconds: u32) -> Result<(f32, f32)> {
    measure_peak(endpoint_id, seconds, true)
}

fn record_peak(endpoint_id: &str, seconds: u32) -> Result<(f32, f32)> {
    measure_peak(endpoint_id, seconds, false)
}

/// A fresh endpoint can steal either default. No-op if nothing moved.
fn restore_defaults(prev_render: Option<String>, prev_capture: Option<String>) {
    if let Some(prev) = prev_render {
        if audio_control::default_render_id().as_deref() != Some(prev.as_str()) {
            match audio_control::set_default_endpoint(&prev) {
                Ok(()) => println!("audio-probe: default playback restored"),
                Err(e) => println!("audio-probe: default playback not restored: {e:#}"),
            }
        }
    }
    if let Some(prev) = prev_capture {
        if audio_control::default_capture_id().as_deref() != Some(prev.as_str()) {
            match audio_control::set_default_endpoint(&prev) {
                Ok(()) => println!("audio-probe: default recording restored"),
                Err(e) => println!("audio-probe: default recording not restored: {e:#}"),
            }
        }
    }
}
