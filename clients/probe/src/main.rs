//! Reference `punktfunk/1` client for protocol verification and stream diagnostics.
//!
//! A synthetic host enables deterministic frame checks; a virtual host writes encoded AUs and
//! reports capture-to-receive latency. Input, microphone, touch and rich-input flags exercise the
//! corresponding wire planes. Host audio and HID feedback are counted rather than rendered.
//!
//! `--pin HEX` pins the host certificate. `--pair -` reads a SPAKE2 PIN from stdin and prints the
//! verified fingerprint; literal PIN arguments are refused. `--discover` lists native mDNS hosts.
//!
//! Usage: `punktfunk-probe [--connect HOST:PORT] [--mode WxHxFPS] [--out FILE]
//! [--bitrate KBPS] [--codec auto|h264|hevc|av1]
//! [--input-test|--mic-test|--touch-test|--pad-audio-test [--pads N]]
//! [--pin HEX|--pair -] [--compositor NAME] [--gamepad NAME] [--trajectory FILE] |
//! --discover [SECS]`
#![forbid(unsafe_code)]

mod trajectory;

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::config::Role;
use punktfunk_core::fp::{hex, parse_hex32};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::FLAG_PROBE;
use punktfunk_core::quic::v2::dgram::Feedback;
use punktfunk_core::quic::v2::hello::{ClientHello, Ready, ServerHello};
use punktfunk_core::quic::v2::io as v2io;
use punktfunk_core::quic::v2::msg::{decode, V2Message};
use punktfunk_core::quic::{
    endpoint, window_report_ppm, BitrateChanged, CursorRenderMode, CursorShape, Hello,
    ProbeRequest, ProbeResult, Reconfigure, Reconfigured, SetBitrate, Welcome,
};
use punktfunk_core::session::test_frame;
use punktfunk_core::{CompositorPref, Mode, PunktfunkError, Session};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;

struct Args {
    connect: String,
    mode: Mode,
    out: Option<String>,
    input_test: bool,
    /// `--mic-test` — stream a synthetic 440 Hz tone as the mic uplink (proves the mic path).
    mic_test: bool,
    /// `--mic-burst` — pace the mic-test like a real client's input tap (2× 20 ms per 40 ms),
    /// the arrival shape that exercises host-side jitter buffering.
    mic_burst: bool,
    /// `--touch-test` — drag a synthetic finger in a circle (proves the touch path).
    touch_test: bool,
    /// `--rich-input-test` — drive the DualSense touchpad + motion over 0xCC (host needs
    /// `PUNKTFUNK_GAMEPAD=dualsense`); also logs the 0xCD HID-output feedback that comes back.
    rich_input_test: bool,
    /// `--pad-audio-test` — declare pads DualSenses that render haptics and speaker, and count
    /// the `0xD1` pad-audio datagrams the host streams back, per pad and lane.
    pad_audio_test: bool,
    /// `--pads N` — how many pads `--pad-audio-test` declares, 1 to 4.
    pads: u8,
    /// `--pad-soak SECS` — drive pad 0 with moving snapshots for SECS; RT carries a frame counter.
    pad_soak: Option<u64>,
    /// `--pad-hz N` — `--pad-soak` send rate, default 125.
    pad_hz: u32,
    /// `--quit` — close the connection with the deliberate-quit code (`QUIT_CLOSE_CODE`) at end of
    /// stream, so the host tears its virtual display down immediately (skips keep-alive linger). A
    /// bare exit closes with code 0 → the host lingers for a reconnect. Tests the #2 quit path.
    quit: bool,
    /// `--seconds N` — cap the receive loop at N seconds, then end the session gracefully (reach the
    /// `conn.close`). Without it the loop runs to the 120s cap. Lets a test bound a live-host stream so
    /// the client-initiated close (with/without `--quit`) fires promptly.
    seconds: Option<u64>,
    pin: Option<[u8; 32]>,
    /// `--remode WxHxFPS:SECS` — request this mode SECS seconds into the stream.
    remode: Option<(Mode, u32)>,
    /// `--rebitrate KBPS:SECS` — send a mid-stream [`SetBitrate`] (the adaptive-bitrate control
    /// message) SECS seconds into the stream: the headless validator for the host's in-place
    /// encoder rate retarget (Phase 3.2) / rebuild fallback. Wiggles the cursor around the switch
    /// so a damage-driven idle desktop actually publishes frames through it.
    rebitrate: Option<(u32, u32)>,
    /// `--link-kbps N` — send the link rate a real client's bring-up ramp would, so the
    /// host paces a pinned stream at that link rate. The probe runs no ramp of its own.
    link_kbps: Option<u32>,
    /// `--pair -` — run the pairing ceremony instead of a session.
    pair: Option<String>,
    /// `--name LABEL` — how the host labels this client when pairing.
    name: String,
    /// `--compositor NAME` — request a host compositor backend (auto|kwin|wlroots|mutter|gamescope|hyprland|niri|umbriel).
    compositor: CompositorPref,
    /// `--gamepad NAME` — request a host virtual-pad backend (auto|xbox360|dualsense).
    gamepad: GamepadPref,
    /// `--bitrate KBPS` — request this encoder bitrate (kilobits/s); 0 = host default.
    bitrate_kbps: u32,
    /// `--audio-channels N` — request stereo (2), 5.1 (6) or 7.1 (8) audio; default 2. The probe
    /// multistream-decodes the host's frames and asserts the per-channel sample count, so it's the
    /// headless validator for the surround encode path.
    audio_channels: u8,
    /// `--audio-format opus|lossless48|lossless96|<rate>/<bits>` — what to ask the host for on the
    /// audio plane. `opus` (default) sends the UNSPECIFIED sentinel and keeps the legacy `0xC9`
    /// wire; anything else asks for the lossless `0xD3` plane at that rate and depth. The rate set
    /// is [`punktfunk_core::audio::pcm::rate_is_supported`] — the probe does not restate it.
    audio_format: Option<(u32, u8)>,
    /// `--audio-out FILE` — write the DECODED audio to `FILE` as raw interleaved `f32` little-endian
    /// at the resolved rate and channel count, for offline analysis.
    ///
    /// This is what makes `design/hi-res-audio.md` §13.2 a command instead of a listening session:
    /// play a >24 kHz tone on the host, run the probe with `--audio-format lossless96 --audio-out`,
    /// and look for energy above 24 kHz in the result. A brick wall there means the host's capture
    /// resampled and the session is claiming a rate its content does not have.
    audio_out: Option<String>,
    /// `--codec h264|hevc|av1|auto` — the preferred video codec (soft; the host honors it when it can
    /// emit it, else falls back). The probe always advertises it can decode all three; this just sets
    /// the preference byte. `auto` (default) = no preference (host decides). `0` = auto.
    preferred_codec: u8,
    /// `--launch ID` — ask the host to launch a library title in this session (a store-qualified
    /// id from the host's `GET /api/v1/library`, e.g. `steam:570`). Host resolves it; `None` = none.
    launch: Option<String>,
    /// `--preset ID:NAME` — the settings preset this session names, as a client does.
    preset: Option<punktfunk_core::quic::SessionPreset>,
    /// `--speed-test KBPS:MS` — after the stream starts, ask the host for a `MS`-millisecond
    /// bandwidth probe burst at `KBPS`, then report measured throughput + loss.
    speed_test: Option<(u32, u32)>,
    /// `--cursor-capture` — negotiate the cursor channel (`CLIENT_CAP_CURSOR`) and immediately
    /// flip it to the capture model (`CursorRenderMode { client_draws: false }`), then wiggle the
    /// pointer with RELATIVE motion for the whole stream: the headless reproduction of a
    /// pointer-lock client expecting the HOST to composite the cursor into the video. Decode the
    /// dump and look for the pointer — a cursorless dump is the bug this flag was built to catch.
    cursor_capture: bool,
    /// `--cursor-nochannel` — the same relative wiggle WITHOUT the cursor channel: no
    /// `CLIENT_CAP_CURSOR`, no render-mode flip. The headless reproduction of a client LATCHED
    /// in capture mode at connect (`console.rs` `latched_mouse` — it never advertises the
    /// channel), the shape of the 2026-07 "no cursor in Mutter capture mode" field report. The
    /// host must composite the metadata cursor on its own; decode the dump and look for the
    /// pointer.
    cursor_nochannel: bool,
    /// `--cursor-channel` — negotiate the channel and leave the client drawing (a desktop
    /// client), flip to the capture model at 15 s and back at 35 s, wiggling throughout. The
    /// host's encoder must follow each flip.
    cursor_channel: bool,
    /// `--discover [SECS]` — browse the LAN for native (`_punktfunk._udp`) hosts for `SECS`
    /// seconds (default 4), print what's found, and exit. No connection is made.
    discover: Option<u64>,
    /// `--trajectory FILE` — stream on the shared client pump (the one the desktop and TV
    /// clients run) and write one JSON line per ABR report window, plus a summary carrying
    /// the link simulator's metrics. The netem rig's client half. `--bitrate` pins the
    /// session: the windows are still recorded, with no controller behind them.
    trajectory: Option<String>,
    /// `--link <achievable_kbps>[:<capacity_kbps>]` — what the shaped path is, for the two
    /// summary metrics a client cannot measure from inside its own session.
    link: trajectory::Link,
    /// `--profile NAME` — the row name the trajectory summary prints under.
    profile: String,
    /// `--as ID` — play as this profile. A host that places it elsewhere answers with a
    /// `Redirect`, which the probe follows once, as the client pump does: the address it names
    /// (else the same), its port, pinned to the certificate it names (else the same pin).
    as_profile: Option<String>,
    /// Set once a `Redirect` was followed; a second one on the same connect ends the run.
    redirected: bool,
    /// `--decoder-hold` — hold the picture after a lost frame until one that re-anchors it
    /// arrives, asking for a keyframe while held, as the shipped TV client does. Without it
    /// the rig resumes as soon as frame indexes line up, which no decoder can do.
    decoder_hold: bool,
    /// `--clock-resync` — after the connect-time skew handshake, immediately run a SECOND
    /// handshake on the same control stream and assert both estimates are sane and consistent:
    /// the headless validator for the host answering `ClockProbe` at any time (what the native
    /// clients' mid-stream re-sync relies on). Aborts the session when the re-probe fails.
    clock_resync: bool,
    /// `--screens N` (2–4) — ask for N screens of `--mode` side by side and count each one's
    /// frames; screen k's go to `<out>.s<stream>`. 1 asks for none.
    screens: u8,
}

/// The screens `--screens` asks for: `--mode` each, left to right; empty for one.
fn screens_ask(args: &Args) -> Vec<punktfunk_core::quic::v2::msg::Screen> {
    if args.screens < 2 {
        return Vec::new();
    }
    (0..args.screens)
        .map(|i| punktfunk_core::quic::v2::msg::Screen {
            index: i,
            mode: args.mode,
            x: i32::from(i) * args.mode.width as i32,
            primary: i == 0,
            ..Default::default()
        })
        .collect()
}

/// The control stream's halves; `punktfunk/2`'s cross the translation edges.
type CtlTx = quinn::SendStream;
type CtlRx = v2io::FrameReader<quinn::RecvStream>;

/// The connection, its datagrams as `punktfunk/1` encodings: on `punktfunk/2` they carry a kind.
#[derive(Clone)]
struct Wire {
    conn: quinn::Connection,
}

impl Wire {
    fn send_datagram(&self, d: Vec<u8>) -> Result<(), quinn::SendDatagramError> {
        match punktfunk_core::quic::v2::dgram::wrap(&d) {
            Some(w) => self.conn.send_datagram(w.into()),
            None => Ok(()),
        }
    }

    /// The next datagram; a kind the probe reads nothing from is skipped.
    async fn read_datagram(&self) -> Result<Vec<u8>, quinn::ConnectionError> {
        loop {
            let b = self.conn.read_datagram().await?;
            if let Some(p) = punktfunk_core::quic::v2::dgram::client_payload(&b) {
                return Ok(p.to_vec());
            }
        }
    }
}

impl std::ops::Deref for Wire {
    type Target = quinn::Connection;
    fn deref(&self) -> &quinn::Connection {
        &self.conn
    }
}

fn parse_mode(m: &str) -> Option<Mode> {
    let mut it = m.split('x');
    Some(Mode {
        width: it.next()?.parse().ok()?,
        height: it.next()?.parse().ok()?,
        refresh_hz: it.next()?.parse().ok()?,
    })
}

/// This probe's identity. The same files as the desktop client, including
/// `PUNKTFUNK_CONFIG_DIR`.
fn load_or_create_identity() -> Result<(String, String)> {
    pf_client_core::trust::load_or_create_identity()
}

fn parse_args() -> Args {
    parse_argv(std::env::args().collect())
}

fn parse_argv(argv: Vec<String>) -> Args {
    let get = |flag: &str| {
        argv.iter()
            .skip_while(|a| *a != flag)
            .nth(1)
            .map(String::as_str)
    };
    let mode = get("--mode").and_then(parse_mode).unwrap_or(Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    });
    let remode = get("--remode").and_then(|s| {
        let (m, secs) = s.split_once(':')?;
        Some((parse_mode(m)?, secs.parse().ok()?))
    });
    let rebitrate = get("--rebitrate").and_then(|s| {
        let (kbps, secs) = s.split_once(':')?;
        Some((kbps.parse().ok()?, secs.parse().ok()?))
    });
    // A present-but-malformed --pin must abort, not silently downgrade to trust-on-first-use
    // (the user asked for verification; fail closed).
    let pin = match get("--pin") {
        None => None,
        Some(s) => {
            match parse_hex32(s) {
                Some(p) => Some(p),
                None => {
                    eprintln!("--pin must be exactly 64 hex chars (the host logs its fingerprint at startup)");
                    std::process::exit(2);
                }
            }
        }
    };
    // A present-but-unrecognized --compositor must abort rather than silently auto-detect.
    let compositor = match get("--compositor") {
        None => CompositorPref::Auto,
        Some(s) => match CompositorPref::from_name(s) {
            Some(c) => c,
            None => {
                eprintln!("--compositor: auto, kwin, mutter, hyprland, niri, umbriel, wlroots or gamescope");
                std::process::exit(2);
            }
        },
    };
    // Same fail-closed discipline for --gamepad.
    let gamepad = match get("--gamepad") {
        None => GamepadPref::Auto,
        Some(s) => match GamepadPref::from_name(s) {
            Some(g) => g,
            None => {
                eprintln!(
                    "--gamepad must be one of: auto, xbox360, dualsense, xboxone, dualshock4"
                );
                std::process::exit(2);
            }
        },
    };
    Args {
        connect: get("--connect").unwrap_or("127.0.0.1:9777").to_string(),
        mode,
        out: get("--out").map(String::from),
        input_test: argv.iter().any(|a| a == "--input-test"),
        mic_test: argv.iter().any(|a| a == "--mic-test"),
        mic_burst: argv.iter().any(|a| a == "--mic-burst"),
        touch_test: argv.iter().any(|a| a == "--touch-test"),
        rich_input_test: argv.iter().any(|a| a == "--rich-input-test"),
        pad_audio_test: argv.iter().any(|a| a == "--pad-audio-test"),
        pads: get("--pads")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .clamp(1, PAD_AUDIO_PADS as u8),
        pad_soak: get("--pad-soak").and_then(|s| s.parse().ok()),
        pad_hz: get("--pad-hz")
            .and_then(|s| s.parse().ok())
            .unwrap_or(125)
            .clamp(1, 1000),
        quit: argv.iter().any(|a| a == "--quit"),
        seconds: get("--seconds").and_then(|s| s.parse().ok()),
        pin,
        remode,
        rebitrate,
        link_kbps: get("--link-kbps").and_then(|s| s.parse().ok()),
        pair: get("--pair").map(|value| {
            if value != "-" {
                eprintln!("a pairing PIN may not be passed in argv; pipe it to `--pair -`");
                std::process::exit(2);
            }
            value.to_string()
        }),
        name: get("--name").unwrap_or("punktfunk-probe").to_string(),
        compositor,
        gamepad,
        bitrate_kbps: get("--bitrate").and_then(|s| s.parse().ok()).unwrap_or(0),
        audio_channels: punktfunk_core::audio::normalize_channels(
            get("--audio-channels")
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
        ),
        audio_format: match get("--audio-format").unwrap_or("opus") {
            "opus" => None,
            "lossless48" => Some((punktfunk_core::audio::SAMPLE_RATE_HZ, 24)),
            "lossless96" => Some((96_000, 24)),
            spec => {
                let (r, b) = spec.split_once('/').unwrap_or((spec, "24"));
                match (r.parse::<u32>(), b.parse::<u8>()) {
                    (Ok(r), Ok(b))
                        if punktfunk_core::audio::pcm::rate_is_supported(r)
                            && punktfunk_core::audio::pcm::depth_is_supported(b) =>
                    {
                        Some((r, b))
                    }
                    _ => {
                        eprintln!(
                            "--audio-format: expected opus | lossless48 | lossless96 | \
                             <rate>/<bits>, got {spec:?}"
                        );
                        std::process::exit(2);
                    }
                }
            }
        },
        audio_out: get("--audio-out").map(String::from),
        preferred_codec: match get("--codec").unwrap_or("auto") {
            "h264" | "avc" => punktfunk_core::quic::CODEC_H264,
            "hevc" | "h265" => punktfunk_core::quic::CODEC_HEVC,
            "av1" => punktfunk_core::quic::CODEC_AV1,
            "pyrowave" => punktfunk_core::quic::CODEC_PYROWAVE,
            _ => 0, // auto — no preference
        },
        launch: get("--launch").map(str::to_string),
        preset: get("--preset").and_then(|s| {
            let (id, name) = s.split_once(':').unwrap_or((s, ""));
            punktfunk_core::quic::SessionPreset::new(id, name)
        }),
        speed_test: get("--speed-test").and_then(|s| {
            let (kbps, ms) = s.split_once(':')?;
            Some((kbps.parse().ok()?, ms.parse().ok()?))
        }),
        // `--discover` may be a bare flag or carry a seconds value (`--discover 8`); only treat
        // the following token as a count when it parses as a number (else it's the next flag).
        discover: argv
            .iter()
            .any(|a| a == "--discover")
            .then(|| get("--discover").and_then(|s| s.parse().ok()).unwrap_or(4)),
        trajectory: get("--trajectory").map(String::from),
        link: match get("--link") {
            None => trajectory::Link::default(),
            Some(s) => match trajectory::Link::parse(s) {
                Some(l) => l,
                None => {
                    eprintln!("--link takes <achievable_kbps>[:<capacity_kbps>], both numbers");
                    std::process::exit(2);
                }
            },
        },
        profile: get("--profile").unwrap_or("rig").to_string(),
        as_profile: get("--as").map(str::to_string),
        redirected: false,
        decoder_hold: argv.iter().any(|a| a == "--decoder-hold"),
        clock_resync: argv.iter().any(|a| a == "--clock-resync"),
        cursor_capture: argv.iter().any(|a| a == "--cursor-capture"),
        cursor_nochannel: argv.iter().any(|a| a == "--cursor-nochannel"),
        cursor_channel: argv.iter().any(|a| a == "--cursor-channel"),
        screens: get("--screens")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(1)
            .clamp(1, punktfunk_core::quic::v2::msg::SCREENS_MAX as u8),
    }
}

/// Human name for the negotiated `Welcome::codec` (also the natural `--out` file extension). The
/// bitstream is dumped verbatim, so an H.264 software-host session should be saved as `.h264`.
fn codec_ext(codec: u8) -> &'static str {
    match codec {
        punktfunk_core::quic::CODEC_H264 => "h264",
        punktfunk_core::quic::CODEC_AV1 => "av1",
        punktfunk_core::quic::CODEC_PYROWAVE => "pyrowave",
        _ => "h265",
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = parse_args();
    if let Err(e) = run(args) {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    // Discovery mode: browse the LAN for native hosts, print them, and exit (no connection).
    if let Some(secs) = args.discover {
        return discover(secs);
    }
    // Pairing mode: run the PIN ceremony and print the fingerprint to pin, then exit.
    if args.pair.is_some() {
        let (host, port) = args
            .connect
            .rsplit_once(':')
            .context("--connect host:port")?;
        let mut pin = String::new();
        std::io::stdin().read_line(&mut pin).context("read PIN")?;
        let pin = pin.trim();
        anyhow::ensure!(!pin.is_empty(), "no PIN on stdin");
        let identity = load_or_create_identity()?;
        let fp = punktfunk_core::client::NativeClient::pair(
            host,
            port.parse().context("port")?,
            (&identity.0, &identity.1),
            pin,
            &args.name,
            std::time::Duration::from_secs(90),
        )
        .map_err(|e| anyhow!("pairing failed: {e:?} (wrong PIN?)"))?;
        tracing::info!(
            fingerprint = %hex(&fp),
            "PAIRED — connect with --pin {} from now on",
            hex(&fp)
        );
        return Ok(());
    }
    // Trajectory mode: the shared pump runs its own runtime and its own controller.
    if let Some(path) = args.trajectory.as_deref() {
        return trajectory::run(
            &args.connect,
            args.mode,
            args.pin,
            load_or_create_identity().ok(),
            &args.name,
            args.seconds.unwrap_or(60),
            path,
            args.link,
            &args.profile,
            args.decoder_hold,
            args.bitrate_kbps,
            args.preferred_codec,
        );
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(session(args))
}

/// Browse the LAN for native (`_punktfunk._udp`) hosts for `secs` seconds and print them, then
/// exit — the discovery side of the host's mDNS advert (host crate `discovery.rs`). TXT keys:
/// `fp` (host cert fingerprint to pin), `pair` (required|optional), `id` (stable host id),
/// `wire` (the protocols the host answers).
fn discover(secs: u64) -> Result<()> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let receiver = daemon
        .browse("_punktfunk._udp.local.")
        .context("browse _punktfunk._udp")?;
    tracing::info!(
        secs,
        "browsing for native punktfunk/1 hosts (_punktfunk._udp)…"
    );
    // One row per host, keyed by the stable uniqueid (falls back to the fullname) so the same
    // host re-advertising or answering on several interfaces collapses to a single entry.
    let mut hosts: BTreeMap<String, String> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Timeout == time left to the deadline: an event returns immediately, otherwise the recv
        // returns Err exactly at the deadline (or if the daemon channel closes) and we stop.
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let props = info.get_properties();
                let val = |k: &str| props.get_property_val_str(k).unwrap_or("").to_string();
                let addr = info
                    .get_addresses()
                    .iter()
                    .next()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "?".into());
                let fp = val("fp");
                let fp_short = fp.get(..16).unwrap_or(fp.as_str());
                let name = info
                    .get_fullname()
                    .split('.')
                    .next()
                    .unwrap_or("?")
                    .to_string();
                let id = val("id");
                let key = if id.is_empty() {
                    info.get_fullname().to_string()
                } else {
                    id
                };
                let row = format!(
                    "  {name:<24} {addr}:{:<6} pair={:<9} wire={:<4} fp={fp_short}…",
                    info.get_port(),
                    val("pair"),
                    val("wire"),
                );
                hosts.insert(key, row);
            }
            Ok(_) => {} // SearchStarted / ServiceFound / removals — ignore
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    if hosts.is_empty() {
        println!("no native punktfunk/1 hosts found on the LAN ({secs}s)");
    } else {
        println!("native punktfunk/1 hosts ({}):", hosts.len());
        for row in hosts.values() {
            println!("{row}");
        }
        println!("\nconnect with: punktfunk-probe --connect <addr:port> [--pin <fp> | --pair -]");
    }
    Ok(())
}

/// Pads `--pad-audio-test` can declare and count.
const PAD_AUDIO_PADS: usize = 4;

/// Counters the planes share. The data loop mirrors the session's figures into the first four
/// for the control tasks; the datagram task counts the host→client planes for the report.
#[derive(Default)]
struct Counters {
    /// Packet-level receive counters. The speed test reads their delta over the burst window, so
    /// throughput and loss count every delivered wire packet, not just reassembled probe AUs.
    rx_wire_packets: AtomicU64,
    rx_wire_bytes: AtomicU64,
    /// The windowed loss estimate the loss relay sends in its feedback window, so the host sizes
    /// FEC to the link. `u32::MAX` = no fresh sample this window.
    loss_ppm: AtomicU32,
    /// What the window's dead frames add to `loss_ppm`, sent beside it.
    dead_ppm: AtomicU32,
    /// The session's cumulative unrecoverable-frame count. The loss relay requests a keyframe
    /// when it grows, the loss trigger under infinite GOP (see `NativeClient::frames_dropped`).
    dropped_frames: AtomicU64,
    audio_pkts: AtomicU64,
    audio_bytes: AtomicU64,
    rumble_pkts: AtomicU64,
    hidout_pkts: AtomicU64,
    /// `0xD1` pad-audio datagrams by pad and lane (haptics, speaker), and those carrying sound.
    pad_audio_pkts: [[AtomicU64; 2]; PAD_AUDIO_PADS],
    pad_audio_voiced: AtomicU64,
    /// A self-terminating v2 rumble envelope (0xCA with the seq+ttl tail) arrived. Asserted at
    /// report time under `PUNKTFUNK_TEST_FEEDBACK`; a legacy v1 datagram leaves it false.
    saw_v2_rumble: AtomicBool,
    /// The connection closed; the blocking receive loop stops once the tail has drained.
    closed: AtomicBool,
}

/// The host placed this profile on another host of the box.
#[derive(Debug)]
struct Redirected(punktfunk_core::quic::v2::msg::Redirect);

impl std::fmt::Display for Redirected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the host placed this profile on port {}", self.0.port)
    }
}

impl std::error::Error for Redirected {}

/// Points `args` at the host a `Redirect` names, once per connect.
fn follow(args: &mut Args, to: &punktfunk_core::quic::v2::msg::Redirect) -> Result<()> {
    anyhow::ensure!(!args.redirected, "redirected twice on one connect");
    let mut remote: std::net::SocketAddr = args.connect.parse().context("--connect host:port")?;
    if !to.addr.is_empty() {
        remote.set_ip(to.addr.parse().context("the Redirect's address")?);
    }
    remote.set_port(to.port);
    args.connect = remote.to_string();
    if !to.pin.is_empty() {
        args.pin = punktfunk_core::fp::parse_hex32(&to.pin);
    }
    args.redirected = true;
    Ok(())
}

async fn session(mut args: Args) -> Result<()> {
    let (ep, quic, shared) = connect(&args).await?;
    let conn = Wire { conn: quic.clone() };
    let (mut send, recv) = conn.open_bi().await.context("open control stream")?;
    // Frame every read on the control stream through the resumable reader, exactly as the client
    // pump does: `clock_sync` bounds each read with a timeout, and a frame straddling two wakeups
    // would otherwise leave the stream permanently misaligned for the rest of the run.
    v2io::write_stream_type(
        &mut send,
        punktfunk_core::quic::v2::registry::STREAM_CONTROL,
    )
    .await?;
    let mut recv: CtlRx = v2io::FrameReader::new(recv);
    let extra = Extra {
        resume: None,
        suites: if std::env::var_os("PUNKTFUNK_CLIENT_CHACHA20").is_some() {
            vec![
                punktfunk_core::crypto::MediaSuite::ChaCha20Poly1305,
                punktfunk_core::crypto::MediaSuite::Aes128Gcm,
            ]
        } else {
            vec![punktfunk_core::crypto::MediaSuite::Aes128Gcm]
        },
    };
    let server = match handshake(&mut send, &mut recv, &args, extra).await {
        Ok(server) => server,
        Err(e) => match e.downcast::<Redirected>() {
            Ok(Redirected(to)) => {
                follow(&mut args, &to)?;
                quic.close(0u32.into(), b"redirected");
                return Box::pin(session(args)).await;
            }
            Err(e) => return Err(e),
        },
    };
    // Screens past the first: their keyframe asks leave on this connection.
    let screen_asks = server
        .features
        .has(punktfunk_core::quic::v2::registry::FEATURE_SCREENS)
        .then(|| quic.clone());
    let welcome = server.welcome;
    // Media on the dialing socket, under keys from the connection's exporter.
    let suite = server.suite.context("unsealed native media")?;
    let keys =
        endpoint::media_keys(&quic, &server.session_id, suite).context("native media keys")?;
    shared.set_host(quic.remote_address());
    let media = (
        shared,
        punktfunk_core::session::MediaV2 {
            clock_origin_ns: server.clock_origin_ns,
            keys: Some(keys),
            clock: None,
            clear_placement: false,
        },
    );
    let clock_offset_ns = clock(&mut send, &mut recv, args.clock_resync).await?;
    let counters = Arc::new(Counters {
        loss_ppm: AtomicU32::new(u32::MAX),
        ..Counters::default()
    });
    control_plane(&args, &conn, send, recv, &counters).await?;
    if args.input_test {
        spawn_input_test(&conn, args.mode);
    }
    if args.mic_test {
        spawn_mic_test(&conn, args.mic_burst);
    }
    if args.touch_test {
        spawn_touch_test(&conn, args.mode);
    }
    if args.rich_input_test {
        spawn_rich_input_test(&conn);
    }
    if args.pad_audio_test {
        spawn_pad_audio_test(&conn, args.pads);
    }
    if let Some(secs) = args.pad_soak {
        spawn_pad_soak(&conn, secs, args.pad_hz);
    }
    // Closed-flag for the blocking receive loop.
    {
        let counters = counters.clone();
        let conn2 = conn.clone();
        tokio::spawn(async move {
            conn2.closed().await;
            counters
                .closed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    // Per-AU host timings (0xCF) → the stream loop, which matches them to received AUs by pts
    // and reports the host/network split. try_send: overflow drops samples, never blocks QUIC.
    let (host_timing_tx, host_timing_rx) =
        std::sync::mpsc::sync_channel::<punktfunk_core::quic::HostTiming>(512);
    spawn_datagram_rx(
        &conn,
        &welcome,
        args.audio_out.clone(),
        &counters,
        host_timing_tx,
    );

    let (out_path, seconds, dp_counters) = (args.out.clone(), args.seconds, counters.clone());
    let result = tokio::task::spawn_blocking(move || {
        data_plane(
            welcome,
            media,
            out_path,
            seconds,
            clock_offset_ns,
            dp_counters,
            host_timing_rx,
            screen_asks,
        )
    })
    .await?;
    report_side_planes(&counters);

    // Under PUNKTFUNK_TEST_FEEDBACK (a loopback harness scripts a feedback burst), a run with no
    // v2 rumble tail (seq + TTL) fails: a host back on v1 level datagrams bumps the rumble counter
    // identically. Only tightens an otherwise-OK run; a video failure stays the primary error.
    let result = if std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
        && result.is_ok()
        && !counters
            .saw_v2_rumble
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        Err(anyhow::anyhow!(
            "PUNKTFUNK_TEST_FEEDBACK: expected a v2 rumble envelope (0xCA seq+ttl tail), received none"
        ))
    } else {
        result
    };

    // `--quit` closes with the deliberate-quit code so the host skips the keep-alive linger; a normal
    // exit uses code 0 (an unwanted-disconnect close → the host lingers for a reconnect).
    let close_code = if args.quit {
        punktfunk_core::quic::QUIT_CLOSE_CODE
    } else {
        0
    };
    conn.close(close_code.into(), b"done");
    // Flush the CONNECTION_CLOSE frame before we exit: without this the process can drop the endpoint
    // before quinn sends the close, so the host waits out the idle timeout instead of seeing the close
    // CODE promptly (deliberate-quit vs. code 0). Bounded so a stuck flush can't hang the probe.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ep.wait_idle()).await;
    result
}

/// Dials `--connect`, pinned to `--pin` when given, and logs how the host was trusted.
async fn connect(
    args: &Args,
) -> Result<(
    quinn::Endpoint,
    quinn::Connection,
    punktfunk_core::transport::shared::ClientMedia,
)> {
    let remote: std::net::SocketAddr = args.connect.parse().context("--connect host:port")?;
    let identity = load_or_create_identity()?;
    let identity = Some((identity.0.as_str(), identity.1.as_str()));
    // QUIC and media on one socket, as the clients dial.
    let (r, observed) = endpoint::client_shared(
        args.pin,
        identity,
        &[punktfunk_core::quic::v2::registry::ALPN],
        Some(remote.ip()),
    );
    let (ep, media) = r.map_err(|e| anyhow!("QUIC client endpoint: {e}"))?;
    let conn = ep
        .connect(remote, "punktfunk")
        .context("connect")?
        .await
        .context("QUIC handshake (a pin mismatch fails here)")?;
    match (args.pin, *observed.lock().unwrap()) {
        (Some(_), _) => tracing::info!(%remote, "connected — host fingerprint pinned"),
        (None, Some(fp)) => tracing::info!(
            %remote,
            fingerprint = %hex(&fp),
            "connected (trust-on-first-use) — pass --pin to verify this host"
        ),
        (None, None) => tracing::info!(%remote, "connected"),
    }
    Ok((ep, conn, media))
}

/// What the `ClientHello` carries beyond the `Hello`.
struct Extra {
    resume: Option<[u8; 16]>,
    suites: Vec<punktfunk_core::crypto::MediaSuite>,
}

/// `ClientHello` out and `ServerHello` back, then `Ready`.
async fn handshake(
    send: &mut CtlTx,
    recv: &mut CtlRx,
    args: &Args,
    extra: Extra,
) -> Result<ServerHello> {
    let hello = ClientHello {
        hello: Hello {
            mode: args.mode,
            compositor: args.compositor,
            gamepad: args.gamepad,
            bitrate_kbps: args.bitrate_kbps,
            // `--name` (also the pairing label) — shown in the host's pending-approval list when
            // this client knocks on a pairing-required host.
            name: Some(args.name.clone()),
            // `--launch ID` — host resolves it against its own library and runs it this session.
            launch: args.launch.clone(),
            // This headless tool just dumps the bitstream (no decode), so it can always claim
            // 10-bit / 4:4:4 support. Gated by env so latency runs stay on the 8-bit 4:2:0 baseline:
            // PUNKTFUNK_CLIENT_10BIT=1 advertises VIDEO_CAP_10BIT (host Main10 path);
            // PUNKTFUNK_CLIENT_444=1 advertises VIDEO_CAP_444 (host HEVC 4:4:4 path) — verify the
            // resulting chroma with `ffprobe` on the `--out` .h265.
            video_caps: {
                // Always ask for per-AU host timings (0xCF) — this is a measurement tool, and the
                // host/network split is exactly what it exists to report. Old hosts ignore the bit.
                // PROBE_SEQ: the shared-core reassembler windows probe-space frames, so the probe
                // qualifies for `--speed-test` bursts; without the bit the host declines them.
                // STREAMED_AU: the same shared reassembler accepts sentinel-headed streamed
                // blocks, and the probe is exactly the tool that measures the overlap win.
                let mut caps = punktfunk_core::quic::VIDEO_CAP_HOST_TIMING
                    | punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ
                    | punktfunk_core::quic::VIDEO_CAP_STREAMED_AU;
                if std::env::var_os("PUNKTFUNK_CLIENT_10BIT").is_some() {
                    caps |= punktfunk_core::quic::VIDEO_CAP_10BIT;
                }
                if std::env::var_os("PUNKTFUNK_CLIENT_444").is_some() {
                    caps |= punktfunk_core::quic::VIDEO_CAP_444;
                }
                // PUNKTFUNK_CLIENT_HDR=1 advertises VIDEO_CAP_HDR (BT.2020 PQ), and 10-bit
                // with it — the host needs both for an HDR session. Pairs with
                // PUNKTFUNK_CLIENT_PEAK_NITS, which carries the panel's ST.2086 volume.
                if std::env::var_os("PUNKTFUNK_CLIENT_HDR").is_some() {
                    caps |=
                        punktfunk_core::quic::VIDEO_CAP_HDR | punktfunk_core::quic::VIDEO_CAP_10BIT;
                }
                // PUNKTFUNK_CLIENT_CHACHA20=1 advertises VIDEO_CAP_CHACHA20 — drives the
                // host's ChaCha20-Poly1305 session-cipher resolution (the soft-AES armv7
                // negotiation, design/chacha20-session-cipher.md §7) without a webOS build;
                // the negotiated cipher is reported in the welcome log line below.
                if std::env::var_os("PUNKTFUNK_CLIENT_CHACHA20").is_some() {
                    caps |= punktfunk_core::quic::VIDEO_CAP_CHACHA20;
                }
                caps
            },
            // `--audio-channels` (default stereo); the probe multistream-decodes + validates the
            // host's frames to exercise the surround encode path headlessly.
            audio_channels: args.audio_channels,
            // Legacy coupling; the probe decodes whatever the host answers (`Welcome::audio_layout`).
            audio_layout: 0,
            // The probe dumps the frame as sent; no view to fill.
            video_fit: 0,
            // No receiver behind the probe: it decodes Opus itself.
            audio_bitstreams: 0,
            // The probe just dumps the bitstream (no decode), so it advertises every codec — HEVC
            // (the host default) AND H.264 (so it can drive a GPU-less software host,
            // `PUNKTFUNK_ENCODER=software`) AND AV1. The host picks one and reports it in
            // `Welcome::codec`; the dump extension follows that.
            video_codecs: punktfunk_core::quic::CODEC_H264
                | punktfunk_core::quic::CODEC_HEVC
                | punktfunk_core::quic::CODEC_AV1
                // PyroWave only on request: a PyroWave-default host would otherwise pick it
                // for every probe run, changing the dump format under long-standing recipes.
                | if args.preferred_codec == punktfunk_core::quic::CODEC_PYROWAVE {
                    punktfunk_core::quic::CODEC_PYROWAVE
                } else {
                    0
                },
            // `--codec` soft preference (0 = auto). The host honors it when it can emit it.
            preferred_codec: args.preferred_codec,
            // PUNKTFUNK_CLIENT_PEAK_NITS=<nits> advertises a synthetic display volume — the host
            // writes it into the virtual display's EDID (CTA HDR block), so the EDID-forwarding
            // path can be validated headlessly (check the host's monitor caps / ADD log line).
            display_hdr: punktfunk_core::client::display_hdr_env_override(),
            // No CLIENT_CAP_CURSOR by default: this headless tool renders nothing — advertising
            // it would just strip the pointer from the dumped bitstream. `--cursor-capture`
            // advertises it deliberately and then flips the channel to the capture model, so the
            // HOST composites and the dump is where the pointer must appear.
            client_caps: {
                let mut c = if args.cursor_capture || args.cursor_channel {
                    punktfunk_core::quic::CLIENT_CAP_CURSOR
                } else {
                    0
                };
                // ⚠ This `Hello` is built BY HAND, so it never passes through
                // `client::advertised_client_caps` — the helper that derives this bit from the
                // requested format for the shipping clients. The rate and depth below are
                // therefore inert on their own: the host's gate checks the CAPABILITY first, so a
                // format without this bit is a request the host is right to ignore. Set both, or
                // neither.
                if args.audio_format.is_some() {
                    c |= punktfunk_core::quic::CLIENT_CAP_AUDIO_HIRES;
                }
                if args.pad_audio_test {
                    c |= punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO;
                }
                c
            },
            // Like STREAMED_AU above: the shared-core reassembler pins geometry per-frame, so
            // the probe accepts a mid-session shard change (and jumbo growth) up to the
            // receive ceiling — and it's exactly the tool to measure both.
            max_shard_payload: punktfunk_core::config::max_shard_payload() as u16,
            // `0`/`0` is UNSPECIFIED. This hand-built `Hello` sets the cap bit and the format
            // together, so no inert format reaches the wire to read as a request.
            audio_rate_hz: args.audio_format.map(|(r, _)| r).unwrap_or(0),
            audio_bits: args.audio_format.map(|(_, b)| b).unwrap_or(0),
        },
        client_label: None,
        abr_features: 0,
        preset: args.preset.clone(),
        link: Default::default(),
        probe_only: false,
        pyrowave_bpp_x100: 0,
        slices: 0,
        resume: extra.resume,
        suites: extra.suites,
        // A host answers a `Redirect` only to a client that says it follows one.
        features: {
            use punktfunk_core::quic::v2::registry::{FEATURE_PROFILES, FEATURE_SCREENS};
            let mut set = punktfunk_core::quic::v2::features::FeatureSet::default();
            if args.as_profile.is_some() {
                set = set.with(FEATURE_PROFILES);
            }
            if args.screens > 1 {
                set = set.with(FEATURE_SCREENS);
            }
            set
        },
        profile: args.as_profile.clone(),
        access_only: false,
        screens: screens_ask(args),
    };
    v2io::send(send, &hello).await?;
    // `Pending` repeats while the host asks its console about this probe.
    let server = loop {
        let (ty, body) = recv.read_frame().await?;
        match ty {
            punktfunk_core::quic::v2::msg::Redirect::TYPE => {
                let to = punktfunk_core::quic::v2::msg::Redirect::from_body(&body)
                    .map_err(|e| anyhow!("Redirect decode: {e:?}"))?;
                println!(
                    "redirect addr={:?} port={} profile={} seat_no={} seat_name={:?} occupant={:?} pin={}",
                    to.addr, to.port, to.profile, to.seat_no, to.seat_name, to.occupant, to.pin
                );
                return Err(anyhow::Error::new(Redirected(to)));
            }
            ServerHello::TYPE => {
                break ServerHello::from_body(&body)
                    .map_err(|e| anyhow!("ServerHello decode: {e:?}"))?
            }
            punktfunk_core::quic::v2::registry::MSG_PENDING => {
                tracing::info!("the host is waiting for this probe to be approved")
            }
            _ => {}
        }
    };
    let welcome = &server.welcome;
    tracing::info!(
        mode = ?welcome.mode,
        fec = ?welcome.fec,
        frames = welcome.frames,
        compositor = welcome.compositor.as_str(),
        gamepad = welcome.gamepad.as_str(),
        bit_depth = welcome.bit_depth,
        color = ?welcome.color,
        hdr = welcome.color.is_hdr(),
        chroma_444 = welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
        chroma_format_idc = welcome.chroma_format,
        codec = codec_ext(welcome.codec),
        suite = ?server.suite,
        "session offer"
    );

    v2io::send(send, &Ready {}).await?;
    Ok(server)
}

/// The wall-clock skew handshake, plus the `--clock-resync` re-probe. `None` is an old host
/// that does not answer.
async fn clock(send: &mut CtlTx, recv: &mut CtlRx, resync: bool) -> Result<Option<i64>> {
    // Wall-clock skew handshake on the still-private control stream (before --remode/--speed-test
    // take it): align our clock to the host's so the per-frame capture→received latency is valid
    // across machines. `None` ⇒ an old host that doesn't answer — fall back to a shared clock (0).
    let first_skew = punktfunk_core::quic::clock_sync(send, recv).await;
    let clock_offset_ns = match &first_skew {
        Some(skew) => {
            tracing::info!(
                offset_ns = skew.offset_ns,
                rtt_us = skew.rtt_ns / 1000,
                rounds = skew.rounds,
                "clock skew estimated (host-client); latency now cross-machine valid"
            );
            Some(skew.offset_ns)
        }
        None => None,
    };

    // `--clock-resync`: prove the host answers `ClockProbe` mid-session, not just at connect —
    // the contract the native clients' mid-stream re-sync rests on. Run a full second handshake
    // and require a sane, consistent estimate: both batches measure the same physical skew, so
    // they must agree to within RTT-scale error (the handshake's own uncertainty is ≈ RTT/2).
    if resync {
        let first = first_skew.as_ref().ok_or_else(|| {
            anyhow!("clock-resync: host never answered the connect-time handshake")
        })?;
        let second = punktfunk_core::quic::clock_sync(send, recv)
            .await
            .ok_or_else(|| anyhow!("clock-resync: host did not answer the re-probe"))?;
        let disagree_ns = (second.offset_ns - first.offset_ns).unsigned_abs();
        let bound_ns = (first.rtt_ns + second.rtt_ns).max(2_000_000);
        tracing::info!(
            first_offset_ns = first.offset_ns,
            second_offset_ns = second.offset_ns,
            disagree_us = disagree_ns / 1000,
            bound_us = bound_ns / 1000,
            second_rtt_us = second.rtt_ns / 1000,
            rounds = second.rounds,
            "clock re-probe answered"
        );
        if second.rounds < 8 || disagree_ns > bound_ns {
            return Err(anyhow!(
                "clock-resync: re-probe unsound (rounds {}, disagreement {} µs > bound {} µs)",
                second.rounds,
                disagree_ns / 1000,
                bound_ns / 1000
            ));
        }
        println!(
            "clock-resync OK: offsets {} / {} ns",
            first.offset_ns, second.offset_ns
        );
    }
    Ok(clock_offset_ns)
}

/// Hands the control stream to the one test that owns it, or else to the loss relay a real
/// client runs. The tests are mutually exclusive: each needs the stream to itself.
async fn control_plane(
    args: &Args,
    conn: &Wire,
    send: CtlTx,
    recv: CtlRx,
    counters: &Arc<Counters>,
) -> Result<()> {
    let link_kbps = args.link_kbps.unwrap_or(0);
    if link_kbps != 0 {
        let fb = Feedback {
            link_kbps,
            ..Default::default()
        };
        conn.conn
            .send_datagram(fb.encode().into())
            .map_err(|e| anyhow!("link rate datagram: {e}"))?;
        tracing::info!(link_kbps, "sent the link rate");
    }
    if let Some((new_mode, after_secs)) = args.remode {
        spawn_remode(send, recv, new_mode, after_secs);
    } else if let Some((new_kbps, after_secs)) = args.rebitrate {
        spawn_rebitrate(conn, send, recv, new_kbps, after_secs);
    } else if let Some(test) = args.speed_test {
        spawn_speed_test(conn, send, recv, test, counters);
    } else if args.cursor_capture || args.cursor_nochannel || args.cursor_channel {
        spawn_cursor_test(conn, send, recv, args.cursor_capture, args.cursor_channel);
    } else {
        spawn_loss_relay(conn.conn.clone(), send, counters, link_kbps);
        spawn_screen_log(recv);
    }
    Ok(())
}

/// Log what the host says about screens past the first: each layout and each stream's config.
fn spawn_screen_log(mut recv: CtlRx) {
    use punktfunk_core::quic::v2::msg::{ScreenLayout, StreamConfig};
    tokio::spawn(async move {
        while let Ok((ty, body)) = recv.read_frame().await {
            if let Ok(layout) = decode::<ScreenLayout>(ty, &body) {
                for s in &layout.screens {
                    tracing::info!(
                        screen = s.index,
                        stream = s.stream,
                        mode = %format!("{}x{}@{}", s.mode.width, s.mode.height, s.mode.refresh_hz),
                        x = s.x,
                        y = s.y,
                        "host screen layout"
                    );
                }
            } else if let Some(cfg) = decode::<StreamConfig>(ty, &body)
                .ok()
                .filter(|c| c.stream != 0)
            {
                tracing::info!(stream = cfg.stream, epoch = cfg.epoch, mode = ?cfg.mode, codec = cfg.codec, "screen stream config");
            }
        }
    });
}

/// `--remode`: after a delay, ask the host to switch modes on the still-open control stream.
/// The stream then carries new-mode AUs (IDR + in-band parameter sets); ffprobe the `--out`
/// file to see both resolutions.
/// The next control frame of type `M`. The host also sends a `StreamConfig` and its notices
/// unasked, so an answer is not always the next frame.
async fn next_of<M: V2Message>(r: &mut CtlRx) -> std::io::Result<M> {
    loop {
        let (ty, body) = r.read_frame().await?;
        if let Ok(m) = decode::<M>(ty, &body) {
            return Ok(m);
        }
    }
}

fn spawn_remode(mut rs: CtlTx, mut rr: CtlRx, new_mode: Mode, after_secs: u32) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(after_secs as u64)).await;
        tracing::info!(?new_mode, "requesting mid-stream mode switch");
        let ask = Reconfigure {
            mode: new_mode,
            exclude_codecs: 0,
        };
        if v2io::send(&mut rs, &ask).await.is_err() {
            tracing::error!("Reconfigure write failed");
            return;
        }
        match next_of::<Reconfigured>(&mut rr).await {
            Ok(ack) if ack.accepted => {
                tracing::info!(mode = ?ack.mode, "mode switch ACCEPTED")
            }
            Ok(ack) => tracing::warn!(active = ?ack.mode, "mode switch REJECTED"),
            other => tracing::error!(?other, "bad Reconfigured"),
        }
    });
}

/// `--rebitrate`: after a delay, send the SetBitrate the Automatic controller would and
/// await the host's BitrateChanged ack. Host-side this exercises the
/// in-place `reconfigure_bitrate` (no IDR) or the rebuild fallback — the host log says
/// which. The cursor wiggle keeps a damage-driven idle desktop publishing frames through
/// the whole window: the encode loop only drains bitrate requests between frames, and the
/// post-switch AUs are what prove the stream carried on.
fn spawn_rebitrate(conn: &Wire, mut rs: CtlTx, mut rr: CtlRx, new_kbps: u32, after_secs: u32) {
    let conn2 = conn.clone();
    tokio::spawn(async move {
        let wiggle = |i: u32| InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: if i % 2 == 0 { 2 } else { -2 },
            y: 0,
            flags: 0,
        };
        let end = std::time::Instant::now() + std::time::Duration::from_secs(after_secs as u64 + 6);
        let switch_at =
            std::time::Instant::now() + std::time::Duration::from_secs(after_secs as u64);
        let mut sent = false;
        let mut i = 0u32;
        while std::time::Instant::now() < end {
            let _ = conn2.send_datagram(wiggle(i).encode().to_vec());
            i += 1;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !sent && std::time::Instant::now() >= switch_at {
                sent = true;
                tracing::info!(new_kbps, "requesting mid-stream bitrate change");
                if v2io::send(
                    &mut rs,
                    &SetBitrate {
                        bitrate_kbps: new_kbps,
                    },
                )
                .await
                .is_err()
                {
                    tracing::error!("SetBitrate write failed");
                    return;
                }
                match next_of::<BitrateChanged>(&mut rr).await {
                    Ok(ack) => tracing::info!(
                        applied_kbps = ack.bitrate_kbps,
                        "BITRATE CHANGE acked by host"
                    ),
                    other => tracing::error!(?other, "bad BitrateChanged"),
                }
            }
        }
    });
}

/// `--speed-test`: once the stream warms up, ask the host to burst FLAG_PROBE filler, then
/// measure delivered WIRE packets (session-stat delta) against what the host reports putting
/// on the wire.
fn spawn_speed_test(
    conn: &Wire,
    mut ss: CtlTx,
    mut sr: CtlRx,
    (target_kbps, duration_ms): (u32, u32),
    counters: &Arc<Counters>,
) {
    let c = counters.clone();
    // Per-packet wire size to express delivered bytes as link bytes (header + shard + tag);
    // every shard is zero-padded to shard_payload so all data packets are this exact size.
    let crypto_overhead = punktfunk_core::crypto::TAG_LEN as u64;
    let conn2 = conn.clone();
    tokio::spawn(async move {
        use std::sync::atomic::Ordering::Relaxed;
        // Warm up the stream — and generate desktop activity while doing so. Damage-driven
        // capture paths (Windows IDD-push, a static headless desktop anywhere) publish NO
        // frame until something composes, and the host's pipeline build waits for a first
        // frame — so an idle virtual display would time the whole speed test out. A ±2 px
        // cursor wiggle over the wire is injected host-side into the right session/desktop.
        for i in 0..20u32 {
            let mv = InputEvent {
                kind: InputKind::MouseMove,
                _pad: [0; 3],
                code: 0,
                x: if i % 2 == 0 { 2 } else { -2 },
                y: 0,
                flags: 0,
            };
            let _ = conn2.send_datagram(mv.encode().to_vec());
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // Baseline the packet-level counters right before the burst (video is paused during it,
        // so the delta is pure probe traffic plus a sliver of resumed video in the settle).
        let base_pkts = c.rx_wire_packets.load(Relaxed);
        let base_bytes = c.rx_wire_bytes.load(Relaxed);
        tracing::info!(target_kbps, duration_ms, "requesting speed-test probe");
        let ask = punktfunk_core::quic::ProbeShaped::from(ProbeRequest {
            target_kbps,
            duration_ms,
        });
        if v2io::send(&mut ss, &ask).await.is_err() {
            tracing::error!("ProbeRequest write failed");
            return;
        }
        let res = match next_of::<ProbeResult>(&mut sr).await {
            Ok(r) => r,
            other => {
                tracing::error!(?other, "bad ProbeResult");
                return;
            }
        };
        // A declined burst comes back all-zero (duration_ms = 0) — e.g. the host predates
        // speed tests. Say so instead of dividing a settle-window sliver by 1 ms.
        if res.duration_ms == 0 {
            tracing::error!(
                "SPEED TEST declined by host (all-zero ProbeResult) — host too old, or it \
                 rejected the request; check the host log"
            );
            return;
        }
        // The reliable result can beat the last UDP shards — let the tail arrive before reading.
        // Keep this short: video resumes the instant the burst ends, so a long settle counts
        // resumed-video packets against the probe (inflating recv past the host's wire count).
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let recv_packets = c.rx_wire_packets.load(Relaxed).saturating_sub(base_pkts);
        // bytes_received counts plaintext (header + shard); add per-packet crypto back for the
        // true on-wire byte count.
        let recv_wire_bytes = c.rx_wire_bytes.load(Relaxed).saturating_sub(base_bytes)
            + recv_packets * crypto_overhead;
        // The host's burst duration is the rate denominator (it sent for this long).
        let window_ms = res.duration_ms.max(1) as u64;
        let throughput_kbps = recv_wire_bytes.saturating_mul(8) / window_ms;
        // Link loss: wire packets the host put out that didn't arrive. host_drop: wire packets
        // the host couldn't even hand to the kernel (send buffer too small / can't keep up).
        let link_loss = if res.wire_packets_sent > 0 {
            (res.wire_packets_sent as i64 - recv_packets as i64).max(0) as f64
                / res.wire_packets_sent as f64
                * 100.0
        } else {
            0.0
        };
        let offered_wire = res.wire_packets_sent + res.send_dropped;
        let host_drop = if offered_wire > 0 {
            res.send_dropped as f64 / offered_wire as f64 * 100.0
        } else {
            0.0
        };
        tracing::info!(
            target_kbps,
            target_mbps = target_kbps / 1000,
            delivered_mbps = throughput_kbps / 1000,
            link_loss_pct = format!("{link_loss:.1}%"),
            host_drop_pct = format!("{host_drop:.1}%"),
            wire_pkts_sent = res.wire_packets_sent,
            wire_pkts_recv = recv_packets,
            send_dropped = res.send_dropped,
            "SPEED TEST complete",
        );
    });
}

/// Capture-model cursor repro. `--cursor-capture`: flip the negotiated cursor channel to
/// "host composites" and drive RELATIVE pointer motion, exactly like a pointer-lock
/// client. `--cursor-nochannel`: the same motion with NO channel at all (a
/// capture-latched client) — the host must composite unprompted. Owns BOTH control
/// halves: the host may send CursorShape (0x50) messages while a negotiated channel is
/// still in its initial client-draws state, and dropping our recv half would fail those
/// writes host-side.
fn spawn_cursor_test(
    conn: &Wire,
    mut cs: CtlTx,
    mut cr: CtlRx,
    flip_channel: bool,
    flip_twice: bool,
) {
    tokio::spawn(async move {
        if flip_twice {
            for (at, client_draws) in [(15, false), (20, true)] {
                tokio::time::sleep(std::time::Duration::from_secs(at)).await;
                if let Err(e) = v2io::send(&mut cs, &CursorRenderMode { client_draws }).await {
                    tracing::error!(error = %format!("{e:#}"), "cursor-channel: render-mode write failed");
                    return;
                }
                tracing::info!(client_draws, "cursor-channel: render mode flipped");
            }
        }
        if flip_channel {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match v2io::send(
                &mut cs,
                &CursorRenderMode {
                    client_draws: false,
                },
            )
            .await
            {
                Ok(()) => tracing::info!(
                    "cursor-capture: CursorRenderMode {{ client_draws: false }} sent — the \
                     host must now composite the pointer into the video"
                ),
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "cursor-capture: render-mode write failed");
                    return;
                }
            }
        }
        // Drain whatever the host still sends on the control stream; a shape says what
        // size it came in.
        while let Ok((ty, b)) = cr.read_frame().await {
            match decode::<CursorShape>(ty, &b) {
                Ok(s) => tracing::info!(
                    serial = s.serial,
                    w = s.w,
                    h = s.h,
                    hot_x = s.hot_x,
                    hot_y = s.hot_y,
                    "cursor-channel: CursorShape received"
                ),
                Err(_) => {
                    tracing::debug!(len = b.len(), ty, "cursor-capture: control message drained")
                }
            }
        }
    });
    let wiggle_conn = conn.clone();
    tokio::spawn(async move {
        // Relative circles, forever: keeps the host pointer moving (and, on metadata-cursor
        // compositors, keeps cursor updates flowing) for the whole dump.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tracing::info!("cursor-capture: relative pointer wiggle running");
        let mut t = 0.0f64;
        loop {
            let e = InputEvent {
                kind: InputKind::MouseMove,
                _pad: [0; 3],
                code: 0,
                x: (10.0 * t.cos()) as i32,
                y: (10.0 * t.sin()) as i32,
                flags: 0,
            };
            let _ = wiggle_conn.send_datagram(e.encode().to_vec());
            t += 0.2;
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
}

/// Normal stream mode: relay the data loop's windowed loss estimate and what landed on the
/// wire to the host as numbered feedback windows, so it sizes FEC to the link and can tell a
/// dead data plane, and — like the real clients — ask for a keyframe whenever the
/// unrecoverable-frame count grows (100 ms poll = a natural throttle; several drops in a
/// burst coalesce into one ask). Each ask goes out twice: a datagram can be lost. The control
/// stream stays open and idle here (remode/speed-test own it in their modes).
fn spawn_loss_relay(conn: quinn::Connection, ls: CtlTx, counters: &Arc<Counters>, link_kbps: u32) {
    let c = counters.clone();
    tokio::spawn(async move {
        use std::sync::atomic::Ordering::Relaxed;
        let _control = ls; // a dropped send half would finish the stream and end the session
        let send = |fb: Feedback| conn.send_datagram(fb.encode().into()).is_ok();
        let mut last_report = std::time::Instant::now();
        let mut last_dropped = 0u64;
        let (mut loss_ppm, mut dead_ppm) = (0u32, 0u32);
        let (mut window, mut ask) = (0u32, 0u16);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let d = c.dropped_frames.load(Relaxed);
            if d > last_dropped {
                last_dropped = d;
                ask = ask.wrapping_add(1).max(1);
                let fb = Feedback {
                    ask,
                    keyframe: true,
                    link_kbps,
                    ..Default::default()
                };
                if !(send(fb) && send(fb)) {
                    break; // connection gone
                }
                tracing::debug!(dropped = d, "unrecoverable frame — requested keyframe");
            }
            if last_report.elapsed() >= std::time::Duration::from_millis(750) {
                last_report = std::time::Instant::now();
                // A window without a fresh loss sample repeats the last: what a dead data
                // plane looks like is the delivery count standing still, not a silent window.
                let v = c
                    .loss_ppm
                    .swap(u32::MAX, std::sync::atomic::Ordering::Acquire);
                if v != u32::MAX {
                    (loss_ppm, dead_ppm) = (v, c.dead_ppm.load(Relaxed));
                }
                window = window.wrapping_add(1).max(1);
                let fb = Feedback {
                    window,
                    loss_ppm,
                    dead_ppm,
                    packets_received: c.rx_wire_packets.load(Relaxed),
                    link_kbps,
                    ..Default::default()
                };
                if !send(fb) {
                    break; // connection gone
                }
            }
        }
    });
}

/// Input plane: scripted events as QUIC datagrams (mouse square + 'A' taps), proving the
/// low-latency input path without a real input device.
fn spawn_input_test(conn: &Wire, mode: Mode) {
    let conn2 = conn.clone();
    let (mw, mh) = (mode.width, mode.height);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tracing::info!("input-test: sending scripted datagrams for ~6s");
        for i in 0..160u32 {
            let (dx, dy) = match (i / 10) % 4 {
                0 => (12, 0),
                1 => (0, 12),
                2 => (-12, 0),
                _ => (0, -12),
            };
            let mv = InputEvent {
                kind: InputKind::MouseMove,
                _pad: [0; 3],
                code: 0,
                x: dx,
                y: dy,
                flags: 0,
            };
            let _ = conn2.send_datagram(mv.encode().to_vec());
            // Absolute motion too (the GTK client's path): a diagonal sweep, with the
            // coordinate-space size packed in `flags` — the contract injectors require.
            let abs = InputEvent {
                kind: InputKind::MouseMoveAbs,
                _pad: [0; 3],
                code: 0,
                x: ((i * mw) / 160) as i32,
                y: ((i * mh) / 160) as i32,
                flags: (mw << 16) | (mh & 0xffff),
            };
            let _ = conn2.send_datagram(abs.encode().to_vec());
            if i % 20 == 0 {
                for kind in [InputKind::KeyDown, InputKind::KeyUp] {
                    let key = InputEvent {
                        kind,
                        _pad: [0; 3],
                        code: 0x41, // VK 'A'
                        x: 0,
                        y: 0,
                        flags: 0,
                    };
                    let _ = conn2.send_datagram(key.encode().to_vec());
                }
                // Gamepad plane: tap A + sweep the left stick on pad 0 (the host
                // accumulates these into its virtual xpad; needs /dev/uinput access).
                use punktfunk_core::input::gamepad::{AXIS_LS_X, BTN_A};
                let pad_events = [
                    (InputKind::GamepadButton, BTN_A, 1),
                    (InputKind::GamepadButton, BTN_A, 0),
                    (
                        InputKind::GamepadAxis,
                        AXIS_LS_X,
                        ((i as i32) % 64 - 32) * 1024,
                    ),
                ];
                for (kind, code, x) in pad_events {
                    let ev = InputEvent {
                        kind,
                        _pad: [0; 3],
                        code,
                        x,
                        y: 0,
                        flags: 0, // pad index 0
                    };
                    let _ = conn2.send_datagram(ev.encode().to_vec());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        tracing::info!("input-test: done");
    });
}

/// Mic plane: stream a synthetic 440 Hz tone as the mic uplink (0xCB) — proves client→host
/// mic passthrough end to end without a real microphone (the host decodes it into its virtual
/// source; record that source to hear the tone). Two pacing modes:
///   default      — Opus 5 ms frames on a steady 5 ms tick (smooth arrival).
///   --mic-burst  — two 20 ms Opus frames back-to-back every 40 ms, replicating a real
///                  client's input-tap cadence (the Mac client's AVAudioEngine tap yields
///                  ~2048-frame buffers → two packets per ~42 ms). A host without a jitter
///                  buffer crackles on this pattern; a steady 5 ms stream never trips it.
///                  Record the host mic and count silence gaps to test host-side buffering.
fn spawn_mic_test(conn: &Wire, burst: bool) {
    let conn2 = conn.clone();
    tokio::spawn(async move {
        let mut enc =
            match opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "mic-test: opus encoder init failed");
                    return;
                }
            };
        let _ = enc.set_bitrate(opus::Bitrate::Bits(64_000));
        // Frame size + tick per pacing mode; `per_tick` packets are sent back-to-back.
        let (frame, tick_ms, per_tick) = if burst {
            (960usize, 40u64, 2u32) // 2× 20 ms every 40 ms — the bursty real-client shape
        } else {
            (240usize, 5u64, 1u32) // 5 ms frames on a smooth tick
        };
        tracing::info!(burst, "mic-test: streaming a 440 Hz tone as the mic uplink");
        let mut phase = 0.0f32;
        let step = 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
        let mut pcm = vec![0f32; frame * 2];
        let mut out = [0u8; 4000];
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
        let mut seq = 0u32;
        'stream: loop {
            interval.tick().await;
            for _ in 0..per_tick {
                for f in 0..frame {
                    let s = (phase.sin()) * 0.25;
                    phase += step;
                    if phase > std::f32::consts::PI * 2.0 {
                        phase -= std::f32::consts::PI * 2.0;
                    }
                    pcm[f * 2] = s;
                    pcm[f * 2 + 1] = s;
                }
                if let Ok(n) = enc.encode_float(&pcm, &mut out) {
                    let d = punktfunk_core::quic::encode_mic_datagram(
                        seq,
                        punktfunk_core::quic::wall_clock_ns(),
                        &out[..n],
                    );
                    if conn2.send_datagram(d).is_err() {
                        break 'stream;
                    }
                }
                seq = seq.wrapping_add(1);
            }
        }
        tracing::info!("mic-test: done");
    });
}

/// Touch plane: drag a synthetic finger (touch id 0) in a circle on the client surface, so
/// the host injects it via libei ei_touchscreen — proves the touch path end to end. `flags`
/// packs the surface w/h; x/y are pixels (the host maps them into the device region).
fn spawn_touch_test(conn: &Wire, mode: Mode) {
    let conn2 = conn.clone();
    let (w, h) = (mode.width, mode.height);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let flags = (w << 16) | (h & 0xffff);
        let (cx, cy, r) = (w as f32 / 2.0, h as f32 / 2.0, h as f32 / 4.0);
        let touch = |kind, x: f32, y: f32| InputEvent {
            kind,
            _pad: [0; 3],
            code: 0, // touch id 0
            x: x as i32,
            y: y as i32,
            flags,
        };
        tracing::info!("touch-test: dragging a finger in a circle for ~6s");
        for loop_i in 0..3u32 {
            let _ = conn2.send_datagram(touch(InputKind::TouchDown, cx + r, cy).encode().to_vec());
            for i in 0..60u32 {
                let a = std::f32::consts::TAU * i as f32 / 60.0;
                let mv = touch(InputKind::TouchMove, cx + r * a.cos(), cy + r * a.sin());
                let _ = conn2.send_datagram(mv.encode().to_vec());
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            let _ = conn2.send_datagram(touch(InputKind::TouchUp, cx + r, cy).encode().to_vec());
            let _ = loop_i;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::info!("touch-test: done");
    });
}

/// Rich-input plane: instantiate pad 0 on the host (a gamepad event creates the virtual
/// DualSense), then drive its touchpad (drag a finger across) + motion (gyro wobble) over the
/// 0xCC plane. Proves the rich client→host path; the 0xCD feedback is logged by the receive
/// loop below. Requires the host on the DualSense backend (`PUNKTFUNK_GAMEPAD=dualsense`).
/// Pad 0 as a player holds it: every stick and trigger moves each frame and A taps once a
/// second, as `GamepadState` snapshots. RT counts frames, so a reader can tell lost from late.
fn spawn_pad_soak(conn: &Wire, secs: u64, hz: u32) {
    let conn2 = conn.clone();
    tokio::spawn(async move {
        use punktfunk_core::input::{gamepad::BTN_A, GamepadSnapshot};
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tracing::info!(secs, hz, "pad-soak: driving pad 0");
        let mut tick =
            tokio::time::interval(std::time::Duration::from_micros(1_000_000 / u64::from(hz)));
        let t0 = std::time::Instant::now();
        let (mut seq, mut sent) = (0u8, 0u64);
        while t0.elapsed().as_secs() < secs {
            tick.tick().await;
            let t = t0.elapsed().as_secs_f64();
            let w = std::f64::consts::TAU * t;
            let stick = |x: f64| (30000.0 * x) as i16;
            seq = seq.wrapping_add(1);
            let snap = GamepadSnapshot {
                pad: 0,
                seq,
                buttons: if t % 1.0 < 0.1 { BTN_A } else { 0 },
                left_trigger: (127.0 + 127.0 * (w / 0.7).sin()) as u8,
                right_trigger: sent as u8,
                ls_x: stick((w / 2.0).sin()),
                ls_y: stick((w / 1.7).cos()),
                rs_x: stick((w / 1.3).sin()),
                rs_y: stick((w / 0.9).cos()),
            };
            let _ = conn2.send_datagram(snap.to_event().encode().to_vec());
            sent += 1;
            if sent % (u64::from(hz) * 10) == 0 {
                tracing::info!(sent, "pad-soak: snapshots sent");
            }
        }
        tracing::info!(sent, "pad-soak: done");
    });
}

fn spawn_rich_input_test(conn: &Wire) {
    let conn2 = conn.clone();
    tokio::spawn(async move {
        use punktfunk_core::input::gamepad::{AXIS_LS_X, MOTION_ACCEL_LSB_PER_G};
        use punktfunk_core::quic::RichInput;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // A neutral gamepad axis event makes the host create the virtual DualSense pad 0.
        let arrive = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: AXIS_LS_X,
            x: 0,
            y: 0,
            flags: 0,
        };
        let _ = conn2.send_datagram(arrive.encode().to_vec());
        tracing::info!(
            "rich-input-test: dragging the DualSense touchpad + wobbling motion for ~6s"
        );
        let touch = |active, x, y| RichInput::Touchpad {
            pad: 0,
            finger: 0,
            active,
            x,
            y,
        };
        for _ in 0..3u32 {
            let _ = conn2.send_datagram(touch(true, 0, 32768).encode());
            for i in 0..60u32 {
                let x = ((i * 65535) / 60) as u16;
                let _ = conn2.send_datagram(touch(true, x, 32768).encode());
                let g = (((i as i32 % 20) - 10) * 500) as i16; // gyro wobble, ±250 °/s
                let _ = conn2.send_datagram(
                    RichInput::Motion {
                        pad: 0,
                        gyro: [g, 0, 0],
                        // At rest, gravity is 1 g on +Z — in WIRE units, which are 10000 LSB/g
                        // and not the 8192 or 16384 a particular driver happens to use.
                        accel: [0, 0, MOTION_ACCEL_LSB_PER_G as i16],
                    }
                    .encode(),
                );
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            let _ = conn2.send_datagram(touch(false, 65535, 32768).encode());
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        tracing::info!("rich-input-test: done");
    });
}

/// Pad-audio plane: declare `pads` DualSenses with haptics and speaker. Each arrival repeats, as
/// a real client's does, and a neutral axis creates the pad.
fn spawn_pad_audio_test(conn: &Wire, pads: u8) {
    let conn2 = conn.clone();
    tokio::spawn(async move {
        use punktfunk_core::input::{encode_gamepad_arrival, gamepad::AXIS_LS_X};
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        for pad in 0..pads {
            let arrival = InputEvent {
                kind: InputKind::GamepadArrival,
                _pad: [0; 3],
                code: u32::from(GamepadPref::DualSense.to_u8()),
                x: 0,
                y: 0,
                flags: encode_gamepad_arrival(pad, 0b11),
            };
            for _ in 0..3 {
                let _ = conn2.send_datagram(arrival.encode().to_vec());
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            let axis = InputEvent {
                kind: InputKind::GamepadAxis,
                _pad: [0; 3],
                code: AXIS_LS_X,
                x: 0,
                y: 0,
                flags: u32::from(pad),
            };
            let _ = conn2.send_datagram(axis.encode().to_vec());
        }
        tracing::info!(
            pads,
            "pad-audio-test: declared DualSense pads with haptics and speaker"
        );
    });
}

/// Host→client datagrams: count Opus audio + rumble (playback is the platform clients'
/// job; here we verify the planes flow), decode the audio, and forward per-AU host timings
/// to the data plane.
/// One datagram off the side planes, by its kind byte. A PCM datagram is audio only on a
/// lossless session: the RESOLVED codec off the Welcome, never what was asked for, since a
/// probe that assumed its own request would mis-parse what it was actually sent.
enum SideDatagram<'a> {
    PadAudio(punktfunk_core::quic::PadAudioFrame),
    Pcm(&'a [u8]),
    Opus(&'a [u8]),
    Rumble(punktfunk_core::quic::RumbleUpdate),
    HdrMeta(punktfunk_core::quic::HdrMeta),
    Hid(punktfunk_core::quic::HidOutput),
    HostTiming(punktfunk_core::quic::HostTiming),
    Other,
}

fn classify_side_datagram(d: &[u8], lossless: bool) -> SideDatagram<'_> {
    use punktfunk_core::quic as q;
    if let Some(frame) = q::decode_pad_audio_datagram(d) {
        return SideDatagram::PadAudio(frame);
    }
    if lossless {
        if let Some((_, _, pcm)) = q::decode_audio_pcm_datagram(d) {
            return SideDatagram::Pcm(pcm);
        }
    }
    if let Some((_, _, opus)) = q::decode_audio_datagram(d) {
        SideDatagram::Opus(opus)
    } else if let Some(u) = q::decode_rumble_envelope(d) {
        SideDatagram::Rumble(u)
    } else if let Some(meta) = q::decode_hdr_meta_datagram(d) {
        SideDatagram::HdrMeta(meta)
    } else if let Some(hid) = q::HidOutput::decode(d) {
        SideDatagram::Hid(hid)
    } else if let Some(t) = q::decode_host_timing_datagram(d) {
        SideDatagram::HostTiming(t)
    } else {
        SideDatagram::Other
    }
}

/// The probe's audio decode on the host-RESOLVED layout, so it decodes the surround stream
/// it was sent (the headless validator for the encode path), written raw interleaved f32 LE
/// to `--audio-out` when asked: the rate and channel count are on the stats line and in the
/// log. The first decoded frame is logged so a loopback test can assert surround decoded.
struct AudioTap {
    channels: u8,
    rate_hz: u32,
    bits: u8,
    dec: Option<opus::MSDecoder>,
    pcm: Vec<f32>,
    lossless_pcm: Vec<f32>,
    logged: bool,
    out: Option<std::io::BufWriter<std::fs::File>>,
}

impl AudioTap {
    fn new(
        channels: u8,
        rate_hz: u32,
        bits: u8,
        lossless: bool,
        layout_wire: u8,
        out_path: Option<&str>,
    ) -> AudioTap {
        let audio_layout =
            punktfunk_core::audio::AudioLayout::from_wire(layout_wire).unwrap_or_default();
        let layout = punktfunk_core::audio::layout_for(channels, audio_layout);
        let dec = (!lossless)
            .then(|| {
                opus::MSDecoder::new(48_000, layout.streams, layout.coupled, layout.mapping).ok()
            })
            .flatten();
        let out = out_path.and_then(|p| {
            std::fs::File::create(p)
                .map(std::io::BufWriter::new)
                .map_err(|e| tracing::error!(path = p, error = %e, "--audio-out did not open"))
                .ok()
        });
        AudioTap {
            channels,
            rate_hz,
            bits,
            dec,
            pcm: vec![0f32; 5760 * channels as usize],
            lossless_pcm: Vec::new(),
            logged: false,
            out,
        }
    }

    /// A lossless PCM datagram (0xD3), at the Welcome's bit depth.
    fn pcm(&mut self, wire: &[u8]) {
        if punktfunk_core::audio::pcm::to_f32(wire, self.bits, &mut self.lossless_pcm).is_none() {
            return;
        }
        if !self.logged {
            self.logged = true;
            tracing::info!(
                channels = self.channels,
                rate_hz = self.rate_hz,
                bits = self.bits,
                samples_per_channel = self.lossless_pcm.len() / self.channels.max(1) as usize,
                "audio decoded (lossless PCM, 0xD3)"
            );
        }
        Self::write(&mut self.out, &self.lossless_pcm);
    }

    /// An Opus multistream datagram, decoded and validated: the per-channel sample count must
    /// be a legal Opus frame size.
    fn opus(&mut self, opus: &[u8]) {
        let Some(dec) = self.dec.as_mut() else {
            return;
        };
        match dec.decode_float(opus, &mut self.pcm, false) {
            Ok(samples) => {
                if !self.logged {
                    self.logged = true;
                    tracing::info!(
                        channels = self.channels,
                        samples_per_channel = samples,
                        "audio decoded (Opus multistream)"
                    );
                }
                Self::write(&mut self.out, &self.pcm[..samples * self.channels as usize]);
            }
            Err(e) => tracing::debug!(error = %e, "probe audio decode"),
        }
    }

    fn write(out: &mut Option<std::io::BufWriter<std::fs::File>>, samples: &[f32]) {
        if let Some(f) = out.as_mut() {
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for s in samples {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            let _ = std::io::Write::write_all(f, &bytes);
        }
    }
}

/// capture→received in µs: our receive instant in the host clock (`now_ns + offset_ns`)
/// minus the host's capture pts. None when the pts is not in our past or the two clocks are
/// ten seconds or more apart: the offset is 0 same-host or against an old host, and such a
/// sample is only noise.
fn capture_latency_us(now_ns: u64, offset_ns: i64, pts_ns: u64) -> Option<u64> {
    let lat = (now_ns as i128 + offset_ns as i128 - pts_ns as i128).max(0) as u64;
    (lat > 0 && lat < 10_000_000_000).then_some(lat / 1000)
}

/// When the receive loop ends: every expected frame checked, the peer closed and quiet for
/// 300 ms, the `--seconds` cap (120 s by default), or 45 s with no frame.
fn stream_over(
    expected: u32,
    checked: u32,
    closed: bool,
    since_rx: std::time::Duration,
    since_start: std::time::Duration,
    cap_secs: u64,
) -> bool {
    (expected > 0 && checked >= expected)
        || (closed && since_rx > std::time::Duration::from_millis(300))
        || since_start > std::time::Duration::from_secs(cap_secs)
        || since_rx > std::time::Duration::from_secs(45)
}

/// The host/network split from the per-AU 0xCF timings (design/stats-unification.md Phase
/// 2): host = the host's own capture→sent, network = capture→received minus it, tiled per
/// frame. Received AUs wait here for their timing, bounded, since an old host sends none.
/// The T0.1 stage tail (queue, encode, pace and the derived seal/xfer residual) stays empty
/// against a host that predates it.
#[derive(Default)]
struct LatencySplit {
    pending: std::collections::VecDeque<(u64, u64)>,
    host_us: Vec<u64>,
    net_us: Vec<u64>,
    queue_us: Vec<u64>,
    enc_us: Vec<u64>,
    xfer_us: Vec<u64>,
    pace_us: Vec<u64>,
}

impl LatencySplit {
    fn received(&mut self, pts_ns: u64, lat_us: u64) {
        self.pending.push_back((pts_ns, lat_us));
        if self.pending.len() > 1024 {
            self.pending.pop_front();
        }
    }

    fn host_timing(&mut self, t: punktfunk_core::quic::HostTiming) {
        let Some(i) = self.pending.iter().position(|(p, _)| *p == t.pts_ns) else {
            return;
        };
        let (_, hostnet_us) = self.pending.remove(i).unwrap();
        self.host_us.push(t.host_us as u64);
        self.net_us
            .push(hostnet_us.saturating_sub(t.host_us as u64));
        if let Some(s) = t.stages {
            self.queue_us.push(s.queue_us as u64);
            self.enc_us.push(s.encode_us as u64);
            self.pace_us.push(s.pace_us as u64);
            self.xfer_us.push(
                (t.host_us as u64)
                    .saturating_sub(s.queue_us as u64 + s.encode_us as u64 + s.pace_us as u64),
            );
        }
    }

    fn report(&mut self) {
        if self.host_us.is_empty() {
            tracing::info!("no host timing datagrams (0xCF) — old host; host+network unsplit");
            return;
        }
        tracing::info!(
            timing_samples = self.host_us.len(),
            host_p50_us = percentile(&mut self.host_us, 0.50),
            host_p95_us = percentile(&mut self.host_us, 0.95),
            net_p50_us = percentile(&mut self.net_us, 0.50),
            net_p95_us = percentile(&mut self.net_us, 0.95),
            "host/network latency split (host = capture→sent on the host; network = wire + \
             reassembly)"
        );
        if self.queue_us.is_empty() {
            return;
        }
        // The T0.1 per-stage host attribution: queue (capture→submit) → encode
        // (submit→bitstream) → xfer (seal/FEC + send-channel wait, derived residual)
        // → pace (the microburst spread). The four tile host_us per frame.
        tracing::info!(
            stage_samples = self.queue_us.len(),
            queue_p50_us = percentile(&mut self.queue_us, 0.50),
            queue_p95_us = percentile(&mut self.queue_us, 0.95),
            encode_p50_us = percentile(&mut self.enc_us, 0.50),
            encode_p95_us = percentile(&mut self.enc_us, 0.95),
            xfer_p50_us = percentile(&mut self.xfer_us, 0.50),
            xfer_p95_us = percentile(&mut self.xfer_us, 0.95),
            pace_p50_us = percentile(&mut self.pace_us, 0.50),
            pace_p95_us = percentile(&mut self.pace_us, 0.95),
            "host stage split (queue → encode → xfer → pace tile the host figure)"
        );
    }
}

/// Packet-level receive counters mirrored for the speed-test reporter (it reads their
/// delta), and the adaptive-FEC loss window: a fresh windowed estimate for the loss relay
/// every 750 ms.
struct StatsMirror {
    last_report: std::time::Instant,
    /// The counters the last loss window closed on.
    at: punktfunk_core::stats::Stats,
}

impl StatsMirror {
    fn publish(&mut self, session: &Session, counters: &Counters) {
        use std::sync::atomic::Ordering::Relaxed;
        let s = session.stats();
        counters.rx_wire_packets.store(s.packets_received, Relaxed);
        counters.rx_wire_bytes.store(s.bytes_received, Relaxed);
        counters.dropped_frames.store(s.frames_dropped, Relaxed);
        if self.last_report.elapsed() < std::time::Duration::from_millis(750) {
            return;
        }
        let at = std::mem::replace(&mut self.at, s);
        let (loss_ppm, dead_ppm) = window_report_ppm(
            s.fec_recovered_shards.wrapping_sub(at.fec_recovered_shards),
            s.fec_late_shards.wrapping_sub(at.fec_late_shards),
            s.partial_shards_lost.wrapping_sub(at.partial_shards_lost),
            s.dead_shards_lost.wrapping_sub(at.dead_shards_lost),
            s.packets_received.wrapping_sub(at.packets_received),
        );
        // Released by the `loss_ppm` store, which the relay acquires before it reads this.
        counters.dead_ppm.store(dead_ppm, Relaxed);
        counters
            .loss_ppm
            .store(loss_ppm, std::sync::atomic::Ordering::Release);
        self.last_report = std::time::Instant::now();
    }
}

/// The video AUs this run took: counted and, in verification mode, checked against the
/// deterministic test pattern, else written to `--out`.
struct VideoTally {
    ok: u32,
    mismatched: u32,
    bytes: u64,
    sink: Option<std::io::BufWriter<std::fs::File>>,
}

impl VideoTally {
    /// A frame under four bytes carries no index and checks as zeros.
    fn note(&mut self, data: &[u8], expected: u32) -> Result<()> {
        self.bytes += data.len() as u64;
        if expected == 0 {
            self.ok += 1;
            if let Some(s) = self.sink.as_mut() {
                s.write_all(data).context("write AU")?;
            }
            return Ok(());
        }
        let idx = data.first_chunk().map_or(0, |b| u32::from_le_bytes(*b));
        if data == test_frame(idx, data.len()) {
            self.ok += 1;
        } else {
            self.mismatched += 1;
        }
        Ok(())
    }

    fn checked(&self) -> u32 {
        self.ok + self.mismatched
    }
}

/// PUNKTFUNK_PERF: the cumulative receive-path stage split for the whole run — where the
/// receive core's time went (kernel drain vs AES-GCM open vs reassembly+FEC), the
/// measurement tool's view of the client-pump wall.
fn report_pump_perf(session: &mut Session) {
    let Some(p) = session.take_pump_perf() else {
        return;
    };
    let per_pkt = |ns: u64| ns.checked_div(p.packets).unwrap_or(0);
    tracing::info!(
        recv_ms = p.recv_ns / 1_000_000,
        decrypt_ms = p.decrypt_ns / 1_000_000,
        reasm_ms = p.reasm_ns / 1_000_000,
        packets = p.packets,
        pkts_per_batch = p.packets.checked_div(p.batches.max(1)).unwrap_or(0),
        decrypt_ns_pkt = per_pkt(p.decrypt_ns),
        reasm_ns_pkt = per_pkt(p.reasm_ns),
        "receive stage split (whole run, PUNKTFUNK_PERF)"
    );
}

/// The `p`-th percentile of `v`, which it sorts in place; 0 when empty.
fn percentile(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn spawn_datagram_rx(
    conn: &Wire,
    welcome: &Welcome,
    audio_out_path: Option<String>,
    counters: &Arc<Counters>,
    host_timing_tx: std::sync::mpsc::SyncSender<punktfunk_core::quic::HostTiming>,
) {
    let c = counters.clone();
    let ht_tx = host_timing_tx;
    let conn2 = conn.clone();
    let (channels, rate_hz, bits) = (
        welcome.audio_channels,
        welcome.audio_rate_hz,
        welcome.audio_bits,
    );
    let lossless = welcome.audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM;
    let layout_wire = welcome.audio_layout;
    tokio::spawn(async move {
        use std::sync::atomic::Ordering::Relaxed;
        let mut hdr_logged = false;
        let mut rumble_logged = false;
        let mut audio = AudioTap::new(
            channels,
            rate_hz,
            bits,
            lossless,
            layout_wire,
            audio_out_path.as_deref(),
        );
        while let Ok(d) = conn2.read_datagram().await {
            match classify_side_datagram(&d, lossless) {
                SideDatagram::PadAudio(frame) => {
                    if let Some(lane) = c
                        .pad_audio_pkts
                        .get(usize::from(frame.pad))
                        .and_then(|pad| pad.get(usize::from(frame.kind)))
                    {
                        lane.fetch_add(1, Relaxed);
                    }
                    if !frame.opus.is_empty() {
                        c.pad_audio_voiced.fetch_add(1, Relaxed);
                    }
                }
                SideDatagram::Pcm(wire) => {
                    c.audio_pkts.fetch_add(1, Relaxed);
                    c.audio_bytes.fetch_add(wire.len() as u64, Relaxed);
                    audio.pcm(wire);
                }
                SideDatagram::Opus(opus) => {
                    c.audio_pkts.fetch_add(1, Relaxed);
                    c.audio_bytes.fetch_add(opus.len() as u64, Relaxed);
                    audio.opus(opus);
                }
                SideDatagram::Rumble(u) => {
                    // Log the first rumble so a loopback test can see the self-terminating v2
                    // envelope tail (seq + TTL) arrived, not just the level. `lt`/`rt` are the v3
                    // impulse-trigger levels, printed beside the envelope: the wire-leg check for
                    // trigger rumble is "non-zero lt/rt AND the envelope still present".
                    if !rumble_logged {
                        rumble_logged = true;
                        tracing::info!(
                            pad = u.pad,
                            low = u.low,
                            high = u.high,
                            lt = u.left_trigger,
                            rt = u.right_trigger,
                            envelope = ?u.envelope,
                            "rumble (0xCA)"
                        );
                    }
                    // A v2 tail was present: the seq/ttl contract check for
                    // PUNKTFUNK_TEST_FEEDBACK, asserted at report time.
                    if u.envelope.is_some() {
                        c.saw_v2_rumble.store(true, Relaxed);
                    }
                    c.rumble_pkts.fetch_add(1, Relaxed);
                }
                SideDatagram::HdrMeta(meta) => {
                    // Logged once so a loopback test can assert the host sent it for an HDR session.
                    if !hdr_logged {
                        hdr_logged = true;
                        tracing::info!(?meta, "HDR static metadata (0xCE)");
                    }
                }
                SideDatagram::Hid(hid) => {
                    // The DualSense feedback plane (lightbar / player LEDs / adaptive triggers):
                    // the first few logged so a playtest can see them arrive without spam.
                    if c.hidout_pkts.fetch_add(1, Relaxed) < 12 {
                        tracing::info!(?hid, "DualSense HID output (0xCD)");
                    }
                }
                // Per-AU host timing (0xCF), to the stream loop for the host/network split.
                SideDatagram::HostTiming(t) => {
                    let _ = ht_tx.try_send(t);
                }
                SideDatagram::Other => {}
            }
        }
    });
}

/// Frames of the screens past the first, per stream: counted, written beside `--out`, and a
/// keyframe asked on the first frame after a gap.
#[derive(Default)]
struct ScreenTally {
    streams: std::collections::BTreeMap<u8, StreamTally>,
}

#[derive(Default)]
struct StreamTally {
    frames: u32,
    keyframes: u32,
    gaps: u32,
    last_index: Option<u32>,
    asks: u16,
    bytes: u64,
    sink: Option<std::io::BufWriter<std::fs::File>>,
}

impl ScreenTally {
    fn note(
        &mut self,
        frame: &punktfunk_core::session::Frame,
        out: Option<&str>,
        asks: Option<&quinn::Connection>,
    ) -> Result<()> {
        let t = self.streams.entry(frame.stream).or_default();
        if t.sink.is_none() {
            if let Some(path) = out {
                let path = format!("{path}.s{}", frame.stream);
                let file =
                    std::fs::File::create(&path).with_context(|| format!("create {path}"))?;
                t.sink = Some(std::io::BufWriter::new(file));
            }
        }
        let gap = t
            .last_index
            .is_some_and(|last| frame.frame_index.wrapping_sub(last) != 1);
        t.last_index = Some(frame.frame_index);
        let key = frame.flags & u32::from(punktfunk_core::packet::FLAG_SOF) != 0;
        if gap && !key {
            t.gaps += 1;
            t.asks = t.asks.wrapping_add(1).max(1);
            let fb = Feedback {
                stream: frame.stream,
                ask: t.asks,
                keyframe: true,
                ..Default::default()
            };
            if let Some(conn) = asks {
                let _ = conn.send_datagram(fb.encode().into());
            }
        }
        t.frames += 1;
        t.keyframes += u32::from(key);
        t.bytes += frame.data.len() as u64;
        if let Some(s) = t.sink.as_mut() {
            s.write_all(&frame.data).context("write screen AU")?;
        }
        Ok(())
    }

    fn report(&mut self) {
        for (stream, t) in &mut self.streams {
            if let Some(s) = t.sink.as_mut() {
                s.flush().ok();
            }
            tracing::info!(
                stream,
                frames = t.frames,
                keyframes = t.keyframes,
                gaps = t.gaps,
                mb = t.bytes / 1_000_000,
                "screen stream complete"
            );
        }
    }
}

/// The data plane, on a blocking thread (native threads only on the frame path): receives
/// each AU, verifies or dumps it, and reports capture→received latency and the host/network
/// split.
#[allow(clippy::too_many_arguments)]
fn data_plane(
    welcome: Welcome,
    (shared, media): (
        punktfunk_core::transport::shared::ClientMedia,
        punktfunk_core::session::MediaV2,
    ),
    out_path: Option<String>,
    seconds: Option<u64>,
    clock_offset_ns: Option<i64>,
    counters: Arc<Counters>,
    host_timing_rx: std::sync::mpsc::Receiver<punktfunk_core::quic::HostTiming>,
    // Some when the host took screens: further screens' keyframe asks leave here.
    screen_asks: Option<quinn::Connection>,
) -> Result<()> {
    let cfg = welcome.session_config(Role::Client);
    let expected = welcome.frames;
    // Our receive time in the host clock before differencing against the host-stamped capture
    // pts. 0 ⇒ same-host or an old host that didn't answer the skew handshake (the latency is
    // then only valid same-host, as before).
    let clock_offset = clock_offset_ns.unwrap_or(0);
    let skew_corrected = clock_offset_ns.is_some();
    let mut session =
        Session::new(cfg, media, Box::new(shared)).map_err(|e| anyhow!("client session: {e:?}"))?;
    session.set_screens(screen_asks.is_some());
    let mut screens = ScreenTally::default();
    let sink = match &out_path {
        Some(p) => Some(std::io::BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("create {p}"))?,
        )),
        None => None,
    };

    let mut video = VideoTally {
        ok: 0,
        mismatched: 0,
        bytes: 0,
        sink,
    };
    let mut latencies_us: Vec<u64> = Vec::new();
    let mut split = LatencySplit::default();
    let mut last_rx = std::time::Instant::now();
    let started = std::time::Instant::now();
    // Stream-duration cap: `--seconds N`, else the 120s default. Ending the loop here reaches the
    // graceful `conn.close` below (with the deliberate-quit code if `--quit`).
    let cap_secs = seconds.unwrap_or(120);
    let mut mirror = StatsMirror {
        last_report: started,
        at: Default::default(),
    };
    loop {
        mirror.publish(&session, &counters);
        if stream_over(
            expected,
            video.checked(),
            counters.closed.load(std::sync::atomic::Ordering::SeqCst),
            last_rx.elapsed(),
            started.elapsed(),
            cap_secs,
        ) {
            break;
        }
        let frame = match session.poll_frame() {
            Ok(frame) => frame,
            Err(PunktfunkError::NoFrame) => {
                std::thread::sleep(std::time::Duration::from_micros(300));
                continue;
            }
            Err(e) => return Err(anyhow!("poll_frame: {e:?}")),
        };
        last_rx = std::time::Instant::now();
        // Speed-test filler isn't video: it's measured via the packet-level counters
        // mirrored at the loop head — skip verification / the --out sink.
        if frame.flags & FLAG_PROBE as u32 != 0 {
            continue;
        }
        if frame.stream != punktfunk_core::packet::V2_STREAM_VIDEO {
            screens.note(&frame, out_path.as_deref(), screen_asks.as_ref())?;
            continue;
        }
        if let Some(lat) = capture_latency_us(
            punktfunk_core::quic::wall_clock_ns(),
            clock_offset,
            frame.pts_ns,
        ) {
            latencies_us.push(lat);
            split.received(frame.pts_ns, lat);
        }
        // Match any host timings (0xCF) that have arrived.
        while let Ok(t) = host_timing_rx.try_recv() {
            split.host_timing(t);
        }
        video.note(&frame.data, expected)?;
    }
    if let Some(mut s) = video.sink.take() {
        s.flush().ok();
    }
    screens.report();

    report_pump_perf(&mut session);

    tracing::info!(
        frames = video.ok,
        mismatched = video.mismatched,
        mb = video.bytes / 1_000_000,
        lat_p50_us = percentile(&mut latencies_us, 0.50),
        lat_p95_us = percentile(&mut latencies_us, 0.95),
        lat_p99_us = percentile(&mut latencies_us, 0.99),
        lat_max_us = latencies_us.last().copied().unwrap_or(0),
        skew_corrected,
        "stream complete (capture→received latency; skew_corrected=true ⇒ \
         cross-machine valid, false ⇒ same-host clock)"
    );
    split.report();
    if expected > 0 {
        anyhow::ensure!(
            video.mismatched == 0,
            "{} corrupted frames",
            video.mismatched
        );
        anyhow::ensure!(
            video.ok == expected,
            "received {}/{expected} frames",
            video.ok
        );
        tracing::info!("verification PASSED");
    } else {
        anyhow::ensure!(video.ok > 0, "no frames received");
    }
    Ok(())
}

/// Reports the side planes, whether or not the video plane succeeded.
fn report_side_planes(counters: &Counters) {
    use std::sync::atomic::Ordering::Relaxed;
    let (a, ab, r, h) = (
        counters.audio_pkts.load(Relaxed),
        counters.audio_bytes.load(Relaxed),
        counters.rumble_pkts.load(Relaxed),
        counters.hidout_pkts.load(Relaxed),
    );
    if a > 0 || r > 0 || h > 0 {
        tracing::info!(
            audio_pkts = a,
            audio_kb = ab / 1000,
            rumble_pkts = r,
            hidout_pkts = h,
            "host→client datagrams (Opus 48 kHz stereo, 5 ms frames; rumble; DualSense HID)"
        );
    }
    for (pad, [haptics, speaker]) in counters.pad_audio_pkts.iter().enumerate() {
        let (haptics, speaker) = (haptics.load(Relaxed), speaker.load(Relaxed));
        if haptics > 0 || speaker > 0 {
            tracing::info!(
                pad,
                haptics_pkts = haptics,
                speaker_pkts = speaker,
                "pad audio (0xD1)"
            );
        }
    }
    let voiced = counters.pad_audio_voiced.load(Relaxed);
    if voiced > 0 {
        tracing::info!(voiced_pkts = voiced, "pad audio frames carrying sound");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Args {
        parse_argv(
            std::iter::once("punktfunk-probe")
                .chain(flags.iter().copied())
                .map(String::from)
                .collect(),
        )
    }

    #[test]
    fn a_redirect_is_followed_once_pinned_to_the_seat() {
        let mut a = args(&["--connect", "192.168.1.213:9777", "--as", "ada"]);
        let to = punktfunk_core::quic::v2::msg::Redirect {
            port: 9779,
            pin: "ab".repeat(32),
            ..Default::default()
        };
        follow(&mut a, &to).unwrap();
        assert_eq!(a.connect, "192.168.1.213:9779");
        assert_eq!(a.pin, Some([0xab; 32]));
        assert!(
            follow(&mut a, &to).is_err(),
            "a second Redirect ends the run"
        );
    }

    #[test]
    fn an_empty_pin_keeps_the_one_dialed_with() {
        let pin = "cd".repeat(32);
        let mut a = args(&["--connect", "10.0.0.2:9777", "--pin", &pin]);
        let to = punktfunk_core::quic::v2::msg::Redirect {
            addr: "10.0.0.3".into(),
            port: 9780,
            ..Default::default()
        };
        follow(&mut a, &to).unwrap();
        assert_eq!(a.connect, "10.0.0.3:9780");
        assert_eq!(a.pin, Some([0xcd; 32]));
    }

    /// The side planes sort by kind byte, and a PCM datagram is audio only on a lossless session.
    #[test]
    fn side_datagrams_sort_by_kind_and_the_welcomes_codec() {
        use punktfunk_core::quic as q;
        let pcm = q::encode_audio_pcm_datagram(1, 2, &[0; 8]);
        assert!(matches!(classify_side_datagram(&pcm, true), SideDatagram::Pcm(b) if b.len() == 8));
        assert!(
            matches!(classify_side_datagram(&pcm, false), SideDatagram::Other),
            "PCM on an Opus session is not audio"
        );
        let opus = q::encode_audio_datagram(1, 2, &[1, 2, 3]);
        assert!(
            matches!(classify_side_datagram(&opus, true), SideDatagram::Opus(b) if b == [1, 2, 3])
        );
        let pad = q::encode_pad_audio_datagram(1, 0, 3, 4, &[9]);
        assert!(matches!(
            classify_side_datagram(&pad, false),
            SideDatagram::PadAudio(f) if f.pad == 1 && f.opus == [9]
        ));
        let rumble = q::encode_rumble_datagram_v2(0, 10, 20, 5, 100);
        assert!(matches!(
            classify_side_datagram(&rumble, false),
            SideDatagram::Rumble(u) if u.envelope.is_some()
        ));
        let t = q::HostTiming {
            pts_ns: 7,
            host_us: 300,
            stages: None,
        };
        assert!(matches!(
            classify_side_datagram(&q::encode_host_timing_datagram(&t), false),
            SideDatagram::HostTiming(got) if got.pts_ns == 7
        ));
        assert!(matches!(
            classify_side_datagram(&[], false),
            SideDatagram::Other
        ));
    }

    #[test]
    fn the_data_plane_decides_its_latency_sample_and_its_end() {
        assert_eq!(capture_latency_us(2_000_000, 0, 1_000_000), Some(1_000));
        assert_eq!(
            capture_latency_us(2_000_000, -1_500_000, 1_000_000),
            None,
            "a pts in our future"
        );
        assert_eq!(capture_latency_us(1_000_000, 500_000, 1_000_000), Some(500));
        assert_eq!(
            capture_latency_us(20_000_000_000, 0, 1),
            None,
            "ten seconds apart is noise"
        );
        let ms = std::time::Duration::from_millis;
        let s = std::time::Duration::from_secs;
        assert!(!stream_over(0, 0, false, ms(0), ms(0), 120));
        assert!(
            stream_over(10, 10, false, ms(0), ms(0), 120),
            "every expected frame checked"
        );
        assert!(!stream_over(10, 9, false, ms(0), ms(0), 120));
        assert!(
            stream_over(0, 0, true, ms(301), ms(0), 120),
            "closed and quiet"
        );
        assert!(!stream_over(0, 0, true, ms(299), ms(0), 120));
        assert!(stream_over(0, 0, false, ms(0), s(121), 120), "the cap");
        assert!(
            stream_over(0, 0, false, s(46), s(0), 120),
            "45 s with no frame"
        );
    }

    #[test]
    fn host_timings_split_the_latency_they_match() {
        use punktfunk_core::quic::{HostStages, HostTiming};
        let mut split = LatencySplit::default();
        split.received(10, 5_000);
        split.received(20, 6_000);
        split.host_timing(HostTiming {
            pts_ns: 20,
            host_us: 2_000,
            stages: None,
        });
        split.host_timing(HostTiming {
            pts_ns: 99,
            host_us: 1,
            stages: None,
        });
        split.host_timing(HostTiming {
            pts_ns: 10,
            host_us: 9_000,
            stages: Some(HostStages {
                queue_us: 1_000,
                encode_us: 2_000,
                pace_us: 500,
            }),
        });
        assert_eq!(split.host_us, [2_000, 9_000]);
        assert_eq!(split.net_us, [4_000, 0], "the network never goes negative");
        assert_eq!(
            (
                &split.queue_us[..],
                &split.enc_us[..],
                &split.pace_us[..],
                &split.xfer_us[..]
            ),
            (&[1_000][..], &[2_000][..], &[500][..], &[5_500][..])
        );
        assert!(split.pending.is_empty());
        for pts in 0..2000 {
            split.received(pts, 1);
        }
        assert_eq!(split.pending.len(), 1024, "bounded against an old host");
        let mut v = vec![5, 1, 4, 2, 3];
        assert_eq!(
            (
                percentile(&mut v, 0.5),
                percentile(&mut v, 0.95),
                percentile(&mut [], 0.5)
            ),
            (3, 5, 0)
        );
    }
}
