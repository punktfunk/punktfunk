//! The Windows-only devtests: virtual HID holds and probes, the audio-substrate toolbox, and
//! the pad-audio endpoint tool. `entry::subcommand` routes to them.

use anyhow::Result;

/// Hold a software-devnode HID Steam Deck (28DE:1205, device_type 3) and watch Steam Input
/// promote it. Signed driver + Steam running. `--seconds N` (default 120).
pub fn deck_windows_spike(args: &[String]) -> Result<()> {
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    crate::inject::dualsense_windows::deck_spike_hold(0, secs)
}

/// Hold the pf-mouse virtual HID pointer and sweep the cursor via HID reports.
///
/// Stack: devnode → INF → mshidumdf → mouhid → win32k. A resident pointer makes
/// `SM_MOUSEPRESENT` true so DWM composites the cursor with no dongle. Stop the host
/// service first — it owns the mailbox. `--seconds N` (default 30).
pub fn vmouse_spike(args: &[String]) -> Result<()> {
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    crate::inject::mouse_windows::spike_hold(secs)
}

/// Probe which HID IOCTL hidclass forwards to a UMDF HID minidriver.
///
/// Throwaway `pf_mouse_probe` at pad index 9 (safe beside a live host). Prints which
/// of the two HID paths answered. Drivers: `punktfunk-host.exe driver install --gamepad`.
pub fn channel_proof_probe(_args: &[String]) -> Result<()> {
    crate::inject::mouse_windows::channel_proof_probe()
}

/// Virtual DualSense via UMDF (`SwDeviceCreate` + shared-memory channel). No session.
///
/// `Get-PnpDevice` VID_054C. Exit closes the devnode. Same entry for DS4 / Xbox / Edge /
/// Deck / Triton via flags. `--idle-after` / `--resume-after` are Moonlight's change-only
/// cadence.
pub fn dualsense_windows_test(args: &[String]) -> Result<()> {
    use punktfunk_core::input::{GamepadEvent, GamepadFrame};
    use std::time::{Duration, Instant};
    let secs: u64 = args
        .iter()
        .skip_while(|a| *a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    // `--index N` → `pf_pad_N` (default 0). Use a spare if the host already holds 0.
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ds4 = args.iter().any(|a| a == "--ds4");
    let xbox = args.iter().any(|a| a == "--xbox");
    let xboxhid = args.iter().any(|a| a == "--xboxhid");
    // `--xboxones` / `--xboxelite`: same report, different VID/PID. `02FD` has no
    // stage-2 `HID\…&IG_00` in `xinputhid.inf` — watch promotion vs `0B13`.
    let xboxones = args.iter().any(|a| a == "--xboxones");
    let xboxelite = args.iter().any(|a| a == "--xboxelite");
    // `--edge`: paddles on pressed beats (report byte 10 = 0x80|0x40). `--deck`:
    // MI_02 Steam Deck. `--triton`: raw_len=0, so a stick sweep is the 0x42 fallback.
    let edge = args.iter().any(|a| a == "--edge");
    let deck = args.iter().any(|a| a == "--deck");
    // `--idle-after N`: stop State frames, keep pumping — Moonlight sends only on CHANGE.
    // Native never hits this: `input_task.rs` re-sends every pad every 100 ms.
    let idle_after: u64 = args
        .iter()
        .skip_while(|a| *a != "--idle-after")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `--resume-after M`: write State again. A listed pad after silence is not proof —
    // `win-input-matrix --watch` timestamps must advance.
    let resume_after: u64 = args
        .iter()
        .skip_while(|a| *a != "--resume-after")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let triton = args.iter().any(|a| a == "--triton");
    // `--xboxhid` presses Share (the Series pad's Consumer `Record` bit) on the same beats.
    let extra_buttons: u32 = if edge || deck || triton {
        punktfunk_core::input::gamepad::BTN_PADDLE1 | punktfunk_core::input::gamepad::BTN_PADDLE2
    } else if xboxhid {
        punktfunk_core::input::gamepad::BTN_MISC1
    } else {
        0
    };
    macro_rules! drive {
        ($mgr:expr, $label:expr) => {{
            let mut mgr = $mgr;
            mgr.handle(&GamepadEvent::Arrival {
                index: idx,
                kind: 2,
                capabilities: 0,
                audio_caps: 0,
            });
            if mgr.live_pads() == 0 {
                anyhow::bail!(
                    "no virtual {} was created at index {idx} — see the ERROR above for the \
                     cause. NOT measuring: any device answering on this index belongs to another \
                     process (a live session's pad), and reading it would look like a result.",
                    $label
                );
            }
            println!(
                "virtual {} up — cycling Cross + sweeping the left stick for {secs}s. Watch \
                 it in joy.cpl / Steam / a game; any feedback the game sends prints below.",
                $label
            );
            let deadline = Instant::now() + Duration::from_secs(secs);
            let started = Instant::now();
            let mut announced_silence = false;
            let mut announced_resume = false;
            let (mut i, mut last) = (0i32, Instant::now());
            while Instant::now() < deadline {
                mgr.pump(
                    |pad, lo, hi, lt, rt| println!(
                        "  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}"
                    ),
                    |o| println!("  hid output from game: {o:?}"),
                );
                let el = started.elapsed();
                let resumed =
                    resume_after != 0 && el >= Duration::from_secs(resume_after.max(idle_after));
                let silent =
                    idle_after != 0 && el >= Duration::from_secs(idle_after) && !resumed;
                if silent && !announced_silence {
                    announced_silence = true;
                    println!(
                        "  --- going SILENT (no more state frames, still pumping) at {}s ---",
                        idle_after
                    );
                }
                if resumed && !announced_resume {
                    announced_resume = true;
                    println!(
                        "  --- RESUMING state frames at {}s (after {}s of silence) ---",
                        resume_after,
                        resume_after.saturating_sub(idle_after)
                    );
                }
                if !silent && last.elapsed() >= Duration::from_millis(400) {
                    last = Instant::now();
                    i += 1;
                    let buttons = if i % 2 == 0 {
                        punktfunk_core::input::gamepad::BTN_A | extra_buttons
                    } else {
                        0
                    };
                    // Distinct phase per axis + opposing trigger ramps: a dead axis and an
                    // undriven one look the same; same-phase would hide crosstalk.
                    let phase = |off: i32| ((((i + off) % 64) - 32) * 1024) as i16;
                    let trig = ((i % 32) * 8).clamp(0, 255) as u8;
                    mgr.handle(&GamepadEvent::State(GamepadFrame {
                        index: idx as i16,
                        active_mask: 1 << idx,
                        buttons,
                        left_trigger: trig,
                        right_trigger: 255 - trig,
                        ls_x: phase(0),
                        ls_y: phase(16),
                        rs_x: phase(32),
                        rs_y: phase(48),
                    }));
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }};
    }
    if xbox {
        // XUSB: handle + pump_rumble, no HID-output plane — cannot use `drive!`.
        let mut mgr = crate::inject::gamepad::GamepadManager::new();
        mgr.handle(&GamepadEvent::Arrival {
            index: idx,
            kind: 1,
            capabilities: 0,
            audio_caps: 0,
        });
        if mgr.live_pads() == 0 {
            anyhow::bail!(
                "no virtual Xbox 360 (XUSB) was created at index {idx} — see the ERROR above. NOT \
                 measuring: a device answering on this index belongs to another process."
            );
        }
        println!(
            "virtual Xbox 360 (XUSB) up — sweeping LS + toggling A for {secs}s. Check with \
             an XInput game or xinputtest.exe."
        );
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut t = 0i32;
        while Instant::now() < deadline {
            // XUSB rumble has no trigger motors (`pump_rumble`); printed to match the HID line.
            mgr.pump_rumble(|pad, lo, hi, lt, rt| {
                println!("  rumble from game: pad={pad} low={lo} high={hi} lt={lt} rt={rt}")
            });
            t += 1;
            let lx = (((t % 200) - 100) * 327).clamp(-32768, 32767) as i16; // ±32700, just under i16 full scale
            let buttons = if (t / 67) % 2 == 0 {
                punktfunk_core::input::gamepad::BTN_A
            } else {
                0
            };
            mgr.handle(&GamepadEvent::State(GamepadFrame {
                index: idx as i16,
                active_mask: 1 << idx,
                buttons,
                left_trigger: 0,
                right_trigger: 0,
                ls_x: lx,
                ls_y: 0,
                rs_x: 0,
                rs_y: 0,
            }));
            std::thread::sleep(Duration::from_millis(15));
        }
    } else if xboxhid {
        // Shipping SwDeviceCreate identity, not devgen: a devgen HID child is
        // `HID\VID_045E&UP:0001_U:0005` with no PID, so Windows never promotes it.
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::new(),
            "Xbox Wireless Controller (HID)"
        );
    } else if xboxones {
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                crate::inject::xbox_windows::XboxWinProto::one_s()
            ),
            "Xbox One S Controller (HID, 045E:02FD)"
        );
    } else if xboxelite {
        drive!(
            crate::inject::xbox_windows::XboxWindowsManager::with_backend(
                crate::inject::xbox_windows::XboxWinProto::elite()
            ),
            "Xbox Elite Wireless Controller Series 2 (HID, 045E:0B22)"
        );
    } else if ds4 {
        drive!(
            crate::inject::dualshock4_windows::DualShock4WindowsManager::new(),
            "DualShock 4"
        );
    } else if edge {
        drive!(
            crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new(),
            "DualSense Edge"
        );
    } else if deck {
        drive!(
            crate::inject::steam_deck_windows::SteamDeckWindowsManager::new(),
            "Steam Deck"
        );
    } else if triton {
        drive!(
            crate::inject::triton_windows::TritonWindowsManager::new(),
            "Steam Controller 2"
        );
    } else {
        drive!(
            crate::inject::dualsense_windows::DualSenseWindowsManager::new(),
            "DualSense"
        );
    }
    println!("dualsense-windows-test: done (devnode removed)");
    Ok(())
}

/// Audio-substrate toolbox (`windows-audio-endpoints-and-vbcable.md`).
///
/// `audio-probe ssm|sink|sss-primary|mint|plan|cleanup [--keep]`. `ssm` mints a second
/// Steam Streaming Microphone and proves render→capture; `sink` parks default on a minted
/// Speakers and loopback-measures; `sss-primary` re-measures the primary Speakers;
/// `mint` runs the provider; `plan` prints one wiring pass and its readiness verdict.
pub fn audio_probe(args: &[String]) -> Result<()> {
    crate::audio::audio_probe::run(args)
}

/// Pad-audio endpoint: `ensure|remove|status|tone|capture|show|hide [--index N]`.
///
/// `ensure` is the startup path (reuse-or-create, bind Steam Streaming Speakers, stamp
/// DualSense 4ch/48k). `remove` is the pnputil escape hatch — endpoints persist.
/// Host parks them hidden with no client pad; `show` before `tone`/`capture`.
/// Stamping needs SYSTEM (MMDevices ACL): service account or PsExec.
pub fn pad_endpoint(args: &[String]) -> Result<()> {
    use crate::audio::pad_capture as pc;
    use crate::audio::pad_endpoint as pe;
    let idx: u8 = args
        .iter()
        .skip_while(|a| *a != "--index")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `--endpoint <id>`: any render device. Same binary on a known-good id separates
    // "this process cannot activate" from "our endpoint is broken".
    let endpoint_override: Option<String> = args
        .iter()
        .skip_while(|a| *a != "--endpoint")
        .nth(1)
        .cloned();
    match args.get(1).map(String::as_str) {
        Some("ensure") => {
            let p = pe::ensure(idx)?;
            println!(
                "pad-endpoint ensure: pad {} devnode {} endpoint {} needs_aeb_kick={}",
                p.pad_index, p.device_instance, p.endpoint_id, p.needs_aeb_kick
            );
            Ok(())
        }
        Some("remove") => match pe::find(idx)? {
            Some(p) => {
                pe::remove(&p);
                println!(
                    "pad-endpoint remove: requested removal of {}",
                    p.device_instance
                );
                Ok(())
            }
            None => {
                println!("pad-endpoint remove: no pad-audio devnode for index {idx}");
                Ok(())
            }
        },
        // Direct WASAPI render — a game launch cannot say which link in the chain broke.
        Some("tone") => {
            let secs: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let hz: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => {
                    // `find` is a system lookup. `endpoint_for` is the service's in-process
                    // cache; this CLI has none.
                    let Some(ep) = pe::find(idx)? else {
                        println!(
                            "pad-endpoint tone: no pad-audio devnode for pad {idx} — run \
                             `ensure` first"
                        );
                        return Ok(());
                    };
                    if ep.endpoint_id.is_empty() {
                        println!("pad-endpoint tone: pad {idx} has no endpoint id yet");
                        return Ok(());
                    }
                    ep.endpoint_id
                }
            };
            // `--pair front`: speaker pair, not voice coils. No game renders the speaker kind.
            let pair = args
                .iter()
                .skip_while(|a| *a != "--pair")
                .nth(1)
                .map_or(pc::TonePair::Back, |s| pc::TonePair::parse(s));
            println!(
                "pad-endpoint tone: {hz} Hz into the {} of {endpoint_id} for {secs}s",
                pair.label()
            );
            pc::render_test_tone(&endpoint_id, secs, hz, pair)?;
            println!(
                "pad-endpoint tone: done. A connected client with pad audio enabled should have \
                 buzzed; the host log shows whether the gate opened."
            );
            Ok(())
        }
        // Receiving half of `tone`. Run both to prove render → engine → loopback with no game.
        Some("capture") => {
            let secs: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => match pe::find(idx)? {
                    Some(ep) if !ep.endpoint_id.is_empty() => ep.endpoint_id,
                    _ => {
                        println!("pad-endpoint capture: pad {idx} has no endpoint — run `ensure`");
                        return Ok(());
                    }
                },
            };
            println!("pad-endpoint capture: listening on {endpoint_id} for {secs}s");
            pc::capture_probe(&endpoint_id, secs)
        }
        Some("status") => pe::print_status(idx),
        // `--channels N`: set the endpoint's device format through the policy API. 2 breaks
        // the pad's 4-channel graph on purpose, so `repair` can be watched putting it back.
        Some("reshape") => {
            let channels: u16 = args
                .iter()
                .skip_while(|a| *a != "--channels")
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(4);
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => match pe::find(idx)? {
                    Some(ep) if !ep.endpoint_id.is_empty() => ep.endpoint_id,
                    _ => {
                        println!("pad-endpoint reshape: pad {idx} has no endpoint — run `ensure`");
                        return Ok(());
                    }
                },
            };
            let mask = if channels == 4 {
                pc::PAD_CHANNEL_MASK
            } else {
                0x3
            };
            let samples = [
                (16, 16, wasapi::SampleType::Int),
                (32, 32, wasapi::SampleType::Float),
            ];
            crate::audio::audio_control::set_endpoint_format(
                &endpoint_id,
                channels,
                48_000,
                &[mask],
                &samples,
            )?;
            println!("pad-endpoint reshape: {endpoint_id} device format set to {channels} ch");
            Ok(())
        }
        // The startup ladder by hand: probe, policy-API reshape, re-mint, probe. `--remint`
        // takes the re-mint branch outright, on a healthy endpoint too.
        Some("repair") => match pe::find(idx)? {
            Some(p) if !p.endpoint_id.is_empty() => {
                let p = if args.iter().any(|a| a == "--remint") {
                    pe::remint(&p)?
                } else {
                    pe::validate(p, true)
                };
                println!(
                    "pad-endpoint repair: pad {} endpoint {} refuses_format={}",
                    p.pad_index,
                    p.endpoint_id,
                    pe::refuses_format(p.pad_index)
                );
                Ok(())
            }
            _ => {
                println!("pad-endpoint repair: pad {idx} has no endpoint — run `ensure`");
                Ok(())
            }
        },
        // DEVICE_STATE_DISABLED. Host parks pads hidden: idle libScePad titles stall on a
        // visible endpoint. `tone`/`capture` need it shown first.
        Some(verb @ ("show" | "hide")) => {
            let endpoint_id = match endpoint_override {
                Some(id) => id,
                None => match pe::find(idx)? {
                    Some(ep) if !ep.endpoint_id.is_empty() => ep.endpoint_id,
                    _ => {
                        println!("pad-endpoint {verb}: pad {idx} has no endpoint — run `ensure`");
                        return Ok(());
                    }
                },
            };
            pe::set_visibility(&endpoint_id, idx, verb == "show");
            println!("pad-endpoint {verb}: {endpoint_id}");
            Ok(())
        }
        _ => anyhow::bail!(
            "usage: punktfunk-host pad-endpoint \
             <ensure|remove|status|repair|reshape|tone|capture|show|hide> [--index N]"
        ),
    }
}
